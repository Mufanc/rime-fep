use std::error::Error;
use std::path::Path;

use crate::rime::Rime;

const TOGGLE: u8 = 0x1c;
const CONTROL_MASK: i32 = 1 << 2;
const BACKSPACE: i32 = 0xff08;
const RETURN: i32 = 0xff0d;
const ESCAPE: i32 = 0xff1b;

pub struct InputMethod {
    rime: Rime,
    enabled: bool,
}

impl InputMethod {
    pub fn new(config_dir: Option<&Path>) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            rime: Rime::new(config_dir)?,
            enabled: false,
        })
    }

    pub fn route(&mut self, input: &[u8]) -> Vec<u8> {
        self.route_rime(input)
    }

    pub fn status(&self) -> String {
        if self.enabled {
            self.rime.status()
        } else {
            "[En] rime-fep".to_owned()
        }
    }

    fn route_rime(&mut self, input: &[u8]) -> Vec<u8> {
        if self.enabled
            && is_kitty_control_grave(input)
            && self.rime.process(i32::from(b'`'), CONTROL_MASK)
        {
            return self.rime.take_commit();
        }

        // Fixme: Parse CSI sequences across read boundaries instead of assuming one read per sequence.
        if input.first() == Some(&0x1b) && input.len() > 1 {
            return input.to_vec();
        }

        let mut output = Vec::new();

        for &byte in input {
            if byte == TOGGLE {
                self.enabled = !self.enabled;
                if !self.enabled {
                    self.rime.clear();
                }
                continue;
            }

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

fn is_kitty_control_grave(input: &[u8]) -> bool {
    let Some(body) = input
        .strip_prefix(b"\x1b[")
        .and_then(|body| body.strip_suffix(b"u"))
    else {
        return false;
    };

    let mut fields = body.split(|byte| *byte == b';');

    fields.next() == Some(b"96")
        && fields
            .next()
            .and_then(|field| field.split(|byte| *byte == b':').next())
            == Some(b"5")
        && fields.next().is_none()
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
        assert!(is_kitty_control_grave(b"\x1b[96;5u"));
        assert!(is_kitty_control_grave(b"\x1b[96;5:1u"));
        assert!(!is_kitty_control_grave(b"\x1b[97;5u"));
    }
}
