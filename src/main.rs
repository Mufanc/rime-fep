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
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType};
use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize};
use signal_hook::consts::signal::SIGWINCH;
use signal_hook::iterator::Signals;
use unicode_width::UnicodeWidthChar;

use self::input::InputMethod;

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Output(Vec<u8>),
    OutputClosed,
    Resize,
}

struct RawMode;

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

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("stdin and stdout must be terminals".into());
    }

    let mut input_method = InputMethod::new(config_dir.as_deref())?;
    let mut size = terminal_size()?;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system.openpty(child_size(size))?;
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

    prepare_screen(&mut stdout, *size, &input_method.status())?;

    let result = proxy_loop(
        &receiver,
        master.as_ref(),
        child,
        &mut pty_writer,
        &mut stdout,
        size,
        input_method,
    );

    let cleanup = restore_screen(&mut stdout, *size);

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
    command.args(command_parts);
    Ok((arguments.config_dir, command))
}

fn proxy_loop(
    receiver: &Receiver<Event>,
    master: &dyn MasterPty,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    pty_writer: &mut Option<Box<dyn Write + Send>>,
    stdout: &mut impl Write,
    size: &mut PtySize,
    input_method: &mut InputMethod,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let mut status = None;
    let mut output_closed = false;
    let mut status_dirty = false;
    let mut last_draw = Instant::now();

    while status.is_none() || !output_closed {
        match receiver.recv_timeout(Duration::from_millis(16)) {
            Ok(Event::Input(bytes)) => {
                if let Some(writer) = pty_writer {
                    writer.write_all(&input_method.route(&bytes))?;
                    writer.flush()?;
                }
                draw_status(stdout, *size, &input_method.status())?;
                status_dirty = false;
                last_draw = Instant::now();
            }
            Ok(Event::InputClosed) => {
                pty_writer.take();
            }
            Ok(Event::Output(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
                status_dirty = true;
            }
            Ok(Event::OutputClosed) => output_closed = true,
            Ok(Event::Resize) => {
                *size = terminal_size()?;
                master.resize(child_size(*size))?;
                draw_status(stdout, *size, &input_method.status())?;
                status_dirty = false;
                last_draw = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if status_dirty && last_draw.elapsed() >= Duration::from_millis(16) {
            draw_status(stdout, *size, &input_method.status())?;
            status_dirty = false;
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

fn child_size(mut size: PtySize) -> PtySize {
    let outer_rows = size.rows;

    size.rows = size.rows.saturating_sub(1).max(1);

    if outer_rows > 0 {
        size.pixel_height =
            (u32::from(size.pixel_height) * u32::from(size.rows) / u32::from(outer_rows)) as u16;
    }

    size
}

fn draw_status(stdout: &mut impl Write, size: PtySize, status: &str) -> io::Result<()> {
    if size.rows < 2 {
        return Ok(());
    }

    set_scroll_region(stdout, size)?;

    let status = fit_status(status, size.cols.saturating_sub(1));

    // Fixme: The terminal-global cursor save slot is also available to the child; track VT state instead.
    queue!(
        stdout,
        SavePosition,
        MoveTo(0, size.rows - 1),
        Clear(ClearType::CurrentLine),
        Print(status),
        RestorePosition
    )?;
    stdout.flush()
}

fn prepare_screen(stdout: &mut impl Write, size: PtySize, status: &str) -> io::Result<()> {
    if size.rows >= 2 {
        queue!(
            stdout,
            MoveTo(0, size.rows - 2),
            Clear(ClearType::CurrentLine)
        )?;
    }
    draw_status(stdout, size, status)
}

fn set_scroll_region(stdout: &mut impl Write, size: PtySize) -> io::Result<()> {
    if size.rows >= 2 {
        write!(stdout, "\x1b7\x1b[1;{}r\x1b8", size.rows - 1)?;
    }
    Ok(())
}

fn clear_status(stdout: &mut impl Write, size: PtySize) -> io::Result<()> {
    if size.rows < 2 {
        return Ok(());
    }

    queue!(
        stdout,
        SavePosition,
        MoveTo(0, size.rows - 1),
        Clear(ClearType::CurrentLine),
        RestorePosition
    )?;
    stdout.flush()
}

fn restore_screen(stdout: &mut impl Write, size: PtySize) -> io::Result<()> {
    clear_status(stdout, size)?;
    write!(stdout, "\x1b7\x1b[r\x1b8")?;
    stdout.flush()
}

fn fit_status(status: &str, columns: u16) -> String {
    let mut width = 0;
    status
        .chars()
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
    fn reserves_one_row_for_status() {
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
        };

        assert_eq!(child_size(size).rows, 23);
        assert_eq!(child_size(size).pixel_height, 460);
        assert_eq!(child_size(PtySize { rows: 1, ..size }).rows, 1);
    }

    #[test]
    fn clips_status_by_display_width() {
        assert_eq!(fit_status("[中] nihao", 8), "[中] nih");
    }

    #[test]
    fn reserves_status_row_in_terminal_scroll_region() {
        let mut output = Vec::new();
        set_scroll_region(
            &mut output,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .unwrap();

        assert_eq!(output, b"\x1b7\x1b[1;23r\x1b8");
    }
}
