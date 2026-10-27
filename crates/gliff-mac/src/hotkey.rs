//! The release hotkey: while the pointer is over the picture every key goes
//! to the remote, Command shortcuts included; this one key (or chord) hands
//! the keyboard back to macOS without moving the pointer. Same forms as the
//! GTK client's `--release-hotkey`: `shift+escape`, `ctrl+alt+q`,
//! `double-escape`, `none`.

use std::time::{Duration, Instant};

use crate::keys::flag;

//= Version of the hotkey grammar; bump when forms are added or changed.
pub const HOTKEY_VERSION: &str = "0.1.0";

/// How long a double-tap may take.
pub const DOUBLE_TAP: Duration = Duration::from_millis(400);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReleaseHotkey {
    None,
    /// `mods` is a set of [`flag`] bits, compared exactly.
    Chord {
        key: u16,
        mods: u64,
        spec: String,
    },
    DoubleTap {
        key: u16,
        spec: String,
    },
}

/// macOS virtual key code for a key name used in hotkey specs.
fn key_code(name: &str) -> Option<u16> {
    let name = name.to_ascii_lowercase();
    let letters = "asdfhgzxcv_bqweryt";
    //= Letters live at irregular codes; look them up by position in the
    //= kVK order (index == code) for the first row, then the rest by hand.
    if name.len() == 1 {
        let c = name.chars().next()?;
        if let Some(i) = letters.find(c) {
            if c != '_' {
                return Some(i as u16);
            }
        }
        return match c {
            'o' => Some(0x1F),
            'u' => Some(0x20),
            'i' => Some(0x22),
            'p' => Some(0x23),
            'l' => Some(0x25),
            'j' => Some(0x26),
            'k' => Some(0x28),
            'n' => Some(0x2D),
            'm' => Some(0x2E),
            '1' => Some(0x12),
            '2' => Some(0x13),
            '3' => Some(0x14),
            '4' => Some(0x15),
            '5' => Some(0x17),
            '6' => Some(0x16),
            '7' => Some(0x1A),
            '8' => Some(0x1C),
            '9' => Some(0x19),
            '0' => Some(0x1D),
            _ => None,
        };
    }
    Some(match name.as_str() {
        "escape" | "esc" => 0x35,
        "tab" => 0x30,
        "space" => 0x31,
        "return" | "enter" => 0x24,
        "backspace" | "delete" => 0x33,
        "grave" | "backtick" => 0x32,
        "f1" => 0x7A,
        "f2" => 0x78,
        "f3" => 0x63,
        "f4" => 0x76,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f8" => 0x64,
        "f9" => 0x65,
        "f10" => 0x6D,
        "f11" => 0x67,
        "f12" => 0x6F,
        _ => return None,
    })
}

fn modifier_bit(name: &str) -> Option<u64> {
    Some(match name.to_ascii_lowercase().as_str() {
        "shift" => flag::SHIFT,
        "ctrl" | "control" => flag::CONTROL,
        "alt" | "option" | "opt" => flag::OPTION,
        "super" | "cmd" | "command" | "meta" => flag::COMMAND,
        _ => return None,
    })
}

impl ReleaseHotkey {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        if let Some(name) = spec.strip_prefix("double-") {
            let key = key_code(name).ok_or_else(|| format!("unknown key `{name}`"))?;
            return Ok(Self::DoubleTap {
                key,
                spec: spec.to_string(),
            });
        }
        let mut mods = 0u64;
        let mut key = None;
        for part in spec.split('+').map(str::trim) {
            if let Some(bit) = modifier_bit(part) {
                mods |= bit;
            } else if key.is_some() {
                return Err(format!("two keys in release hotkey `{spec}`"));
            } else {
                key = Some(key_code(part).ok_or_else(|| format!("unknown key `{part}`"))?);
            }
        }
        let key = key.ok_or_else(|| format!("no key in release hotkey `{spec}`"))?;
        Ok(Self::Chord {
            key,
            mods,
            spec: spec.to_string(),
        })
    }

    /// Human form for hints and the window subtitle.
    pub fn describe(&self) -> String {
        match self {
            Self::None => "release disabled".into(),
            Self::Chord { spec, .. } | Self::DoubleTap { spec, .. } => spec.clone(),
        }
    }
}

/// Matches key-downs against a hotkey, keeping the double-tap timer.
#[derive(Debug)]
pub struct HotkeyMatcher {
    hotkey: ReleaseHotkey,
    last_tap: Option<Instant>,
}

impl HotkeyMatcher {
    pub fn new(hotkey: ReleaseHotkey) -> Self {
        Self {
            hotkey,
            last_tap: None,
        }
    }

    pub fn hotkey(&self) -> &ReleaseHotkey {
        &self.hotkey
    }

    /// True if this key-down (macOS `code`, `NSEventModifierFlags` bits)
    /// is the release trigger. For a double-tap the first tap is not
    /// swallowed: it still reaches the remote, as in the GTK client.
    pub fn on_key_down(&mut self, code: u16, flags: u64, now: Instant) -> bool {
        match &self.hotkey {
            ReleaseHotkey::None => false,
            ReleaseHotkey::Chord { key, mods, .. } => *key == code && flags & flag::CHORD == *mods,
            ReleaseHotkey::DoubleTap { key, .. } => {
                if *key != code || flags & flag::CHORD != 0 {
                    self.last_tap = None;
                    return false;
                }
                match self.last_tap.take() {
                    Some(t) if now.duration_since(t) <= DOUBLE_TAP => true,
                    _ => {
                        self.last_tap = Some(now);
                        false
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_forms() {
        assert_eq!(ReleaseHotkey::parse("none").unwrap(), ReleaseHotkey::None);
        let ReleaseHotkey::Chord { key, mods, .. } = ReleaseHotkey::parse("shift+escape").unwrap()
        else {
            panic!()
        };
        assert_eq!((key, mods), (0x35, flag::SHIFT));
        let ReleaseHotkey::Chord { key, mods, .. } = ReleaseHotkey::parse("ctrl+alt+q").unwrap()
        else {
            panic!()
        };
        assert_eq!((key, mods), (0x0C, flag::CONTROL | flag::OPTION));
        assert!(matches!(
            ReleaseHotkey::parse("double-escape").unwrap(),
            ReleaseHotkey::DoubleTap { key: 0x35, .. }
        ));
        assert!(ReleaseHotkey::parse("shift").is_err());
        assert!(ReleaseHotkey::parse("a+b").is_err());
        assert!(ReleaseHotkey::parse("ctrl+nosuchkey").is_err());
    }

    #[test]
    fn letters_resolve_to_their_codes() {
        for (c, code) in [
            ("a", 0x00),
            ("g", 0x05),
            ("v", 0x09),
            ("b", 0x0B),
            ("t", 0x11),
        ] {
            assert_eq!(key_code(c), Some(code), "{c}");
        }
        assert_eq!(key_code("_"), None);
    }

    #[test]
    fn chords_compare_modifiers_exactly() {
        let mut m = HotkeyMatcher::new(ReleaseHotkey::parse("shift+escape").unwrap());
        let now = Instant::now();
        assert!(m.on_key_down(0x35, flag::SHIFT | (1 << 16), now)); //= + Caps Lock
        assert!(!m.on_key_down(0x35, flag::SHIFT | flag::COMMAND, now));
        assert!(!m.on_key_down(0x35, 0, now));
    }

    #[test]
    fn double_tap_needs_two_quick_presses() {
        let mut m = HotkeyMatcher::new(ReleaseHotkey::parse("double-escape").unwrap());
        let t0 = Instant::now();
        assert!(!m.on_key_down(0x35, 0, t0));
        assert!(m.on_key_down(0x35, 0, t0 + Duration::from_millis(200)));
        assert!(!m.on_key_down(0x35, 0, t0 + Duration::from_millis(300)));
        assert!(!m.on_key_down(0x35, 0, t0 + Duration::from_millis(900)));
    }
}
