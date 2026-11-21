//! A key that carries editing commands goes to the page through the protocol's
//! `Input.dispatchKeyEvent` (the engine's own key call has nowhere to put the commands), and that
//! call takes the DOM's names for the key where the engine's takes codes: this turns bateri's key
//! (Windows virtual-key code, macOS key code, characters) into them.

use browser_ipc::message::{KeyData, modifier};
use serde_json::{Value, json};

/// How many commands a key may carry and how long a command's name may be: the system binds a key
/// to a handful of commands, so more is not a key.
const MAX_COMMANDS: usize = 8;
const MAX_NAME: usize = 48;

/// The names of the commands the way the page's editor knows them: `moveToBeginningOfLine:` (the
/// selector the system called) is `moveToBeginningOfLine`. The ones that insert text are left
/// out, like Chrome does: the text of such a key arrives as text, and Tab would otherwise type a
/// tab where it should move on. `noop` is the system saying the key has no command.
pub fn clean_commands(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|n| n.strip_suffix(':').unwrap_or(n))
        .filter(|n| !n.is_empty() && n.len() <= MAX_NAME)
        .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric()))
        .filter(|n| *n != "noop" && !n.starts_with("insert"))
        .take(MAX_COMMANDS)
        .map(str::to_string)
        .collect()
}

/// The protocol's modifier bits (Alt 1, Control 2, Meta 4, Shift 8).
fn protocol_mods(mods: u32) -> u32 {
    [
        (modifier::ALT, 1),
        (modifier::CONTROL, 2),
        (modifier::COMMAND, 4),
        (modifier::SHIFT, 8),
    ]
    .iter()
    .filter(|(mine, _)| mods & mine != 0)
    .fold(0, |acc, (_, theirs)| acc | theirs)
}

/// The DOM `KeyboardEvent.key` of keys that are not characters, by Windows virtual-key code.
fn named_key(vk: i32) -> Option<&'static str> {
    Some(match vk {
        8 => "Backspace",
        9 => "Tab",
        13 => "Enter",
        27 => "Escape",
        33 => "PageUp",
        34 => "PageDown",
        35 => "End",
        36 => "Home",
        37 => "ArrowLeft",
        38 => "ArrowUp",
        39 => "ArrowRight",
        40 => "ArrowDown",
        45 => "Insert",
        46 => "Delete",
        112 => "F1",
        113 => "F2",
        114 => "F3",
        115 => "F4",
        116 => "F5",
        117 => "F6",
        118 => "F7",
        119 => "F8",
        120 => "F9",
        121 => "F10",
        122 => "F11",
        123 => "F12",
        _ => return None,
    })
}

/// `KeyboardEvent.key` of a key: its name, or the character it makes without the system's
/// control-character and function-key stand-ins (macOS gives the arrows private-use characters).
fn dom_key(key: &KeyData) -> String {
    if let Some(name) = named_key(key.vk) {
        return name.into();
    }
    let printable = |unit: u16| {
        let c = char::from_u32(unit as u32)?;
        (!c.is_control() && !(0xF700..=0xF8FF).contains(&(unit as u32))).then_some(c)
    };
    // With Control held the character is the control code; the key is still the letter.
    match printable(key.character).or_else(|| printable(key.unmodified)) {
        Some(c) if key.mods & modifier::SHIFT != 0 => c.to_uppercase().collect(),
        Some(c) => c.to_string(),
        None => "Unidentified".into(),
    }
}

/// `KeyboardEvent.code` by the macOS key code (the physical key, whatever the layout).
fn dom_code(native: i32) -> &'static str {
    match native {
        0 => "KeyA",
        1 => "KeyS",
        2 => "KeyD",
        3 => "KeyF",
        4 => "KeyH",
        5 => "KeyG",
        6 => "KeyZ",
        7 => "KeyX",
        8 => "KeyC",
        9 => "KeyV",
        10 => "IntlBackslash",
        11 => "KeyB",
        12 => "KeyQ",
        13 => "KeyW",
        14 => "KeyE",
        15 => "KeyR",
        16 => "KeyY",
        17 => "KeyT",
        18 => "Digit1",
        19 => "Digit2",
        20 => "Digit3",
        21 => "Digit4",
        22 => "Digit6",
        23 => "Digit5",
        24 => "Equal",
        25 => "Digit9",
        26 => "Digit7",
        27 => "Minus",
        28 => "Digit8",
        29 => "Digit0",
        30 => "BracketRight",
        31 => "KeyO",
        32 => "KeyU",
        33 => "BracketLeft",
        34 => "KeyI",
        35 => "KeyP",
        36 => "Enter",
        37 => "KeyL",
        38 => "KeyJ",
        39 => "Quote",
        40 => "KeyK",
        41 => "Semicolon",
        42 => "Backslash",
        43 => "Comma",
        44 => "Slash",
        45 => "KeyN",
        46 => "KeyM",
        47 => "Period",
        48 => "Tab",
        49 => "Space",
        50 => "Backquote",
        51 => "Backspace",
        53 => "Escape",
        54 => "MetaRight",
        55 => "MetaLeft",
        56 => "ShiftLeft",
        58 => "AltLeft",
        59 => "ControlLeft",
        60 => "ShiftRight",
        61 => "AltRight",
        62 => "ControlRight",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        99 => "F3",
        100 => "F8",
        101 => "F9",
        103 => "F11",
        109 => "F10",
        111 => "F12",
        114 => "Insert",
        115 => "Home",
        116 => "PageUp",
        117 => "Delete",
        118 => "F4",
        119 => "End",
        120 => "F2",
        121 => "PageDown",
        122 => "F1",
        123 => "ArrowLeft",
        124 => "ArrowRight",
        125 => "ArrowDown",
        126 => "ArrowUp",
        _ => "",
    }
}

/// The parameters of `Input.dispatchKeyEvent` for a key going down with editing commands. It is a
/// `rawKeyDown` without text: a key that has commands is not a character (the text a key types is
/// sent as text), and the page's default for it is the commands.
pub fn dispatch_key_down(key: &KeyData, commands: &[String]) -> Value {
    json!({
        "type": "rawKeyDown",
        "modifiers": protocol_mods(key.mods),
        "windowsVirtualKeyCode": key.vk,
        "nativeVirtualKeyCode": key.native,
        "key": dom_key(key),
        "code": dom_code(key.native),
        "isSystemKey": key.system,
        "commands": commands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(of: &[&str]) -> Vec<String> {
        of.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn selectors_become_the_editors_command_names() {
        assert_eq!(
            clean_commands(&names(&[
                "moveToBeginningOfLine:",
                "selectAll",
                "deleteToBeginningOfLine:"
            ])),
            names(&[
                "moveToBeginningOfLine",
                "selectAll",
                "deleteToBeginningOfLine"
            ])
        );
    }

    #[test]
    fn text_inserting_and_empty_commands_are_dropped() {
        assert_eq!(
            clean_commands(&names(&[
                "insertNewline:",
                "insertTab:",
                "noop:",
                "",
                ":",
                "moveLeft:"
            ])),
            names(&["moveLeft"])
        );
    }

    #[test]
    fn a_command_is_a_plain_name() {
        let long = "a".repeat(MAX_NAME + 1);
        let many: Vec<String> = (0..20).map(|n| format!("c{n}")).collect();
        assert!(clean_commands(&names(&["a b", "x;y", "../x", &long])).is_empty());
        assert_eq!(clean_commands(&many).len(), MAX_COMMANDS);
    }

    #[test]
    fn a_command_arrow_is_named_for_the_page() {
        let key = KeyData {
            vk: 37,
            native: 123,
            character: 0xF702,
            unmodified: 0xF702,
            mods: modifier::COMMAND,
            system: true,
        };
        let call = dispatch_key_down(&key, &names(&["moveToBeginningOfLine"]));
        assert_eq!(call["key"], "ArrowLeft");
        assert_eq!(call["code"], "ArrowLeft");
        assert_eq!(call["modifiers"], 4);
        assert_eq!(call["windowsVirtualKeyCode"], 37);
        assert_eq!(call["nativeVirtualKeyCode"], 123);
        assert_eq!(call["isSystemKey"], true);
        assert_eq!(call["commands"][0], "moveToBeginningOfLine");
    }

    #[test]
    fn a_control_letter_is_the_letter() {
        // Control-A: the character is the control code 1, the key is `a` on the A key.
        let key = KeyData {
            vk: 65,
            native: 0,
            character: 1,
            unmodified: 'a' as u16,
            mods: modifier::CONTROL,
            system: false,
        };
        let call = dispatch_key_down(&key, &names(&["moveToBeginningOfLine"]));
        assert_eq!(call["key"], "a");
        assert_eq!(call["code"], "KeyA");
        assert_eq!(call["modifiers"], 2);
        let shifted = KeyData {
            mods: modifier::CONTROL | modifier::SHIFT,
            ..key
        };
        assert_eq!(dispatch_key_down(&shifted, &[])["key"], "A");
        assert_eq!(dispatch_key_down(&shifted, &[])["modifiers"], 10);
    }
}
