mod input;
mod rime;
mod rime_data;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
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
    match run() {
        Ok(status) => ExitCode::from(status.exit_code().min(u8::MAX as u32) as u8),
        Err(error) => {
            eprintln!("rime-fep: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let (config_dir, command) = arguments()?;

    if env::var_os(ACTIVE_ENV).is_some() {
        return Err("rime-fep is already running in this terminal session".into());
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("stdin and stdout must be terminals".into());
    }

    let mut input_method = InputMethod::new(config_dir.as_deref())?;
    let mut size = terminal_size()?;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(size)?;
    let mut child = pair.slave.spawn_command(command)?;

    drop(pair.slave);

    let result = run_proxy(pair.master, &mut child, &mut input_method, &mut size);

    if result.is_err() {
        let _ = child.kill();
    }
    result
}

fn run_proxy(
    master: Box<dyn MasterPty + Send>,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    input_method: &mut InputMethod,
    size: &mut PtySize,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let _raw_mode = RawMode::enable()?;
    let mut pty_reader = master.try_clone_reader()?;
    let mut pty_writer = Some(master.take_writer()?);
    let (sender, receiver) = mpsc::channel();

    spawn_reader(
        sender.clone(),
        move |buffer| pty_reader.read(buffer),
        Event::Output,
        Event::OutputClosed,
    );
    spawn_reader(
        sender.clone(),
        |buffer| io::stdin().read(buffer),
        Event::Input,
        Event::InputClosed,
    );
    spawn_resize_listener(sender)?;

    let mut stdout = io::stdout().lock();
    let mut parser = VtParser::new(size.rows, size.cols, 0);
    let mut popup = None;

    let result = proxy_loop(
        &receiver,
        master.as_ref(),
        child,
        &mut pty_writer,
        &mut stdout,
        size,
        input_method,
        &mut parser,
        &mut popup,
    );

    let cleanup = redraw_popup(&mut stdout, parser.screen(), &mut popup, None, *size);

    match result {
        Err(error) => Err(error),
        Ok(status) => {
            cleanup?;
            Ok(status)
        }
    }
}

fn arguments() -> Result<(Option<PathBuf>, CommandBuilder), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
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
    Ok((arguments.config_dir, command))
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
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let mut status = None;
    let mut output_closed = false;
    let mut popup_dirty = false;
    let mut last_draw = Instant::now();

    while status.is_none() || !output_closed {
        match receiver.recv_timeout(Duration::from_millis(16)) {
            Ok(Event::Input(bytes)) => {
                if let Some(writer) = pty_writer {
                    writer.write_all(&input_method.route(&bytes))?;
                    writer.flush()?;
                }
                let text = input_method.popup();
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
                parser.screen_mut().set_size(size.rows, size.cols);
                stdout.write_all(&parser.screen().state_formatted())?;
                stdout.flush()?;
                let text = input_method.popup();
                redraw_popup(stdout, parser.screen(), popup, text.as_deref(), *size)?;
                popup_dirty = false;
                last_draw = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if input_method.expire_notice() {
            let text = input_method.popup();
            redraw_popup(stdout, parser.screen(), popup, text.as_deref(), *size)?;
            popup_dirty = false;
            last_draw = Instant::now();
        }

        if popup_dirty && last_draw.elapsed() >= Duration::from_millis(16) {
            let text = input_method.popup();
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
        rows: size.rows,
        cols: size.columns,
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

    stdout.write_all(&screen.cursor_state_formatted())?;
    stdout.write_all(&screen.attributes_formatted())?;
    stdout.flush()
}

fn restore_popup(stdout: &mut impl Write, screen: &Screen, popup: Popup) -> io::Result<()> {
    let (_, columns) = screen.size();
    let Some(row) = screen
        .rows_formatted(0, columns)
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
    stdout.write_all(&row)
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
    fn places_popup_below_or_above_cursor() {
        let size = PtySize {
            rows: 3,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut parser = VtParser::new(size.rows, size.cols, 0);

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
    fn restores_content_covered_by_popup() {
        let size = PtySize {
            rows: 3,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut background = VtParser::new(size.rows, size.cols, 0);
        background.process(b"first\r\nsecond\r\nthird");
        let mut displayed = VtParser::new(size.rows, size.cols, 0);
        displayed.process(&background.screen().state_formatted());
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
