use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::rime::Rime;

const TOGGLE: u8 = 0x1c;
const CONTROL_MASK: i32 = 1 << 2;
const BACKSPACE: i32 = 0xff08;
const RETURN: i32 = 0xff0d;
const ESCAPE: i32 = 0xff1b;
const F4: i32 = 0xffc1;
const SWITCH_NOTICE_DURATION: Duration = Duration::from_millis(800);

pub struct InputMethod {
    rime: Rime,
    enabled: bool,
    notice_until: Option<Instant>,
}

impl InputMethod {
    pub fn new(config_dir: Option<&Path>) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            rime: Rime::new(config_dir)?,
            enabled: false,
            notice_until: None,
        })
    }

    pub fn route(&mut self, input: &[u8]) -> Vec<u8> {
        self.route_rime(input)
    }

    pub fn popup(&self) -> Option<String> {
        if self.enabled {
            let status = self.rime.status();
            if status != "[中]" {
                return Some(status);
            }
        }

        self.notice_until
            .filter(|until| Instant::now() < *until)
            .map(|_| if self.enabled { "[中]" } else { "[En]" }.to_owned())
    }

    pub fn expire_notice(&mut self) -> bool {
        if self
            .notice_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.notice_until = None;
            true
        } else {
            false
        }
    }

    fn route_rime(&mut self, input: &[u8]) -> Vec<u8> {
        let sequence_key = escape_sequence_key(input);
        if sequence_key == Some((i32::from(b'\\'), CONTROL_MASK)) {
            self.toggle();
            return Vec::new();
        }

        if self.enabled
            && let Some((keycode, modifiers)) = sequence_key
        {
            self.notice_until = None;
            if self.rime.process(keycode, modifiers) {
                return self.rime.take_commit();
            }
        }

        // Fixme: Parse CSI sequences across read boundaries instead of assuming one read per sequence.
        if input.first() == Some(&0x1b) && input.len() > 1 {
            self.notice_until = None;
            return input.to_vec();
        }

        let mut output = Vec::new();

        for &byte in input {
            if byte == TOGGLE {
                self.toggle();
                continue;
            }

            self.notice_until = None;
            if !self.enabled {
                output.push(byte);
                continue;
            }

            match rime_key(byte) {
                Some((keycode, modifiers)) if self.rime.process(keycode, modifiers) => {}
                _ => output.push(byte),
            }
            output.extend(self.rime.take_commit());
        }
        output
    }

    fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.notice_until = Some(Instant::now() + SWITCH_NOTICE_DURATION);
        if !self.enabled {
            self.rime.clear();
        }
    }
}

fn rime_key(byte: u8) -> Option<(i32, i32)> {
    match byte {
        // Legacy terminals encode Ctrl+`, Ctrl+@, and Ctrl+Space as the same NUL byte.
        0x00 => Some((i32::from(b'`'), CONTROL_MASK)),
        0x08 | 0x7f => Some((BACKSPACE, 0)),
        b'\r' | b'\n' => Some((RETURN, 0)),
        0x1b => Some((ESCAPE, 0)),
        b' '..=b'~' => Some((i32::from(byte), 0)),
        _ => None,
    }
}

fn escape_sequence_key(input: &[u8]) -> Option<(i32, i32)> {
    if matches!(input, b"\x1bOS" | b"\x1b[14~") {
        return Some((F4, 0));
    }

    let body = input
        .strip_prefix(b"\x1b[")
        .and_then(|body| body.strip_suffix(b"u"))?;

    let mut fields = body.split(|byte| *byte == b';');
    let key = fields.next()?;
    let modifiers = fields
        .next()
        .and_then(|field| field.split(|byte| *byte == b':').next())
        .unwrap_or(b"1");

    if fields.next().is_some() {
        return None;
    }

    match (key, modifiers) {
        (b"27", b"1") => Some((ESCAPE, 0)),
        (b"92", b"5") => Some((i32::from(b'\\'), CONTROL_MASK)),
        (b"96", b"5") => Some((i32::from(b'`'), CONTROL_MASK)),
        (b"57367", b"1") => Some((F4, 0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_terminal_bytes_to_rime_keycodes() {
        assert_eq!(rime_key(0x00), Some((i32::from(b'`'), CONTROL_MASK)));
        assert_eq!(rime_key(0x7f), Some((BACKSPACE, 0)));
        assert_eq!(rime_key(b'\r'), Some((RETURN, 0)));
        assert_eq!(rime_key(b'a'), Some((i32::from(b'a'), 0)));
        assert_eq!(rime_key(0x03), None);
        assert_eq!(escape_sequence_key(b"\x1b[27u"), Some((ESCAPE, 0)));
        assert_eq!(
            escape_sequence_key(b"\x1b[92;5u"),
            Some((i32::from(b'\\'), CONTROL_MASK))
        );
        assert_eq!(escape_sequence_key(b"\x1b[96;5u"), Some((96, CONTROL_MASK)));
        assert_eq!(
            escape_sequence_key(b"\x1b[96;5:1u"),
            Some((96, CONTROL_MASK))
        );
        assert_eq!(escape_sequence_key(b"\x1b[57367u"), Some((F4, 0)));
        assert_eq!(escape_sequence_key(b"\x1bOS"), Some((F4, 0)));
        assert_eq!(escape_sequence_key(b"\x1b[14~"), Some((F4, 0)));
        assert_eq!(escape_sequence_key(b"\x1b[97;5u"), None);
    }
}
