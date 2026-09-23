use std::error::Error;
use std::fs::{self, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use crate::rime::{RimeEngine, SessionId};
use crate::{rime, rime_data};

const OP_HELLO: u8 = 0;
const OP_PROCESS: u8 = 1;
const OP_STATUS: u8 = 2;
const OP_CLEAR: u8 = 3;
const MAX_FRAME_SIZE: usize = 64 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(120);
const IDLE_TIMEOUT: Duration = Duration::from_millis(500);

pub struct RimeClient {
    stream: UnixStream,
    commit: Vec<u8>,
}

impl RimeClient {
    pub fn connect(config_dir: Option<&Path>) -> Result<Self, Box<dyn Error>> {
        let fingerprint = rime_data::fingerprint(config_dir, &rime::version()?)?;
        let socket = socket_path(&fingerprint)?;
        let deadline = Instant::now() + START_TIMEOUT;
        let mut last_start = None;

        loop {
            match Self::connect_once(&socket, &fingerprint) {
                Ok(client) => return Ok(client),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    return Err(error.into());
                }
                Err(error) if Instant::now() >= deadline => return Err(error.into()),
                Err(_) => {
                    if last_start.is_none_or(|started: Instant| started.elapsed() >= IDLE_TIMEOUT) {
                        spawn_server(config_dir, &fingerprint)?;
                        last_start = Some(Instant::now());
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }

    fn connect_once(socket: &Path, fingerprint: &str) -> io::Result<Self> {
        let mut stream = UnixStream::connect(socket)?;
        let mut hello = Vec::with_capacity(fingerprint.len() + 1);

        hello.push(OP_HELLO);
        hello.extend_from_slice(fingerprint.as_bytes());
        request(&mut stream, &hello)?;

        Ok(Self {
            stream,
            commit: Vec::new(),
        })
    }

    pub fn process(&mut self, keycode: i32, modifiers: i32) -> io::Result<bool> {
        let mut payload = Vec::with_capacity(9);

        payload.push(OP_PROCESS);
        payload.extend_from_slice(&keycode.to_le_bytes());
        payload.extend_from_slice(&modifiers.to_le_bytes());

        let response = request(&mut self.stream, &payload)?;
        let Some((&consumed, commit)) = response.split_first() else {
            return Err(invalid_data("empty process response"));
        };

        self.commit = commit.to_vec();
        Ok(consumed != 0)
    }

    pub fn take_commit(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.commit)
    }

    pub fn status(&mut self) -> io::Result<String> {
        String::from_utf8(request(&mut self.stream, &[OP_STATUS])?)
            .map_err(|_| invalid_data("server returned invalid UTF-8 status"))
    }

    pub fn clear(&mut self) -> io::Result<()> {
        request(&mut self.stream, &[OP_CLEAR])?;
        self.commit.clear();
        Ok(())
    }
}

pub fn serve(config_dir: Option<&Path>, fingerprint: &str) -> Result<(), Box<dyn Error>> {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid configuration fingerprint".into());
    }

    let runtime_dir = rime_data::runtime_dir()?;

    fs::create_dir_all(&runtime_dir)?;

    let short = &fingerprint[..16];
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(runtime_dir.join(format!("{short}.lock")))?;

    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    let socket = socket_path(fingerprint)?;
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    let _socket = SocketGuard(socket);
    let mut engine = RimeEngine::new(config_dir, fingerprint)?;
    let (sender, receiver) = mpsc::channel();

    spawn_acceptor(listener, sender.clone());
    run_engine(&mut engine, &receiver, fingerprint)
}

enum EngineCommand {
    Create {
        fingerprint: String,
        reply: SyncSender<Result<SessionId, String>>,
    },
    Process {
        session: SessionId,
        keycode: i32,
        modifiers: i32,
        reply: SyncSender<(bool, Vec<u8>)>,
    },
    Status {
        session: SessionId,
        reply: SyncSender<String>,
    },
    Clear {
        session: SessionId,
        reply: SyncSender<()>,
    },
    Destroy(SessionId),
}

fn run_engine(
    engine: &mut RimeEngine,
    receiver: &Receiver<EngineCommand>,
    fingerprint: &str,
) -> Result<(), Box<dyn Error>> {
    let mut sessions = 0usize;
    let mut connected = false;

    loop {
        let timeout = if connected && sessions == 0 {
            IDLE_TIMEOUT
        } else {
            START_TIMEOUT
        };
        let command = match receiver.recv_timeout(timeout) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) if sessions == 0 => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };

        match command {
            EngineCommand::Create {
                fingerprint: requested,
                reply,
            } => {
                let result = if requested != fingerprint {
                    Err("configuration fingerprint does not match server".to_owned())
                } else {
                    engine
                        .create_session()
                        .ok_or_else(|| "librime could not create a session".to_owned())
                };
                if result.is_ok() {
                    sessions += 1;
                    connected = true;
                }
                let _ = reply.send(result);
            }
            EngineCommand::Process {
                session,
                keycode,
                modifiers,
                reply,
            } => {
                let consumed = engine.process(session, keycode, modifiers);
                let _ = reply.send((consumed, engine.take_commit(session)));
            }
            EngineCommand::Status { session, reply } => {
                let _ = reply.send(engine.status(session));
            }
            EngineCommand::Clear { session, reply } => {
                engine.clear(session);
                let _ = reply.send(());
            }
            EngineCommand::Destroy(session) => {
                engine.destroy_session(session);
                sessions = sessions.saturating_sub(1);
            }
        }
    }
}

fn spawn_acceptor(listener: UnixListener, sender: mpsc::Sender<EngineCommand>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let sender = sender.clone();
            thread::spawn(move || handle_connection(stream, sender));
        }
    });
}

fn handle_connection(mut stream: UnixStream, sender: mpsc::Sender<EngineCommand>) {
    let Ok(hello) = read_frame(&mut stream) else {
        return;
    };
    let Some(fingerprint) = hello.strip_prefix(&[OP_HELLO]) else {
        let _ = write_error(&mut stream, "expected handshake");
        return;
    };
    let Ok(fingerprint) = String::from_utf8(fingerprint.to_vec()) else {
        let _ = write_error(&mut stream, "invalid handshake fingerprint");
        return;
    };
    let (reply, response) = mpsc::sync_channel(0);
    if sender
        .send(EngineCommand::Create { fingerprint, reply })
        .is_err()
    {
        return;
    }
    let Ok(result) = response.recv() else { return };
    let session = match result {
        Ok(session) => session,
        Err(error) => {
            let _ = write_error(&mut stream, &error);
            return;
        }
    };
    if write_success(&mut stream, &[]).is_ok() {
        while let Ok(payload) = read_frame(&mut stream) {
            if handle_request(&mut stream, &sender, session, &payload).is_err() {
                break;
            }
        }
    }
    let _ = sender.send(EngineCommand::Destroy(session));
}

fn handle_request(
    stream: &mut UnixStream,
    sender: &mpsc::Sender<EngineCommand>,
    session: SessionId,
    payload: &[u8],
) -> io::Result<()> {
    match payload {
        [OP_PROCESS, rest @ ..] if rest.len() == 8 => {
            let keycode = i32::from_le_bytes(rest[..4].try_into().unwrap());
            let modifiers = i32::from_le_bytes(rest[4..].try_into().unwrap());
            let (reply, response) = mpsc::sync_channel(0);
            sender
                .send(EngineCommand::Process {
                    session,
                    keycode,
                    modifiers,
                    reply,
                })
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
            let (consumed, commit) = response.recv().map_err(|_| io::ErrorKind::BrokenPipe)?;
            let mut payload = Vec::with_capacity(commit.len() + 1);
            payload.push(u8::from(consumed));
            payload.extend_from_slice(&commit);
            write_success(stream, &payload)
        }
        [OP_STATUS] => {
            let (reply, response) = mpsc::sync_channel(0);
            sender
                .send(EngineCommand::Status { session, reply })
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
            let status = response.recv().map_err(|_| io::ErrorKind::BrokenPipe)?;
            write_success(stream, status.as_bytes())
        }
        [OP_CLEAR] => {
            let (reply, response) = mpsc::sync_channel(0);
            sender
                .send(EngineCommand::Clear { session, reply })
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
            response.recv().map_err(|_| io::ErrorKind::BrokenPipe)?;
            write_success(stream, &[])
        }
        _ => write_error(stream, "invalid request"),
    }
}

fn spawn_server(config_dir: Option<&Path>, fingerprint: &str) -> io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);

    command.arg("--server").arg(fingerprint);

    if let Some(config_dir) = config_dir {
        command.arg("--config-dir").arg(config_dir);
    }

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;

    thread::spawn(move || {
        let _ = child.wait();
    });

    Ok(())
}

fn socket_path(fingerprint: &str) -> Result<PathBuf, Box<dyn Error>> {
    let short = fingerprint
        .get(..16)
        .ok_or("invalid configuration fingerprint")?;

    Ok(rime_data::runtime_dir()?.join(format!("{short}.sock")))
}

fn request(stream: &mut UnixStream, payload: &[u8]) -> io::Result<Vec<u8>> {
    write_frame(stream, payload)?;
    let response = read_frame(stream)?;
    match response.split_first() {
        Some((&0, payload)) => Ok(payload.to_vec()),
        Some((&1, message)) => Err(invalid_data(String::from_utf8_lossy(message))),
        _ => Err(invalid_data("invalid server response")),
    }
}

fn write_success(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let mut response = Vec::with_capacity(payload.len() + 1);
    response.push(0);
    response.extend_from_slice(payload);
    write_frame(stream, &response)
}

fn write_error(stream: &mut UnixStream, message: &str) -> io::Result<()> {
    let mut response = Vec::with_capacity(message.len() + 1);
    response.push(1);
    response.extend_from_slice(message.as_bytes());
    write_frame(stream, &response)
}

fn write_frame(stream: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| invalid_data("frame is too large"))?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(invalid_data("frame is too large"));
    }
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_frame(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(invalid_data("frame is too large"));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, b"hello").unwrap();
        assert_eq!(read_frame(&mut bytes.as_slice()).unwrap(), b"hello");
    }
}
