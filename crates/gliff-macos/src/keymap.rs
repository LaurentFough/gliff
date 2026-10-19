//! Export the Mac's Unicode keyboard layout as an XKB map for Linux hosts.
use anyhow::{ensure, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct Layout {
    keys: Vec<Key>,
}

#[derive(Deserialize)]
struct Key {
    keycode: u16,
    levels: Vec<Level>,
}

#[derive(Deserialize)]
struct Level {
    text: String,
    dead: bool,
}

pub fn from_event(event: &serde_json::Value) -> Result<String> {
    let layout: Layout = serde_json::from_value(event.clone())?;
    ensure!(layout.keys.len() <= 128, "too many keyboard layout keys");
    let mut map = String::from(
        "xkb_keymap {\n\
         xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n\
         xkb_types { include \"complete\"\n\
           type \"MAC\" { modifiers = Shift+Lock+Mod1;\n\
             map[None]=Level1; map[Shift]=Level2;\n\
             map[Mod1]=Level3; map[Shift+Mod1]=Level4;\n\
             map[Lock]=Level5; map[Shift+Lock]=Level6;\n\
             map[Lock+Mod1]=Level7; map[Shift+Lock+Mod1]=Level8;\n\
           };\n\
         };\n\
         xkb_compatibility { include \"complete\" };\n\
         xkb_symbols { include \"pc+us+inet(evdev)\"\n",
    );
    for key in layout.keys {
        ensure!(key.levels.len() == 8, "invalid keyboard layout levels");
        let Some(name) = crate::keys::evdev(key.keycode).and_then(key_name) else {
            continue;
        };
        let levels = key.levels.iter().map(symbol).collect::<Vec<_>>().join(", ");
        map.push_str(&format!(
            "  replace key <{name}> {{ type[Group1]=\"MAC\", symbols[Group1]=[ {levels} ] }};\n"
        ));
    }
    map.push_str("};\n};\n");
    Ok(map)
}

fn key_name(code: u32) -> Option<String> {
    Some(match code {
        2..=13 => format!("AE{:02}", code - 1),
        16..=27 => format!("AD{:02}", code - 15),
        30..=40 => format!("AC{:02}", code - 29),
        44..=53 => format!("AB{:02}", code - 43),
        41 => "TLDE".into(),
        43 => "BKSL".into(),
        57 => "SPCE".into(),
        86 => "LSGT".into(),
        71..=73 => format!("KP{}", code - 64),
        75..=77 => format!("KP{}", code - 71),
        79..=81 => format!("KP{}", code - 78),
        82 => "KP0".into(),
        83 => "KPDL".into(),
        55 => "KPMU".into(),
        74 => "KPSU".into(),
        78 => "KPAD".into(),
        98 => "KPDV".into(),
        117 => "KPEQ".into(),
        _ => return None,
    })
}

fn symbol(level: &Level) -> String {
    let mut chars = level.text.chars();
    let Some(c) = chars.next().filter(|c| !c.is_control()) else {
        return "NoSymbol".into();
    };
    if chars.next().is_some() {
        return "NoSymbol".into();
    }
    if level.dead {
        let name = match c {
            '`' | '\u{0300}' => "dead_grave",
            '´' | '\u{0301}' => "dead_acute",
            '^' | '\u{0302}' => "dead_circumflex",
            '~' | '\u{0303}' => "dead_tilde",
            '¨' | '\u{0308}' => "dead_diaeresis",
            'ˇ' | '\u{030c}' => "dead_caron",
            '˘' | '\u{0306}' => "dead_breve",
            '¯' | '\u{0304}' => "dead_macron",
            '˚' | '\u{030a}' => "dead_abovering",
            '˝' | '\u{030b}' => "dead_doubleacute",
            '˙' | '\u{0307}' => "dead_abovedot",
            '¸' | '\u{0327}' => "dead_cedilla",
            '˛' | '\u{0328}' => "dead_ogonek",
            _ => return format!("U{:04X}", c as u32),
        };
        return name.into();
    }
    format!("U{:04X}", c as u32)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use xkbcommon::xkb;

    #[test]
    fn german_positions_shift_caps_option_and_dead_keys_compile() {
        // Captured through production UCKeyTranslate without selecting the
        // German layout or changing the user's current input source.
        let event =
            serde_json::from_str(include_str!("../tests/fixtures/german-layout.json")).unwrap();
        let text = from_event(&event).unwrap();
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_string(
            &context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("Mac layout must compile in the Linux host's XKB implementation");
        let mut state = xkb::State::new(&map);
        let utf8 = |state: &xkb::State, code: u32| state.key_get_utf8(xkb::Keycode::new(code + 8));
        assert_eq!(utf8(&state, 21), "z");
        assert_eq!(utf8(&state, 44), "y");
        assert_eq!(utf8(&state, 39), "ö");
        state.update_key(xkb::Keycode::new(42 + 8), xkb::KeyDirection::Down);
        assert_eq!(utf8(&state, 21), "Z");
        state.update_key(xkb::Keycode::new(42 + 8), xkb::KeyDirection::Up);
        state.update_key(xkb::Keycode::new(58 + 8), xkb::KeyDirection::Down);
        state.update_key(xkb::Keycode::new(58 + 8), xkb::KeyDirection::Up);
        assert_eq!(utf8(&state, 21), "Z");
        state.update_key(xkb::Keycode::new(56 + 8), xkb::KeyDirection::Down);
        assert_eq!(utf8(&state, 38), "@");
        state.update_key(xkb::Keycode::new(56 + 8), xkb::KeyDirection::Up);
        assert_eq!(
            xkb::keysym_get_name(state.key_get_one_sym(xkb::Keycode::new(13 + 8))),
            "dead_acute"
        );
        assert_eq!(
            xkb::keysym_get_name(state.key_get_one_sym(xkb::Keycode::new(28 + 8))),
            "Return"
        );
    }
}
