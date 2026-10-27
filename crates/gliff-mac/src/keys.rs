//! macOS input → the Linux evdev codes the gliff protocol carries.
//!
//! The server types keys through a virtual keyboard whose keymap is the XKB
//! text the client sent (or US when it sent none), so the client must send
//! *positional* evdev codes. macOS virtual key codes (`kVK_*` from Carbon's
//! `HIToolbox/Events.h`) are positional too, so this is a fixed table.
//!
//! Everything here is pure data and arithmetic; the AppKit layer calls it
//! with values read off `NSEvent`.

use gliff_proto::Axis;

//= Version of the key/button/scroll mapping; bump when a code changes.
pub const KEYMAP_VERSION: &str = "0.1.0";

/// What the Command key becomes on the remote. Hyprland binds most
/// shortcuts to Super, so that is the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CommandAs {
    #[default]
    Super,
    Ctrl,
    Alt,
}

impl std::str::FromStr for CommandAs {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "super" | "meta" | "logo" => Ok(Self::Super),
            "ctrl" | "control" => Ok(Self::Ctrl),
            "alt" => Ok(Self::Alt),
            other => Err(format!("`{other}` is not super, ctrl or alt")),
        }
    }
}

/// Options that change the key table.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyOptions {
    pub command_as: CommandAs,
    /// Swap the two keys Apple ISO keyboards exchange (`§`/`` ` ``), as
    /// Linux's `hid_apple.iso_layout` does.
    pub iso_swap: bool,
}

//= evdev codes used by name below (linux/input-event-codes.h).
pub mod ev {
    pub const KEY_LEFTCTRL: u32 = 29;
    pub const KEY_LEFTSHIFT: u32 = 42;
    pub const KEY_RIGHTSHIFT: u32 = 54;
    pub const KEY_LEFTALT: u32 = 56;
    pub const KEY_CAPSLOCK: u32 = 58;
    pub const KEY_RIGHTCTRL: u32 = 97;
    pub const KEY_RIGHTALT: u32 = 100;
    pub const KEY_LEFTMETA: u32 = 125;
    pub const KEY_RIGHTMETA: u32 = 126;
    pub const BTN_LEFT: u32 = 0x110;
    pub const BTN_RIGHT: u32 = 0x111;
    pub const BTN_MIDDLE: u32 = 0x112;
    pub const BTN_SIDE: u32 = 0x113;
}

//= macOS virtual key codes for the modifier keys (Events.h).
pub mod vk {
    pub const RIGHT_COMMAND: u16 = 0x36;
    pub const COMMAND: u16 = 0x37;
    pub const SHIFT: u16 = 0x38;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const OPTION: u16 = 0x3A;
    pub const CONTROL: u16 = 0x3B;
    pub const RIGHT_SHIFT: u16 = 0x3C;
    pub const RIGHT_OPTION: u16 = 0x3D;
    pub const RIGHT_CONTROL: u16 = 0x3E;
    pub const ISO_SECTION: u16 = 0x0A;
    pub const ANSI_GRAVE: u16 = 0x32;
}

/// `kVK_*` → evdev for every key that is not a modifier. Index is the
/// macOS virtual key code; 0 means "no remote equivalent".
#[rustfmt::skip]
const TABLE: [u16; 0x80] = [
    //= 0x00 A    S    D    F    H    G    Z    X
            30,  31,  32,  33,  35,  34,  44,  45,
    //= 0x08 C    V    §    B    Q    W    E    R
            46,  47,  86,  48,  16,  17,  18,  19,
    //= 0x10 Y    T    1    2    3    4    6    5
            21,  20,   2,   3,   4,   5,   7,   6,
    //= 0x18 =    9    7    -    8    0    ]    O
            13,  10,   8,  12,   9,  11,  27,  24,
    //= 0x20 U    [    I    P    Ret  L    J    '
            22,  26,  23,  25,  28,  38,  36,  40,
    //= 0x28 K    ;    \    ,    /    N    M    .
            37,  39,  43,  51,  53,  49,  50,  52,
    //= 0x30 Tab  Spc  `    Bksp --   Esc  RCmd Cmd
            15,  57,  41,  14,   0,   1, 126, 125,
    //= 0x38 Shft Caps Opt  Ctrl RShf ROpt RCtl Fn
            42,  58,  56,  29,  54, 100,  97,   0,
    //= 0x40 F17  KP.  --   KP*  --   KP+  --   Clear(NumLock)
           187,  83,   0,  55,   0,  78,   0,  69,
    //= 0x48 Vol+ Vol- Mute KP/  KPEn --   KP-  F18
           115, 114, 113,  98,  96,   0,  74, 188,
    //= 0x50 F19  KP=  KP0  KP1  KP2  KP3  KP4  KP5
           189, 117,  82,  79,  80,  81,  75,  76,
    //= 0x58 KP6  KP7  F20  KP8  KP9  Yen  Ro   KP,
            77,  71, 190,  72,  73, 124,  89, 121,
    //= 0x60 F5   F6   F7   F3   F8   F9   Eisu F11
            63,  64,  65,  61,  66,  67, 123,  87,
    //= 0x68 Kana F13  F16  F14  --   F10  Menu F12
           122, 183, 186, 184,   0,  68, 127,  88,
    //= 0x70 --   F15  Help(Insert) Home PgUp FwdDel F4  End
             0, 185, 110, 102, 104, 111,  62, 107,
    //= 0x78 F2   PgDn F1   Left Right Down Up   --
            60, 109,  59, 105, 106, 108, 103,   0,
];

/// The evdev code for a macOS virtual key code, or `None` for keys with no
/// remote meaning (Fn) and unknown codes.
pub fn evdev_key(code: u16, opts: KeyOptions) -> Option<u32> {
    let code = match (opts.iso_swap, code) {
        (true, vk::ISO_SECTION) => vk::ANSI_GRAVE,
        (true, vk::ANSI_GRAVE) => vk::ISO_SECTION,
        _ => code,
    };
    let mapped = match code {
        vk::COMMAND | vk::RIGHT_COMMAND => {
            let right = code == vk::RIGHT_COMMAND;
            match (opts.command_as, right) {
                (CommandAs::Super, false) => ev::KEY_LEFTMETA,
                (CommandAs::Super, true) => ev::KEY_RIGHTMETA,
                (CommandAs::Ctrl, false) => ev::KEY_LEFTCTRL,
                (CommandAs::Ctrl, true) => ev::KEY_RIGHTCTRL,
                (CommandAs::Alt, false) => ev::KEY_LEFTALT,
                (CommandAs::Alt, true) => ev::KEY_RIGHTALT,
            }
        }
        _ => u32::from(*TABLE.get(usize::from(code))?),
    };
    (mapped != 0).then_some(mapped)
}

/// Device-dependent modifier bits in `NSEvent.modifierFlags` (IOKit's
/// `NX_DEVICE*KEYMASK`). They tell left from right, which the
/// device-independent flags do not.
pub mod device_mask {
    pub const LCTL: u64 = 0x0000_0001;
    pub const LSHIFT: u64 = 0x0000_0002;
    pub const RSHIFT: u64 = 0x0000_0004;
    pub const LCMD: u64 = 0x0000_0008;
    pub const RCMD: u64 = 0x0000_0010;
    pub const LALT: u64 = 0x0000_0020;
    pub const RALT: u64 = 0x0000_0040;
    pub const RCTL: u64 = 0x0000_2000;
}

/// Device-independent modifier flags (`NSEventModifierFlags`).
pub mod flag {
    pub const SHIFT: u64 = 1 << 17;
    pub const CONTROL: u64 = 1 << 18;
    pub const OPTION: u64 = 1 << 19;
    pub const COMMAND: u64 = 1 << 20;
    /// The four that make a shortcut chord.
    pub const CHORD: u64 = SHIFT | CONTROL | OPTION | COMMAND;
}

/// What a `flagsChanged` event for modifier key `code` means: whether the
/// key is now down. Read from the device-dependent bit for that very key,
/// so pressing right Shift while left Shift is held still reads correctly.
/// Caps Lock reports its lock state, not the key; `None` there tells the
/// caller to send a full press-and-release tap.
pub fn modifier_down(code: u16, flags: u64) -> Option<bool> {
    let mask = match code {
        vk::CONTROL => device_mask::LCTL,
        vk::RIGHT_CONTROL => device_mask::RCTL,
        vk::SHIFT => device_mask::LSHIFT,
        vk::RIGHT_SHIFT => device_mask::RSHIFT,
        vk::COMMAND => device_mask::LCMD,
        vk::RIGHT_COMMAND => device_mask::RCMD,
        vk::OPTION => device_mask::LALT,
        vk::RIGHT_OPTION => device_mask::RALT,
        _ => return None,
    };
    Some(flags & mask != 0)
}

/// `NSEvent.buttonNumber` → evdev `BTN_*`: 0 left, 1 right, 2 middle, and
/// 3+ the side buttons (back, forward, ...) in evdev order.
pub fn evdev_button(number: i64) -> u32 {
    match number {
        0 => ev::BTN_LEFT,
        1 => ev::BTN_RIGHT,
        2 => ev::BTN_MIDDLE,
        n if (3..=10).contains(&n) => ev::BTN_SIDE + (n as u32 - 3),
        _ => ev::BTN_LEFT,
    }
}

/// One `PointerAxis` to send.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisEvent {
    pub axis: Axis,
    pub value: f64,
    pub discrete: Option<i32>,
}

/// Wayland's pixel distance for one wheel notch, as the GTK client uses.
pub const WHEEL_STEP: f64 = 15.0;

/// Translate an `NSEvent` scroll into wayland axis events.
///
/// AppKit's deltas point the way the *content* moves (with the user's
/// natural-scrolling setting applied); wayland's point the way the
/// *viewport* moves. Hence the sign flip. Precise (trackpad / Magic Mouse)
/// deltas are already in points; a classic wheel reports lines, which
/// become notches of [`WHEEL_STEP`] with a `discrete` count.
pub fn scroll(dx: f64, dy: f64, precise: bool) -> Vec<AxisEvent> {
    let one = |axis: Axis, d: f64| {
        if d == 0.0 || !d.is_finite() {
            return None;
        }
        let d = -d;
        Some(if precise {
            AxisEvent {
                axis,
                value: d,
                discrete: None,
            }
        } else {
            //= macOS accelerates wheel lines (0.1, 2.7, ...); at least one notch.
            let notches = d.round().clamp(-10.0, 10.0) as i32;
            let notches = if notches == 0 {
                d.signum() as i32
            } else {
                notches
            };
            AxisEvent {
                axis,
                value: f64::from(notches) * WHEEL_STEP,
                discrete: Some(notches),
            }
        })
    };
    [one(Axis::Vertical, dy), one(Axis::Horizontal, dx)]
        .into_iter()
        .flatten()
        .collect()
}

/// Evdev keys held on the remote, so they can all be released when the
/// keyboard leaves the picture or Command swallows a key-up.
#[derive(Default, Debug)]
pub struct HeldKeys {
    keys: std::collections::BTreeSet<u32>,
}

impl HeldKeys {
    /// Record a press; false when it was already down (an autorepeat or a
    /// duplicate), which the caller should not send again.
    pub fn press(&mut self, code: u32) -> bool {
        self.keys.insert(code)
    }

    /// Record a release; false when the press never reached the remote,
    /// so a stray release is not sent either.
    pub fn release(&mut self, code: u32) -> bool {
        self.keys.remove(&code)
    }

    /// Every held key, emptied.
    pub fn drain(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.keys).into_iter().collect()
    }

    /// Held keys that are not modifiers: macOS sends no key-up for a key
    /// released while Command is down, so these are released with Command.
    pub fn drain_non_modifiers(&mut self) -> Vec<u32> {
        let (mods, rest): (Vec<u32>, Vec<u32>) = std::mem::take(&mut self.keys)
            .into_iter()
            .partition(|c| is_modifier(*c));
        self.keys = mods.into_iter().collect();
        rest
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// True for the evdev modifier codes.
pub fn is_modifier(code: u32) -> bool {
    matches!(
        code,
        ev::KEY_LEFTCTRL
            | ev::KEY_RIGHTCTRL
            | ev::KEY_LEFTSHIFT
            | ev::KEY_RIGHTSHIFT
            | ev::KEY_LEFTALT
            | ev::KEY_RIGHTALT
            | ev::KEY_LEFTMETA
            | ev::KEY_RIGHTMETA
            | ev::KEY_CAPSLOCK
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16) -> Option<u32> {
        evdev_key(code, KeyOptions::default())
    }

    #[test]
    fn letters_and_digits_are_positional() {
        assert_eq!(key(0x00), Some(30)); //= A
        assert_eq!(key(0x0C), Some(16)); //= Q
        assert_eq!(key(0x06), Some(44)); //= Z
        assert_eq!(key(0x12), Some(2)); //= 1
        assert_eq!(key(0x1D), Some(11)); //= 0
        assert_eq!(key(0x24), Some(28)); //= Return
        assert_eq!(key(0x33), Some(14)); //= Delete is Backspace
        assert_eq!(key(0x75), Some(111)); //= Forward delete is Delete
    }

    #[test]
    fn every_mapped_code_is_unique() {
        let mut seen = std::collections::BTreeMap::new();
        for code in 0..0x80u16 {
            if let Some(ev) = key(code) {
                if let Some(prev) = seen.insert(ev, code) {
                    panic!("evdev {ev} from both {prev:#x} and {code:#x}");
                }
            }
        }
    }

    #[test]
    fn function_keys_follow_the_odd_mac_order() {
        let f = [
            0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
        ];
        let got: Vec<u32> = f.iter().map(|c| key(*c).unwrap()).collect();
        assert_eq!(got, vec![59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88]);
    }

    #[test]
    fn command_follows_the_option() {
        let o = |c| KeyOptions {
            command_as: c,
            iso_swap: false,
        };
        assert_eq!(evdev_key(vk::COMMAND, o(CommandAs::Super)), Some(125));
        assert_eq!(evdev_key(vk::RIGHT_COMMAND, o(CommandAs::Ctrl)), Some(97));
        assert_eq!(evdev_key(vk::COMMAND, o(CommandAs::Alt)), Some(56));
        assert_eq!(key(0x3F), None); //= Fn
        assert_eq!(key(0x7F), None);
        assert_eq!(key(0x1234), None);
    }

    #[test]
    fn iso_swap_exchanges_section_and_grave() {
        let o = KeyOptions {
            iso_swap: true,
            ..Default::default()
        };
        assert_eq!(evdev_key(vk::ISO_SECTION, o), Some(41)); //= KEY_GRAVE
        assert_eq!(evdev_key(vk::ANSI_GRAVE, o), Some(86)); //= KEY_102ND
    }

    #[test]
    fn modifiers_read_their_own_device_bit() {
        let flags = device_mask::LSHIFT | flag::SHIFT;
        assert_eq!(modifier_down(vk::SHIFT, flags), Some(true));
        assert_eq!(modifier_down(vk::RIGHT_SHIFT, flags), Some(false));
        assert_eq!(modifier_down(vk::CAPS_LOCK, flags), None);
    }

    #[test]
    fn buttons_map_to_evdev() {
        assert_eq!(evdev_button(0), 0x110);
        assert_eq!(evdev_button(1), 0x111);
        assert_eq!(evdev_button(2), 0x112);
        assert_eq!(evdev_button(3), 0x113);
        assert_eq!(evdev_button(4), 0x114);
        assert_eq!(evdev_button(-1), 0x110);
    }

    #[test]
    fn scroll_flips_sign_and_quantises_wheels() {
        let e = scroll(0.0, 2.5, true);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].value, -2.5);
        assert_eq!(e[0].discrete, None);
        let e = scroll(0.0, 0.1, false);
        assert_eq!(e[0].discrete, Some(-1));
        assert_eq!(e[0].value, -WHEEL_STEP);
        let e = scroll(-3.0, 0.0, false);
        assert_eq!(e[0].axis, Axis::Horizontal);
        assert_eq!(e[0].discrete, Some(3));
        assert!(scroll(0.0, 0.0, true).is_empty());
    }

    #[test]
    fn held_keys_release_non_modifiers_with_command() {
        let mut h = HeldKeys::default();
        assert!(h.press(125));
        assert!(h.press(30));
        assert!(!h.press(30));
        assert_eq!(h.drain_non_modifiers(), vec![30]);
        assert!(!h.release(30));
        assert_eq!(h.drain(), vec![125]);
        assert!(h.is_empty());
    }
}
