//! Every message kind has a fixture file that is its wire form: the file parses to the value the
//! table holds and the value serializes back to the file. `BLESS=1 cargo test -p browser-ipc
//! --test fixtures` rewrites them, so a change to the wire shows up as a diff to review.

use browser_ipc::channel::{read_message, write_message};
use browser_ipc::manifest::{Asset, FORMAT, Manifest};
use browser_ipc::message::*;
use std::path::PathBuf;

fn dir(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(sub)
}

fn profile() -> Profile {
    Profile {
        context: "mac/app/work".into(),
        dir: Some("0123abcd".into()),
        user: "u-7".into(),
        pass: "p-7".into(),
    }
}

fn key() -> KeyData {
    KeyData {
        vk: 37,
        native: 123,
        character: 0xf702,
        unmodified: 0xf702,
        mods: modifier::COMMAND,
        system: true,
    }
}

fn commands() -> Vec<Command> {
    vec![
        Command::Open {
            browser: 1,
            profile: profile(),
            width: 800,
            height: 600,
            scale: 2.0,
        },
        Command::Close { browser: 1 },
        Command::Navigate {
            browser: 1,
            url: "http://localhost:3000/".into(),
        },
        Command::Back { browser: 1 },
        Command::Forward { browser: 1 },
        Command::Reload {
            browser: 1,
            ignore_cache: true,
        },
        Command::Stop { browser: 1 },
        Command::Resize {
            browser: 1,
            width: 1024,
            height: 768,
            scale: 2.0,
        },
        Command::Focus {
            browser: 1,
            on: true,
        },
        Command::Mouse {
            browser: 1,
            kind: MouseKind::Down,
            x: 10,
            y: 20,
            button: Button::Right,
            clicks: 1,
            mods: modifier::SHIFT | modifier::RIGHT_BUTTON,
        },
        Command::Wheel {
            browser: 1,
            x: 10,
            y: 20,
            dx: 0,
            dy: -120,
            mods: 0,
        },
        Command::CanScroll {
            browser: 1,
            req: 5,
            x: 10,
            y: 20,
            dx: 30,
        },
        Command::Windows {
            browser: 1,
            mode: NewWindows::Here,
        },
        Command::Pinch {
            browser: 1,
            kind: PinchKind::Update,
            x: 10,
            y: 20,
            scale: 1.05,
        },
        Command::Key {
            browser: 1,
            kind: KeyKind::Down,
            key: key(),
            commands: vec![],
        },
        Command::Edit {
            browser: 1,
            action: EditAction::SelectAll,
        },
        Command::ImeCommit {
            browser: 1,
            text: "çğıöşü İiıI".into(),
        },
        Command::ImeCompose {
            browser: 1,
            text: "ka".into(),
            from: 1,
            to: 2,
        },
        Command::ImeCancel { browser: 1 },
        Command::Find {
            browser: 1,
            pattern: "needle".into(),
            case: false,
            forward: true,
            next: false,
        },
        Command::StopFinding {
            browser: 1,
            keep_selection: true,
        },
        Command::Zoom {
            browser: 1,
            ratio: 1.25,
        },
        Command::DevTools { browser: 1 },
        Command::Agent {
            browser: 1,
            req: 9,
            op: AgentOp::Click {
                target: Target::Node { id: 12 },
            },
        },
        Command::MenuResult {
            req: 3,
            command: Some(11),
        },
        Command::DialogResult {
            req: 4,
            ok: true,
            text: "typed".into(),
        },
        Command::FileResult {
            req: 5,
            paths: vec!["/tmp/a.csv".into()],
        },
        Command::DownloadResult {
            req: 6,
            path: Some("/tmp/report.csv".into()),
        },
        Command::DownloadCancel { req: 6 },
        Command::Quit,
    ]
}

fn events() -> Vec<Event> {
    vec![
        Event::Hello {
            protocol: PROTOCOL,
            version: "0.1.0".into(),
            chromium: "156.0.7871.0".into(),
            pid: 4242,
            key: "00ff".into(),
        },
        Event::Opened { browser: 1 },
        Event::Closed { browser: 1 },
        Event::State {
            browser: 1,
            url: "http://localhost:3000/".into(),
            title: "Dev".into(),
            loading: false,
            can_back: true,
            can_forward: false,
            status: String::new(),
            zoom: 1.0,
            error: Some(LoadError {
                url: "http://x.test/".into(),
                code: -105,
                message: "net::ERR_NAME_NOT_RESOLVED".into(),
            }),
            found: Some(Found {
                count: 4,
                current: 2,
            }),
        },
        Event::Cursor {
            browser: 1,
            cursor: Cursor::IBeam,
        },
        Event::Popup {
            browser: 1,
            show: true,
            x: 40,
            y: 80,
            width: 200,
            height: 120,
        },
        Event::Crashed {
            browser: 1,
            reason: "killed".into(),
        },
        Event::Refused {
            browser: 1,
            url: "file:///etc/hosts".into(),
            reason: "scheme".into(),
        },
        Event::Window {
            browser: 1,
            url: "http://a.test/".into(),
            background: true,
        },
        Event::UnhandledKey {
            browser: 1,
            kind: KeyKind::Down,
            key: key(),
        },
        Event::Menu {
            browser: 1,
            req: 3,
            x: 5,
            y: 6,
            items: vec![
                MenuItem {
                    command: 11,
                    label: "Back".into(),
                    enabled: false,
                    checked: false,
                    separator: false,
                    shortcut: String::new(),
                },
                MenuItem {
                    command: 0,
                    label: String::new(),
                    enabled: false,
                    checked: false,
                    separator: true,
                    shortcut: String::new(),
                },
            ],
            link: Some("http://a.test/".into()),
            source: None,
            selection: Some("words".into()),
            editable: false,
        },
        Event::Dialog {
            browser: 1,
            req: 4,
            kind: DialogKind::Prompt,
            origin: "http://a.test".into(),
            message: "Name?".into(),
            default_text: "x".into(),
        },
        Event::DialogVoid { browser: 1, req: 4 },
        Event::FileDialog {
            browser: 1,
            req: 5,
            mode: FileMode::OpenMultiple,
            title: "Choose".into(),
            default_path: String::new(),
            accept: vec![".csv".into(), "image/png".into()],
        },
        Event::Download {
            browser: 1,
            req: 6,
            url: "http://a.test/r.csv".into(),
            name: "r.csv".into(),
            mime: "text/csv".into(),
            size: Some(2048),
        },
        Event::DownloadProgress {
            req: 6,
            state: DownloadState::Running,
            received: 1024,
            total: Some(2048),
        },
        Event::AgentResult {
            browser: 1,
            req: 9,
            output: AgentOutput::Text {
                text: "hello".into(),
                truncated: false,
            },
        },
        Event::CanScroll {
            browser: 1,
            req: 5,
            can: false,
        },
        Event::Error {
            browser: Some(1),
            message: "no such browser".into(),
        },
    ]
}

fn check<T>(sub: &str, name: &str, value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let path = dir(sub).join(format!("{name}.json"));
    let want = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(&path, &want).unwrap();
    }
    let have = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (BLESS=1 writes it)", path.display()));
    assert_eq!(
        have,
        want,
        "{} is not what the type serializes to",
        path.display()
    );
    let back: T = serde_json::from_str(&have).unwrap();
    assert_eq!(
        &back,
        value,
        "{} does not parse to the table's value",
        path.display()
    );
}

fn files(sub: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir(sub))
        .unwrap()
        .map(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".json")
                .to_string()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn every_command_has_its_fixture() {
    let all = commands();
    for c in &all {
        check("to-helper", c.name(), c);
    }
    // A shape of a message that has an optional part has a fixture of its own besides the plain one.
    let with_commands = Command::Key {
        browser: 1,
        kind: KeyKind::Down,
        key: key(),
        commands: vec!["moveToBeginningOfLine".into(), "selectAll".into()],
    };
    check("to-helper", "key_commands", &with_commands);
    let mut want: Vec<String> = all.iter().map(|c| c.name().to_string()).collect();
    want.push("key_commands".into());
    want.sort();
    assert_eq!(
        files("to-helper"),
        want,
        "a fixture without a message, or the reverse"
    );
}

#[test]
fn every_event_has_its_fixture() {
    let all = events();
    for e in &all {
        check("to-bateri", e.name(), e);
    }
    // A row with keys of its own has a fixture besides the plain menu.
    let with_shortcut = Event::Menu {
        browser: 1,
        req: 3,
        x: 5,
        y: 6,
        items: vec![MenuItem {
            command: 26500,
            label: "Zoom In".into(),
            enabled: true,
            checked: false,
            separator: false,
            shortcut: "⌘+".into(),
        }],
        link: None,
        source: None,
        selection: None,
        editable: false,
    };
    check("to-bateri", "menu_shortcut", &with_shortcut);
    let mut want: Vec<String> = all.iter().map(|e| e.name().to_string()).collect();
    want.push("menu_shortcut".into());
    want.sort();
    assert_eq!(
        files("to-bateri"),
        want,
        "a fixture without a message, or the reverse"
    );
}

#[test]
fn agent_operations_and_outputs_are_fixtures_too() {
    let ops = [
        ("text", AgentOp::Text),
        ("snapshot", AgentOp::Snapshot),
        (
            "click-point",
            AgentOp::Click {
                target: Target::Point { x: 10.5, y: 20.0 },
            },
        ),
        (
            "type",
            AgentOp::Type {
                text: "hi".into(),
                target: Some(Target::Node { id: 3 }),
                enter: true,
            },
        ),
        ("shot", AgentOp::Shot),
    ];
    for (name, op) in &ops {
        check("agent-ops", name, op);
    }
    let outputs = [
        (
            "text",
            AgentOutput::Text {
                text: "t".into(),
                truncated: true,
            },
        ),
        (
            "png",
            AgentOutput::Png {
                data: "iVBORw0KGgo=".into(),
            },
        ),
        ("done", AgentOutput::Done),
        (
            "failed",
            AgentOutput::Failed {
                message: "no such node".into(),
            },
        ),
    ];
    for (name, output) in &outputs {
        check("agent-outputs", name, output);
    }
}

#[test]
fn the_release_notice_is_a_fixture() {
    let notice = Manifest {
        format: FORMAT,
        version: "0.1.0".into(),
        chromium: "156.0.7871.0".into(),
        protocol: PROTOCOL,
        minimum: "0.1.0".into(),
        assets: vec![Asset {
            arch: "arm64".into(),
            name: "bateri-browser-0.1.0-arm64.zip".into(),
            size: 123456789,
            sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        }],
    };
    check("manifest", "notice", &notice);
    let text = std::fs::read(dir("manifest").join("notice.json")).unwrap();
    assert_eq!(Manifest::parse(&text).unwrap(), notice);

    let mut bad = notice.clone();
    bad.assets[0].name = "../evil.zip".into();
    assert!(Manifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err());
    let mut bad = notice.clone();
    bad.assets[0].sha256 = "ABC".into();
    assert!(Manifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err());
    let mut bad = notice;
    bad.format = FORMAT + 1;
    assert!(Manifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err());
}

#[test]
fn a_message_round_trips_through_the_framing() {
    let mut wire = Vec::new();
    for c in commands() {
        write_message(&mut wire, &c).unwrap();
    }
    let mut input = wire.as_slice();
    for c in commands() {
        let back: Command = read_message(&mut input, 1 << 20).unwrap().unwrap();
        assert_eq!(back, c);
    }
    assert!(
        read_message::<_, Command>(&mut input, 1 << 20)
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_framing_refuses_what_is_too_big_or_cut_short() {
    // A length over the limit is refused before anything is read for it.
    let mut wire = Vec::new();
    wire.extend_from_slice(&(1u32 << 30).to_be_bytes());
    wire.extend_from_slice(b"{}");
    let e = read_message::<_, Command>(&mut wire.as_slice(), 1 << 20).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);

    // A body shorter than its length is an unexpected end, not a short message.
    let mut wire = Vec::new();
    wire.extend_from_slice(&100u32.to_be_bytes());
    wire.extend_from_slice(b"{\"t\":\"quit\"}");
    let e = read_message::<_, Command>(&mut wire.as_slice(), 1 << 20).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);

    // A cut length prefix is the same.
    let e = read_message::<_, Command>(&mut [0u8, 0].as_slice(), 1 << 20).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);

    // Not JSON of a message.
    let mut wire = Vec::new();
    wire.extend_from_slice(&2u32.to_be_bytes());
    wire.extend_from_slice(b"{}");
    let e = read_message::<_, Command>(&mut wire.as_slice(), 1 << 20).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn a_key_from_before_editing_commands_still_reads() {
    // The fixture from before `commands` existed stays as it was: no field, no commands.
    let plain = std::fs::read_to_string(dir("to-helper").join("key.json")).unwrap();
    assert!(!plain.contains("commands"), "{plain}");
    let key: Command = serde_json::from_str(&plain).unwrap();
    assert!(matches!(key, Command::Key { commands, .. } if commands.is_empty()));
}

#[test]
fn an_older_message_without_a_new_optional_field_still_reads() {
    let reload: Command = serde_json::from_str(r#"{"t":"reload","browser":1}"#).unwrap();
    assert_eq!(
        reload,
        Command::Reload {
            browser: 1,
            ignore_cache: false
        }
    );
}

#[test]
fn a_menu_row_without_shortcut_and_a_pinch_without_scale_still_read() {
    // The menu fixture from before `shortcut` existed stays as it was: no field, no keys.
    let plain = std::fs::read_to_string(dir("to-bateri").join("menu.json")).unwrap();
    assert!(!plain.contains("shortcut"), "{plain}");
    let Event::Menu { items, .. } = serde_json::from_str(&plain).unwrap() else {
        panic!("not a menu")
    };
    assert!(items.iter().all(|row| row.shortcut.is_empty()));
    // `Begin` and `End` say no change.
    let begin: Command =
        serde_json::from_str(r#"{"t":"pinch","browser":1,"kind":"begin","x":4,"y":5}"#).unwrap();
    assert_eq!(
        begin,
        Command::Pinch {
            browser: 1,
            kind: PinchKind::Begin,
            x: 4,
            y: 5,
            scale: 1.0
        }
    );
}

#[test]
fn the_side_buttons_of_a_mouse_go_by_their_names() {
    // `mouse.json` stays as it was (a right button): the two new buttons are values of the same field.
    for (name, want) in [("back", Button::Back), ("forward", Button::Forward)] {
        let text = format!(
            r#"{{"t":"mouse","browser":1,"kind":"up","x":1,"y":2,"button":"{name}","clicks":1,"mods":0}}"#
        );
        let Command::Mouse { button, .. } = serde_json::from_str(&text).unwrap() else {
            panic!("not a mouse")
        };
        assert_eq!(button, want);
        assert!(serde_json::to_string(&want).unwrap().contains(name));
    }
}

#[test]
fn an_event_kind_a_reader_does_not_know_is_an_error_to_it() {
    // Why `dialog_void` (added after the first fixtures, `PROTOCOL` still 1) needs a bateri built
    // at this revision: an older reader fails on the kind. Nothing was released at 1 yet.
    let void: Event = serde_json::from_str(r#"{"e":"dialog_void","browser":1,"req":4}"#).unwrap();
    assert_eq!(void.name(), "dialog_void");
    assert!(serde_json::from_str::<Event>(r#"{"e":"from_the_future","browser":1}"#).is_err());
}
