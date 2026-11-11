mod input;
mod ipc;
mod rime;
mod rime_data;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType};
use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize};
use signal_hook::consts::signal::SIGWINCH;
use signal_hook::iterator::Signals;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vt100::{Parser as VtParser, Screen};

use self::input::InputMethod;

const ACTIVE_ENV: &str = "RIME_FEP_ACTIVE";

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Output(Vec<u8>),
    OutputClosed,
    Resize,
}

struct RawMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Popup {
    row: u16,
    col: u16,
}

#[derive(Parser)]
#[command(version, trailing_var_arg = true)]
struct Arguments {
    #[arg(long)]
    config_dir: Option<PathBuf>,
    #[arg(long, hide = true, value_name = "FINGERPRINT")]
    server: Option<String>,
    #[arg(allow_hyphen_values = true)]
    command: Vec<OsString>,
}

impl RawMode {
    fn enable() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> ExitCode {
    run().unwrap_or_else(|error| {
        eprintln!("rime-fep: {error}");
        ExitCode::FAILURE
    })
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    if let Some(fingerprint) = arguments.server.as_deref() {
        ipc::serve(arguments.config_dir.as_deref(), fingerprint)?;
        return Ok(ExitCode::SUCCESS);
    }
    let config_dir = arguments.config_dir.clone();
    let command = command(arguments)?;

    if env::var_os(ACTIVE_ENV).is_some() {
        return Err("rime-fep is already running in this terminal session".into());
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("stdin and stdout must be terminals".into());
    }

    let mut input_method = InputMethod::new(config_dir.as_deref())?;
    let status = run_proxy(command, &mut input_method)?;

    Ok(ExitCode::from(status.exit_code().min(u8::MAX as u32) as u8))
}

fn run_proxy(
    command: CommandBuilder,
    input_method: &mut InputMethod,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let _raw_mode = RawMode::enable()?;
    let (sender, receiver) = mpsc::channel();

    spawn_reader(
        sender.clone(),
        |buffer| io::stdin().read(buffer),
        Event::Input,
        Event::InputClosed,
    );

    let mut stdout = io::stdout().lock();
    let mut pending_input = Vec::new();

    // Synchronize before forwarding child output, which starts at the terminal's current cursor.
    stdout.write_all(b"\x1b[6n")?;
    stdout.flush()?;

    let (column, row) = initial_cursor_position(&receiver, &mut pending_input)?;

    spawn_resize_listener(sender.clone())?;

    // shortcut: a resize between CPR and sizing can shift the cursor; repeat calibration if observed.
    let mut size = terminal_size()?;
    let mut parser = VtParser::new(size.rows.try_into()?, size.cols.try_into()?, 0);
    let mut popup = None;

    queue!(parser, MoveTo(column, row))?;

    // Leave a blank row above the child so a bottom-edge popup can be restored.
    stdout.write_all(b"\r\n\r\n")?;
    stdout.flush()?;
    parser.process(b"\r\n\r\n");

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(size)?;
    let mut pty_reader = pair.master.try_clone_reader()?;
    let mut pty_writer = Some(pair.master.take_writer()?);
    let mut child = pair.slave.spawn_command(command)?;
    let master = pair.master;

    drop(pair.slave);

    spawn_reader(
        sender,
        move |buffer| pty_reader.read(buffer),
        Event::Output,
        Event::OutputClosed,
    );

    let result = proxy_loop(
        &receiver,
        master.as_ref(),
        &mut child,
        &mut pty_writer,
        &mut stdout,
        &mut size,
        input_method,
        &mut parser,
        &mut popup,
        pending_input,
    );

    let cleanup = redraw_popup(&mut stdout, parser.screen(), &mut popup, None, size);

    if result.is_err() {
        let _ = child.kill();
    }

    match result {
        Err(error) => Err(error),
        Ok(status) => {
            cleanup?;
            Ok(status)
        }
    }
}

fn initial_cursor_position(
    receiver: &Receiver<Event>,
    pending_input: &mut Vec<u8>,
) -> io::Result<(u16, u16)> {
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "terminal cursor query timed out")
            })?;
        let event = receiver.recv_timeout(timeout).map_err(|error| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("cannot query terminal cursor position: {error}"),
            )
        })?;
        let Event::Input(bytes) = event else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed"));
        };

        pending_input.extend_from_slice(&bytes);

        let report = pending_input
            .windows(2)
            .enumerate()
            .filter(|(_, bytes)| *bytes == b"\x1b[")
            .find_map(|(start, _)| {
                let bytes = &pending_input[start + 2..];
                // Two u16 coordinates, a separator, and R fit in 12 bytes.
                let length = bytes.iter().take(12).position(|&byte| byte == b'R')?;
                let (row, column) = str::from_utf8(&bytes[..length]).ok()?.split_once(';')?;
                let row = row.parse::<u16>().ok()?.checked_sub(1)?;
                let column = column.parse::<u16>().ok()?.checked_sub(1)?;

                Some((start, start + length + 3, column, row))
            });

        if let Some((start, end, column, row)) = report {
            pending_input.drain(start..end);
            return Ok((column, row));
        }
    }
}

fn command(arguments: Arguments) -> Result<CommandBuilder, Box<dyn std::error::Error>> {
    let mut command_parts = arguments.command;
    let program = if command_parts.is_empty() {
        env::var_os("SHELL").ok_or("SHELL is not set and no command was provided")?
    } else {
        command_parts.remove(0)
    };
    let mut command = CommandBuilder::new(program);

    command.cwd(env::current_dir()?);
    command.env(ACTIVE_ENV, "1");
    command.args(command_parts);

    Ok(command)
}

#[allow(clippy::too_many_arguments)]
fn proxy_loop(
    receiver: &Receiver<Event>,
    master: &dyn MasterPty,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    pty_writer: &mut Option<Box<dyn Write + Send>>,
    stdout: &mut impl Write,
    size: &mut PtySize,
    input_method: &mut InputMethod,
    parser: &mut VtParser,
    popup: &mut Option<Popup>,
    pending_input: Vec<u8>,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let mut initial_input = Some(pending_input);
    let mut status = None;
    let mut output_closed = false;
    let mut popup_dirty = false;
    let mut last_draw = Instant::now();

    while status.is_none() || !output_closed {
        let event = match initial_input.take() {
            Some(bytes) => Ok(Event::Input(bytes)),
            None => receiver.recv_timeout(Duration::from_millis(16)),
        };

        match event {
            Ok(Event::Input(bytes)) => {
                if let Some(writer) = pty_writer {
                    writer.write_all(&input_method.route(&bytes)?)?;
                    writer.flush()?;
                }
                let text = input_method.popup()?;

                redraw_popup(stdout, parser.screen(), popup, text.as_deref(), *size)?;
                popup_dirty = false;
                last_draw = Instant::now();
            }
            Ok(Event::InputClosed) => {
                pty_writer.take();
            }
            Ok(Event::Output(bytes)) => {
                if popup.is_some() {
                    redraw_popup(stdout, parser.screen(), popup, None, *size)?;
                }
                parser.process(&bytes);
                stdout.write_all(&bytes)?;
                stdout.flush()?;
                popup_dirty = true;
            }
            Ok(Event::OutputClosed) => output_closed = true,
            Ok(Event::Resize) => {
                popup.take();
                *size = terminal_size()?;
                master.resize(*size)?;
                parser.set_size(size.rows.try_into()?, size.cols.try_into()?);
                stdout.write_all(parser.screen().state_formatted().as_bytes())?;
                stdout.flush()?;

                let text = input_method.popup()?;

                redraw_popup(stdout, parser.screen(), popup, text.as_deref(), *size)?;
                popup_dirty = false;
                last_draw = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if input_method.expire_notice() {
            let text = input_method.popup()?;
            redraw_popup(stdout, parser.screen(), popup, text.as_deref(), *size)?;
            popup_dirty = false;
            last_draw = Instant::now();
        }

        if popup_dirty && last_draw.elapsed() >= Duration::from_millis(16) {
            let text = input_method.popup()?;
            redraw_popup(stdout, parser.screen(), popup, text.as_deref(), *size)?;
            popup_dirty = false;
            last_draw = Instant::now();
        }

        if status.is_none() {
            status = child.try_wait()?;
        }
    }

    Ok(status.unwrap_or_else(|| ExitStatus::with_exit_code(1)))
}

fn spawn_reader<F>(sender: Sender<Event>, mut read: F, data: fn(Vec<u8>) -> Event, closed: Event)
where
    F: FnMut(&mut [u8]) -> io::Result<usize> + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = vec![0; 8192];
        loop {
            match read(&mut buffer) {
                Ok(0) => break,
                Ok(length) => {
                    if sender.send(data(buffer[..length].to_vec())).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = sender.send(closed);
    });
}

fn spawn_resize_listener(sender: Sender<Event>) -> io::Result<()> {
    let mut signals = Signals::new([SIGWINCH])?;

    thread::spawn(move || {
        for _ in signals.forever() {
            if sender.send(Event::Resize).is_err() {
                break;
            }
        }
    });

    Ok(())
}

fn terminal_size() -> io::Result<PtySize> {
    let size = terminal::window_size()?;

    Ok(PtySize {
        rows: size.rows.max(1),
        cols: size.columns.max(1),
        pixel_width: size.width,
        pixel_height: size.height,
    })
}

fn redraw_popup(
    stdout: &mut impl Write,
    screen: &Screen,
    popup: &mut Option<Popup>,
    text: Option<&str>,
    size: PtySize,
) -> io::Result<()> {
    let old_popup = popup.take();
    let new_popup = text.and_then(|text| popup_content(screen, size, text));

    if old_popup.is_none() && new_popup.is_none() {
        return Ok(());
    }

    if let Some(old_popup) = old_popup {
        restore_popup(stdout, screen, old_popup)?;
    }

    if let Some((new_popup, text)) = new_popup {
        queue!(
            stdout,
            MoveTo(new_popup.col, new_popup.row),
            SetAttribute(Attribute::Reset),
            SetAttribute(Attribute::Reverse),
            Print(text),
            SetAttribute(Attribute::Reset)
        )?;
        *popup = Some(new_popup);
    }

    stdout.write_all(screen.cursor_state_formatted().as_bytes())?;
    stdout.write_all(screen.attributes_formatted().as_bytes())?;
    stdout.flush()
}

fn restore_popup(stdout: &mut impl Write, screen: &Screen, popup: Popup) -> io::Result<()> {
    let (_, columns) = screen.size();
    let Some(row) = screen
        .rows_formatted(0, columns.get())
        .nth(usize::from(popup.row))
    else {
        return Ok(());
    };

    queue!(
        stdout,
        MoveTo(0, popup.row),
        SetAttribute(Attribute::Reset),
        Clear(ClearType::CurrentLine)
    )?;
    stdout.write_all(row.as_bytes())
}

fn popup_content(screen: &Screen, size: PtySize, text: &str) -> Option<(Popup, String)> {
    if size.rows < 2 || size.cols == 0 {
        return None;
    }

    let text = fit_text(&format!(" {text} "), size.cols);
    let width = u16::try_from(UnicodeWidthStr::width(text.as_str())).ok()?;
    if width == 0 {
        return None;
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    let row = if cursor_row.saturating_add(1) < size.rows {
        cursor_row + 1
    } else {
        cursor_row.saturating_sub(1)
    };
    let col = cursor_col.min(size.cols.saturating_sub(width));

    Some((Popup { row, col }, text))
}

fn fit_text(text: &str, columns: u16) -> String {
    let mut width = 0;
    text.chars()
        .take_while(|character| {
            let character_width = UnicodeWidthChar::width(*character).unwrap_or(0);
            if width + character_width > usize::from(columns) {
                return false;
            }
            width += character_width;
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_initial_cursor_without_consuming_keyboard_input() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Event::Input(
                b"typed\x1b[A\x1b[0;1R\x1b[65536;1R\x1b[24;".to_vec(),
            ))
            .unwrap();
        sender.send(Event::Input(b"5Rafter".to_vec())).unwrap();
        let mut input = Vec::new();

        assert_eq!(
            initial_cursor_position(&receiver, &mut input).unwrap(),
            (4, 23)
        );
        assert_eq!(input, b"typed\x1b[A\x1b[0;1R\x1b[65536;1Rafter");
    }

    #[test]
    fn cursor_query_reports_closed_input() {
        let (sender, receiver) = mpsc::channel();
        sender.send(Event::InputClosed).unwrap();

        assert_eq!(
            initial_cursor_position(&receiver, &mut Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn places_popup_below_or_above_cursor() {
        let size = PtySize {
            rows: 3,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut parser = VtParser::new(
            size.rows.try_into().unwrap(),
            size.cols.try_into().unwrap(),
            0,
        );

        assert_eq!(
            popup_content(parser.screen(), size, "[中] a")
                .unwrap()
                .0
                .row,
            1
        );
        parser.process(b"\r\n\r\n");
        assert_eq!(
            popup_content(parser.screen(), size, "[中] a")
                .unwrap()
                .0
                .row,
            1
        );
    }

    #[test]
    fn clips_popup_by_display_width() {
        assert_eq!(fit_text(" [中] nihao ", 8), " [中] ni");
    }

    #[test]
    fn reserved_popup_row_preserves_existing_output() {
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let rows = size.rows.try_into().unwrap();
        let cols = size.cols.try_into().unwrap();
        let mut tracked = VtParser::new(rows, cols, 0);
        queue!(tracked, MoveTo(0, 23)).unwrap();
        let mut displayed = VtParser::new(rows, cols, 0);
        displayed.process(b"\x1b[23;1HPREVIOUS OUTPUT\x1b[24;1H");
        tracked.process(b"\r\n\r\nprompt> ");
        displayed.process(b"\r\n\r\nprompt> ");
        let before = displayed.screen().contents();
        let mut popup = None;
        let mut output = Vec::new();

        assert!(before.contains("PREVIOUS OUTPUT"));
        redraw_popup(
            &mut output,
            tracked.screen(),
            &mut popup,
            Some("[中] nihao"),
            size,
        )
        .unwrap();
        displayed.process(&output);
        output.clear();
        redraw_popup(&mut output, tracked.screen(), &mut popup, None, size).unwrap();
        displayed.process(&output);

        assert_eq!(displayed.screen().contents(), before);
        assert_eq!(
            displayed.screen().cursor_position(),
            tracked.screen().cursor_position()
        );
    }

    #[test]
    fn restores_content_covered_by_popup() {
        let size = PtySize {
            rows: 3,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let rows = size.rows.try_into().unwrap();
        let cols = size.cols.try_into().unwrap();
        let mut background = VtParser::new(rows, cols, 0);
        background.process(b"first\r\nsecond\r\nthird");
        let mut displayed = VtParser::new(rows, cols, 0);
        displayed.process(background.screen().state_formatted().as_bytes());
        let mut popup = None;
        let mut output = Vec::new();

        redraw_popup(
            &mut output,
            background.screen(),
            &mut popup,
            Some("[中] nihao"),
            size,
        )
        .unwrap();
        displayed.process(&output);
        assert_ne!(
            displayed.screen().contents(),
            background.screen().contents()
        );

        output.clear();
        redraw_popup(&mut output, background.screen(), &mut popup, None, size).unwrap();
        displayed.process(&output);

        let child_output = b"\x1b[3;1H\r\nupdated";
        background.process(child_output);
        displayed.process(child_output);
        output.clear();
        redraw_popup(
            &mut output,
            background.screen(),
            &mut popup,
            Some("[中] hao"),
            size,
        )
        .unwrap();
        displayed.process(&output);

        output.clear();
        redraw_popup(&mut output, background.screen(), &mut popup, None, size).unwrap();
        displayed.process(&output);

        assert_eq!(
            displayed.screen().contents(),
            background.screen().contents()
        );
    }
}
