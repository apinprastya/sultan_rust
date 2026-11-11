//! Keyboard input, the PIN buffer and time texts (plan Phase 8: keyboard-first, as proven by
//! `sultan-spike`).
//!
//! The window has one focused `FocusScope` that hands every key press to Rust as Slint key text
//! plus modifiers; [`Key::parse`] turns it into a [`Key`], and the controller decides per screen
//! what it means. Digits and other printable characters ([`Key::Char`]) also feed the session
//! home's barcode field, which is drawn by Slint but edited in Rust (no text widget). Clicks on the PIN pad, keypad and key chips send the same keys or actions, so
//! mouse, touch and keyboard share one path.

use chrono::{Local, TimeZone};

/// A key the register reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `0`–`9` (top row or numeric keypad).
    Digit(u8),
    /// Backspace.
    Backspace,
    /// Enter / Return.
    Enter,
    /// Escape.
    Escape,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Tab.
    Tab,
    /// Shift+Tab.
    BackTab,
    /// F1–F12.
    F(u8),
    /// Another printable character (letters, `-`, `.`, space, `é`, …): what a barcode scanner
    /// or a typed SKU sends besides digits, and the text of the customer dialog's fields (spec
    /// 2.5 M5). The barcode field keeps only printable ASCII without space.
    Char(char),
    /// Anything else.
    Other,
}

// Slint key texts (`i-slint-common` key codes).
const BACKSPACE: char = '\u{8}';
const TAB: char = '\t';
const BACKTAB: char = '\u{19}';
const RETURN: char = '\n';
const ESCAPE: char = '\u{1b}';
const UP: char = '\u{F700}';
const DOWN: char = '\u{F701}';
const LEFT: char = '\u{F702}';
const RIGHT: char = '\u{F703}';
const F1: u32 = 0xF704;

impl Key {
    /// The key for a Slint key event text. Keys with Ctrl are never digits or editing keys, so a
    /// shortcut like Ctrl+1 does not type into the PIN.
    #[must_use]
    pub fn parse(text: &str, control: bool, shift: bool) -> Self {
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return Self::Other;
        };
        if control {
            return Self::Other;
        }
        match c {
            '0'..='9' => Self::Digit(u8::try_from(u32::from(c) - u32::from('0')).unwrap_or(0)),
            BACKSPACE => Self::Backspace,
            RETURN | '\r' => Self::Enter,
            ESCAPE => Self::Escape,
            UP => Self::Up,
            DOWN => Self::Down,
            LEFT => Self::Left,
            RIGHT => Self::Right,
            TAB if shift => Self::BackTab,
            TAB => Self::Tab,
            BACKTAB => Self::BackTab,
            c if (F1..F1 + 12).contains(&u32::from(c)) => {
                Self::F(u8::try_from(u32::from(c) - F1 + 1).unwrap_or(0))
            }
            c if c.is_ascii_graphic() || c == ' ' => Self::Char(c),
            // Other key codes of Slint (Home, Insert, …) are in the private-use area.
            c if !c.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&c) => Self::Char(c),
            _ => Self::Other,
        }
    }

    /// The label on a key chip, e.g. `F10`, `Enter`, `Esc`.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Digit(d) => d.to_string(),
            Self::Backspace => "⌫".to_owned(),
            Self::Enter => "Enter".to_owned(),
            Self::Escape => "Esc".to_owned(),
            Self::Up => "↑".to_owned(),
            Self::Down => "↓".to_owned(),
            Self::Left => "←".to_owned(),
            Self::Right => "→".to_owned(),
            Self::Tab => "Tab".to_owned(),
            Self::BackTab => "Shift+Tab".to_owned(),
            Self::F(n) => format!("F{n}"),
            Self::Char(c) => c.to_string(),
            Self::Other => String::new(),
        }
    }
}

/// Shortest PIN (spec 2.1 §4).
pub const PIN_MIN: usize = 4;
/// Longest PIN (spec 2.1 §4).
pub const PIN_MAX: usize = 6;

/// A PIN being typed. Only its length is ever shown (as dots).
#[derive(Default, Clone, PartialEq, Eq)]
pub struct Pin(String);

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pin({} digits)", self.0.len())
    }
}

impl Pin {
    /// Appends a digit unless the PIN is full. Returns whether it was added.
    pub fn push(&mut self, digit: u8) -> bool {
        if digit > 9 || self.0.len() >= PIN_MAX {
            return false;
        }
        self.0.push(char::from(b'0' + digit));
        true
    }

    /// Removes the last digit.
    pub fn pop(&mut self) -> bool {
        self.0.pop().is_some()
    }

    /// Forgets the PIN.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Number of digits typed.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing was typed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether it is long enough to send.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.0.len() >= PIN_MIN
    }

    /// Takes the PIN out for sending, leaving it empty.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

/// Remaining time as `m:ss`, rounded up to the second (`299_001` ms → `5:00`).
#[must_use]
pub fn remaining(ms: i64) -> String {
    let seconds = ms.max(0).saturating_add(999) / 1_000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Local time of day, `14:05`.
#[must_use]
pub fn time_of_day(unix_ms: i64) -> String {
    Local
        .timestamp_millis_opt(unix_ms)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Local date and time for the header, `08/11/2026 14:05`.
#[must_use]
pub fn date_time(unix_ms: i64) -> String {
    Local
        .timestamp_millis_opt(unix_ms)
        .single()
        .map(|t| t.format("%d/%m/%Y %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        #[rustfmt::skip]
        let cases = [
            ("7", false, false, Key::Digit(7)),
            ("0", false, true, Key::Digit(0)),
            ("7", true, false, Key::Other),
            ("\u{8}", false, false, Key::Backspace),
            ("\n", false, false, Key::Enter),
            ("\r", false, false, Key::Enter),
            ("\u{1b}", false, false, Key::Escape),
            ("\u{F700}", false, false, Key::Up),
            ("\u{F701}", false, false, Key::Down),
            ("\u{F702}", false, false, Key::Left),
            ("\u{F703}", false, false, Key::Right),
            ("\t", false, false, Key::Tab),
            ("\t", false, true, Key::BackTab),
            ("\u{19}", false, true, Key::BackTab),
            ("\u{F704}", false, false, Key::F(1)),
            ("\u{F70D}", false, false, Key::F(10)),
            ("\u{F70F}", false, false, Key::F(12)),
            ("\u{F710}", false, false, Key::Other),
            ("a", false, false, Key::Char('a')),
            ("A", false, true, Key::Char('A')),
            ("-", false, false, Key::Char('-')),
            ("a", true, false, Key::Other),
            (" ", false, false, Key::Char(' ')),
            ("é", false, false, Key::Char('é')),
            ("\u{F729}", false, false, Key::Other),
            ("\u{7f}", false, false, Key::Other),
            ("", false, false, Key::Other),
            ("12", false, false, Key::Other),
        ];
        for (text, control, shift, key) in cases {
            assert_eq!(Key::parse(text, control, shift), key, "{text:?}");
        }
    }

    #[test]
    fn key_labels() {
        #[rustfmt::skip]
        let cases = [
            (Key::Digit(3), "3"), (Key::Backspace, "⌫"), (Key::Enter, "Enter"),
            (Key::Escape, "Esc"), (Key::Up, "↑"), (Key::Down, "↓"), (Key::Left, "←"),
            (Key::Right, "→"), (Key::Tab, "Tab"), (Key::BackTab, "Shift+Tab"),
            (Key::F(10), "F10"), (Key::Char('x'), "x"), (Key::Other, ""),
        ];
        for (key, label) in cases {
            assert_eq!(key.label(), label);
        }
    }

    #[test]
    fn pin_buffer() {
        let mut pin = Pin::default();
        assert!(pin.is_empty() && !pin.pop());
        for d in [1, 2, 3] {
            assert!(pin.push(d));
        }
        assert!(!pin.is_complete());
        assert!(pin.push(4));
        assert!(pin.is_complete());
        assert!(pin.push(5) && pin.push(6));
        assert!(!pin.push(7), "at most 6 digits");
        assert!(!pin.push(10));
        assert_eq!(format!("{pin:?}"), "Pin(6 digits)", "never printed");
        assert!(pin.pop());
        assert_eq!(pin.take(), "12345");
        assert_eq!(pin.len(), 0);
        pin.push(1);
        pin.clear();
        assert!(pin.is_empty());
    }

    #[test]
    fn times() {
        let cases = [
            (300_000, "5:00"),
            (299_001, "5:00"),
            (61_000, "1:01"),
            (1, "0:01"),
            (0, "0:00"),
            (-5, "0:00"),
        ];
        for (ms, text) in cases {
            assert_eq!(remaining(ms), text, "{ms}");
        }
        assert_eq!(time_of_day(1_800_000_000_000).len(), 5);
        assert_eq!(date_time(1_800_000_000_000).len(), 16);
        assert_eq!(time_of_day(i64::MAX), "");
        assert_eq!(date_time(i64::MAX), "");
    }
}
