//! Between the protocol's plain values and the engine's: modifier bits, cursors, key events.

use browser_ipc::message::{Cursor, KeyData, KeyKind, modifier};
use cef::*;

/// (protocol bit, the engine's `EVENTFLAG_*` bit)
const MODS: [(u32, u32); 8] = [
    (modifier::SHIFT, 1 << 1),
    (modifier::CONTROL, 1 << 2),
    (modifier::ALT, 1 << 3),
    (modifier::COMMAND, 1 << 7),
    (modifier::CAPS_LOCK, 1 << 0),
    (modifier::LEFT_BUTTON, 1 << 4),
    (modifier::MIDDLE_BUTTON, 1 << 5),
    (modifier::RIGHT_BUTTON, 1 << 6),
];

pub fn mods_to_cef(mods: u32) -> u32 {
    MODS.iter()
        .filter(|(mine, _)| mods & mine != 0)
        .fold(0, |acc, (_, theirs)| acc | theirs)
}

pub fn mods_from_cef(flags: u32) -> u32 {
    MODS.iter()
        .filter(|(_, theirs)| flags & theirs != 0)
        .fold(0, |acc, (mine, _)| acc | mine)
}

pub fn cursor_of(cursor: &CursorType) -> Cursor {
    let table: [(CursorType, Cursor); 22] = [
        (CursorType::HAND, Cursor::Hand),
        (CursorType::IBEAM, Cursor::IBeam),
        (CursorType::CROSS, Cursor::Cross),
        (CursorType::WAIT, Cursor::Wait),
        (CursorType::PROGRESS, Cursor::Progress),
        (CursorType::HELP, Cursor::Help),
        (CursorType::MOVE, Cursor::Move),
        (CursorType::NOTALLOWED, Cursor::NotAllowed),
        (CursorType::NODROP, Cursor::NotAllowed),
        (CursorType::GRAB, Cursor::Grab),
        (CursorType::GRABBING, Cursor::Grabbing),
        (CursorType::ZOOMIN, Cursor::ZoomIn),
        (CursorType::ZOOMOUT, Cursor::ZoomOut),
        (CursorType::COPY, Cursor::Copy),
        (CursorType::ALIAS, Cursor::Alias),
        (CursorType::CELL, Cursor::Cell),
        (CursorType::CONTEXTMENU, Cursor::ContextMenu),
        (CursorType::VERTICALTEXT, Cursor::VerticalText),
        (CursorType::COLUMNRESIZE, Cursor::ColumnResize),
        (CursorType::ROWRESIZE, Cursor::RowResize),
        (CursorType::EASTWESTRESIZE, Cursor::ResizeEastWest),
        (CursorType::NORTHSOUTHRESIZE, Cursor::ResizeNorthSouth),
    ];
    if let Some((_, mine)) = table.iter().find(|(theirs, _)| theirs == cursor) {
        return *mine;
    }
    let diagonals = [
        (
            CursorType::NORTHEASTSOUTHWESTRESIZE,
            Cursor::ResizeNorthEastSouthWest,
        ),
        (
            CursorType::NORTHEASTRESIZE,
            Cursor::ResizeNorthEastSouthWest,
        ),
        (
            CursorType::SOUTHWESTRESIZE,
            Cursor::ResizeNorthEastSouthWest,
        ),
        (
            CursorType::NORTHWESTSOUTHEASTRESIZE,
            Cursor::ResizeNorthWestSouthEast,
        ),
        (
            CursorType::NORTHWESTRESIZE,
            Cursor::ResizeNorthWestSouthEast,
        ),
        (
            CursorType::SOUTHEASTRESIZE,
            Cursor::ResizeNorthWestSouthEast,
        ),
        (CursorType::EASTRESIZE, Cursor::ResizeEastWest),
        (CursorType::WESTRESIZE, Cursor::ResizeEastWest),
        (CursorType::NORTHRESIZE, Cursor::ResizeNorthSouth),
        (CursorType::SOUTHRESIZE, Cursor::ResizeNorthSouth),
        (CursorType::NONE, Cursor::Hidden),
    ];
    diagonals
        .iter()
        .find(|(theirs, _)| theirs == cursor)
        .map(|(_, mine)| *mine)
        .unwrap_or(Cursor::Arrow)
}

pub fn key_kind_of(kind: &KeyEventType) -> KeyKind {
    if *kind == KeyEventType::KEYUP {
        KeyKind::Up
    } else if *kind == KeyEventType::CHAR {
        KeyKind::Char
    } else {
        KeyKind::Down
    }
}

pub fn key_type_of(kind: KeyKind) -> KeyEventType {
    match kind {
        KeyKind::Down => KeyEventType::RAWKEYDOWN,
        KeyKind::Up => KeyEventType::KEYUP,
        KeyKind::Char => KeyEventType::CHAR,
    }
}

pub fn key_data_of(event: &KeyEvent) -> KeyData {
    KeyData {
        vk: event.windows_key_code,
        native: event.native_key_code,
        character: event.character,
        unmodified: event.unmodified_character,
        mods: mods_from_cef(event.modifiers),
        system: event.is_system_key != 0,
    }
}

pub fn key_event_of(kind: KeyKind, key: &KeyData) -> KeyEvent {
    KeyEvent {
        size: std::mem::size_of::<sys::_cef_key_event_t>(),
        type_: key_type_of(kind),
        modifiers: mods_to_cef(key.mods),
        windows_key_code: key.vk,
        native_key_code: key.native,
        is_system_key: key.system as _,
        character: key.character,
        unmodified_character: key.unmodified,
        focus_on_editable_field: 0,
    }
}

/// The header bateri's proxy puts on the answer it makes up when it cannot reach the server: the
/// Chromium net error (a negative number) the failure stands for.
pub const NET_ERROR_HEADER: &str = "X-Bateri-Net-Error";

/// The code a `NET_ERROR_HEADER` value carries; anything that is not a Chromium net error
/// (-1 to -999) is not one.
pub fn net_error_code(value: &str) -> Option<i32> {
    value
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|code| (-999..=-1).contains(code))
}

/// What a net error is called (the short name Chromium shows on its own error pages).
pub fn net_error_name(code: i32) -> String {
    match code {
        -2 => "ERR_FAILED".into(),
        -7 => "ERR_TIMED_OUT".into(),
        -100 => "ERR_CONNECTION_CLOSED".into(),
        -101 => "ERR_CONNECTION_RESET".into(),
        -102 => "ERR_CONNECTION_REFUSED".into(),
        -103 => "ERR_CONNECTION_ABORTED".into(),
        -104 => "ERR_CONNECTION_FAILED".into(),
        -105 => "ERR_NAME_NOT_RESOLVED".into(),
        -106 => "ERR_INTERNET_DISCONNECTED".into(),
        -109 => "ERR_ADDRESS_UNREACHABLE".into(),
        -118 => "ERR_CONNECTION_TIMED_OUT".into(),
        -130 => "ERR_PROXY_CONNECTION_FAILED".into(),
        other => format!("net error {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_round_trip() {
        let all = modifier::SHIFT
            | modifier::CONTROL
            | modifier::ALT
            | modifier::COMMAND
            | modifier::CAPS_LOCK
            | modifier::LEFT_BUTTON
            | modifier::MIDDLE_BUTTON
            | modifier::RIGHT_BUTTON;
        assert_eq!(mods_from_cef(mods_to_cef(all)), all);
        assert_eq!(mods_to_cef(modifier::COMMAND), 1 << 7);
        assert_eq!(
            mods_to_cef(modifier::SHIFT | modifier::ALT),
            (1 << 1) | (1 << 3)
        );
        assert_eq!(mods_to_cef(0), 0);
    }

    #[test]
    fn unknown_engine_bits_are_dropped() {
        // num lock, key pad, is-left, repeat: nothing bateri can use.
        assert_eq!(
            mods_from_cef((1 << 8) | (1 << 9) | (1 << 10) | (1 << 13)),
            0
        );
    }

    #[test]
    fn cursors_map() {
        assert_eq!(cursor_of(&CursorType::HAND), Cursor::Hand);
        assert_eq!(cursor_of(&CursorType::POINTER), Cursor::Arrow);
        assert_eq!(
            cursor_of(&CursorType::NORTHEASTRESIZE),
            Cursor::ResizeNorthEastSouthWest
        );
        assert_eq!(cursor_of(&CursorType::NONE), Cursor::Hidden);
    }

    #[test]
    fn the_proxys_net_error_header_is_read() {
        assert_eq!(net_error_code("-102"), Some(-102));
        assert_eq!(net_error_code(" -118 "), Some(-118));
        for bad in ["", "0", "102", "-1000", "refused", "-1.5", "-102x"] {
            assert_eq!(net_error_code(bad), None, "{bad:?}");
        }
        assert_eq!(net_error_name(-102), "ERR_CONNECTION_REFUSED");
        assert_eq!(net_error_name(-130), "ERR_PROXY_CONNECTION_FAILED");
        assert_eq!(net_error_name(-321), "net error -321");
    }

    #[test]
    fn keys_map() {
        assert_eq!(key_kind_of(&KeyEventType::RAWKEYDOWN), KeyKind::Down);
        assert_eq!(key_kind_of(&KeyEventType::KEYDOWN), KeyKind::Down);
        assert_eq!(key_kind_of(&KeyEventType::KEYUP), KeyKind::Up);
        assert_eq!(key_kind_of(&KeyEventType::CHAR), KeyKind::Char);
        let key = KeyData {
            vk: 76,
            native: 37,
            character: 108,
            unmodified: 108,
            mods: modifier::COMMAND,
            system: true,
        };
        let event = key_event_of(KeyKind::Down, &key);
        assert_eq!(key_data_of(&event), key);
    }
}
