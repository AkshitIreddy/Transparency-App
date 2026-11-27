//! Hotkey combo strings ("ctrl+alt+t") <-> (modifiers, virtual key).

pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    TogglePaused,
    ToggleFocus,
    NudgeUp,
    NudgeDown,
    Panic,
    ToggleDimmer,
}

/// (config key, label, hotkey id, default combo, action)
pub const DEFS: &[(&str, &str, i32, &str, Action)] = &[
    ("toggle_transparency", "Toggle transparency", 1, "ctrl+alt+t", Action::TogglePaused),
    ("toggle_focus", "Toggle focus mode", 2, "ctrl+alt+f", Action::ToggleFocus),
    ("nudge_up", "More opaque (active window)", 3, "ctrl+alt+up", Action::NudgeUp),
    ("nudge_down", "More transparent (active window)", 4, "ctrl+alt+down", Action::NudgeDown),
    ("panic", "Restore everything (panic)", 5, "ctrl+alt+home", Action::Panic),
    ("toggle_dimmer", "Toggle screen dimmer", 6, "ctrl+alt+d", Action::ToggleDimmer),
];

fn key_vk(name: &str) -> Option<u32> {
    let vk = match name {
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "backspace" => 0x08,
        "escape" => 0x1B,
        "plus" => 0xBB,
        "minus" => 0xBD,
        "comma" => 0xBC,
        "period" => 0xBE,
        _ => {
            if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
                if (1..=24).contains(&n) {
                    return Some(0x70 + n - 1);
                }
                return None;
            }
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_digit() => c as u32,
                (Some(c), None) if c.is_ascii_lowercase() => c.to_ascii_uppercase() as u32,
                _ => return None,
            }
        }
    };
    Some(vk)
}

fn vk_name(vk: u32) -> String {
    match vk {
        0x26 => "↑".into(),
        0x28 => "↓".into(),
        0x25 => "←".into(),
        0x27 => "→".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x08 => "Backspace".into(),
        0x1B => "Escape".into(),
        0xBB => "Plus".into(),
        0xBD => "Minus".into(),
        0xBC => "Comma".into(),
        0xBE => "Period".into(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => char::from_u32(vk).map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
    }
}

/// 'ctrl+alt+t' -> (modifiers, vk). Needs at least one modifier and exactly
/// one other key (a bare global key would swallow normal typing).
pub fn parse(combo: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut vk = None;
    for part in combo.to_lowercase().replace(' ', "").split('+') {
        if part.is_empty() {
            continue;
        }
        match part {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" => mods |= MOD_WIN,
            k => {
                if vk.is_some() {
                    return None;
                }
                vk = Some(key_vk(k)?);
            }
        }
    }
    if mods == 0 {
        return None;
    }
    Some((mods, vk?))
}

/// Human readable parts: 'ctrl+alt+t' -> ["Ctrl", "Alt", "T"].
pub fn display_parts(combo: &str) -> Vec<String> {
    let Some((mods, vk)) = parse(combo) else {
        return vec![combo.to_string()];
    };
    let mut parts: Vec<String> = [
        (MOD_CONTROL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
        (MOD_WIN, "Win"),
    ]
    .iter()
    .filter(|(m, _)| mods & m != 0)
    .map(|(_, n)| n.to_string())
    .collect();
    parts.push(vk_name(vk));
    parts
}

pub fn format(combo: &str) -> String {
    display_parts(combo).join(" + ")
}

/// Build a combo from a key captured in the UI (Slint key text).
pub fn from_key_event(text: &str, ctrl: bool, alt: bool, shift: bool, win: bool) -> Option<String> {
    use slint::platform::Key;
    let ch = text.chars().next()?;
    let special = |k: Key| -> bool { char::from(k) == ch };
    let key: String = if special(Key::UpArrow) {
        "up".into()
    } else if special(Key::DownArrow) {
        "down".into()
    } else if special(Key::LeftArrow) {
        "left".into()
    } else if special(Key::RightArrow) {
        "right".into()
    } else if special(Key::Home) {
        "home".into()
    } else if special(Key::End) {
        "end".into()
    } else if special(Key::PageUp) {
        "pageup".into()
    } else if special(Key::PageDown) {
        "pagedown".into()
    } else if special(Key::Insert) {
        "insert".into()
    } else if special(Key::Delete) {
        "delete".into()
    } else if special(Key::Backspace) {
        "backspace".into()
    } else if special(Key::Tab) {
        "tab".into()
    } else if special(Key::Return) {
        "enter".into()
    } else if special(Key::Space) || ch == ' ' {
        "space".into()
    } else if let Some(n) = [
        Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9,
        Key::F10, Key::F11, Key::F12,
    ]
    .iter()
    .position(|k| special(k.clone()))
    {
        format!("f{}", n + 1)
    } else if ch.is_ascii_alphanumeric() {
        ch.to_ascii_lowercase().to_string()
    } else {
        match ch {
            '+' | '=' => "plus".into(),
            '-' | '_' => "minus".into(),
            ',' | '<' => "comma".into(),
            '.' | '>' => "period".into(),
            _ => return None,
        }
    };
    let mut parts = vec![];
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    if win {
        parts.push("win");
    }
    parts.push(&key);
    let combo = parts.join("+");
    parse(&combo).map(|_| combo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        assert_eq!(parse("ctrl+alt+t"), Some((MOD_CONTROL | MOD_ALT, 'T' as u32)));
        assert_eq!(parse("Ctrl + Shift + F5"), Some((MOD_CONTROL | MOD_SHIFT, 0x74)));
        assert_eq!(parse("t"), None);
        assert_eq!(parse("ctrl+a+b"), None);
        assert_eq!(parse("ctrl+nonsense"), None);
        assert_eq!(format("ctrl+alt+up"), "Ctrl + Alt + ↑");
        assert_eq!(format("alt+shift+home"), "Alt + Shift + Home");
    }

    #[test]
    fn defaults_are_valid_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for (_, _, _, combo, _) in DEFS {
            assert!(seen.insert(parse(combo).unwrap()));
        }
    }
}
