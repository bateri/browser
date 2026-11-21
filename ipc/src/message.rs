//! The messages bateri and the helper exchange on the control socket.
//!
//! Everything is plain data with JSON as its serialization, so a fixture file is the wire format.
//! Pixels do not travel here: frames go over mach (`crate::mach`). A message names its browser by
//! the number bateri chose in `Command::Open`.
//!
//! Evolution: a new optional field takes `#[serde(default)]` and keeps `PROTOCOL`; anything the
//! other side could misread bumps `PROTOCOL`, and the handshake refuses a different number.

use serde::{Deserialize, Serialize};

/// The conversation's version; both sides must report the same one in the handshake.
pub const PROTOCOL: u32 = 1;

/// Modifier bits of keys, the mouse and the wheel (own numbering, not Chromium's).
pub mod modifier {
    pub const SHIFT: u32 = 1;
    pub const CONTROL: u32 = 1 << 1;
    pub const ALT: u32 = 1 << 2;
    pub const COMMAND: u32 = 1 << 3;
    pub const CAPS_LOCK: u32 = 1 << 4;
    /// Mouse buttons held down: a move while one is down is a drag.
    pub const LEFT_BUTTON: u32 = 1 << 5;
    pub const MIDDLE_BUTTON: u32 = 1 << 6;
    pub const RIGHT_BUTTON: u32 = 1 << 7;
}

/// Where a profile's data lives and who it is to the proxy.
///
/// Browsers that carry the same `context` share one set of cookies and storage, and one proxy
/// identity: the identity belongs to the context, not to a browser (a second browser joining a
/// context is not asked for credentials again, so it could not carry its own).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Profile {
    pub context: String,
    /// A folder name under the helper's `--root`; `None` keeps the profile in memory, gone when
    /// its last browser closes. One path component: `[A-Za-z0-9._-]`, not starting with a dot.
    pub dir: Option<String>,
    /// The proxy credentials this context answers a 407 with.
    pub user: String,
    pub pass: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Move,
    Leave,
    Down,
    Up,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    /// A key went down (not text).
    Down,
    Up,
    /// A character the key produced.
    Char,
}

/// A key as the system gave it: `vk` is the Windows virtual-key code Chromium keys on, `native`
/// the macOS key code, `character` and `unmodified` UTF-16 units.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct KeyData {
    pub vk: i32,
    pub native: i32,
    pub character: u16,
    pub unmodified: u16,
    pub mods: u32,
    /// The command key is down (what Chromium calls a system key).
    pub system: bool,
}

/// What an agent points at.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A node reference a snapshot printed (`[12]`).
    Node { id: i64 },
    /// A point in the page's own pixels (CSS px from the top-left of the view).
    Point { x: f64, y: f64 },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentOp {
    /// The page's visible text.
    Text,
    /// The accessibility tree as an outline whose lines carry node references.
    Snapshot,
    Click {
        target: Target,
    },
    /// Types into the focused element, or into `target` after focusing it.
    Type {
        text: String,
        #[serde(default)]
        target: Option<Target>,
        /// Press Return afterwards.
        #[serde(default)]
        enter: bool,
    },
    /// A PNG of the page.
    Shot,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOutput {
    Text {
        text: String,
        /// The helper cut the text at its size cap.
        truncated: bool,
    },
    /// Base64 of the PNG.
    Png {
        data: String,
    },
    Done,
    Failed {
        message: String,
    },
}

/// What bateri tells the helper.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Command {
    /// Creates a browser. `width` and `height` are the view's size in points, `scale` the display's
    /// pixel ratio; frames come back in device pixels (points times scale).
    Open {
        browser: u32,
        profile: Profile,
        width: u32,
        height: u32,
        scale: f32,
    },
    Close {
        browser: u32,
    },
    /// Loads `url`; only `http`, `https`, `about:blank` and `data:` are taken (`Event::Refused`
    /// says no to the rest).
    Navigate {
        browser: u32,
        url: String,
    },
    Back {
        browser: u32,
    },
    Forward {
        browser: u32,
    },
    Reload {
        browser: u32,
        #[serde(default)]
        ignore_cache: bool,
    },
    Stop {
        browser: u32,
    },
    Resize {
        browser: u32,
        width: u32,
        height: u32,
        scale: f32,
    },
    /// The browser gains or loses the keyboard.
    Focus {
        browser: u32,
        on: bool,
    },
    Mouse {
        browser: u32,
        kind: MouseKind,
        x: i32,
        y: i32,
        button: Button,
        /// Click count for `Down` and `Up`.
        clicks: u32,
        mods: u32,
    },
    Wheel {
        browser: u32,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        mods: u32,
    },
    Key {
        browser: u32,
        kind: KeyKind,
        key: KeyData,
    },
    /// Text the input method committed as a whole.
    ImeCommit {
        browser: u32,
        text: String,
    },
    /// Marked text; `from`..`to` is the selection inside it, in UTF-16 units.
    ImeCompose {
        browser: u32,
        text: String,
        from: u32,
        to: u32,
    },
    ImeCancel {
        browser: u32,
    },
    Find {
        browser: u32,
        pattern: String,
        case: bool,
        /// `false` searches backwards.
        forward: bool,
        /// Continue the search that is already running instead of starting one.
        next: bool,
    },
    StopFinding {
        browser: u32,
        /// Leave the match selected.
        keep_selection: bool,
    },
    /// Page zoom as a ratio, 1.0 is 100 %.
    Zoom {
        browser: u32,
        ratio: f64,
    },
    /// Opens the developer tools in a window of their own.
    DevTools {
        browser: u32,
    },
    Agent {
        browser: u32,
        req: u32,
        op: AgentOp,
    },
    /// The answer to `Event::Menu`: the chosen command, `None` for dismissed.
    MenuResult {
        req: u32,
        command: Option<i32>,
    },
    /// The answer to `Event::Dialog`.
    DialogResult {
        req: u32,
        ok: bool,
        text: String,
    },
    /// The answer to `Event::FileDialog`; empty means cancelled.
    FileResult {
        req: u32,
        paths: Vec<String>,
    },
    /// The answer to `Event::Download`: where to save it, `None` to refuse. Nothing is written
    /// before this arrives.
    DownloadResult {
        req: u32,
        path: Option<String>,
    },
    DownloadCancel {
        req: u32,
    },
    /// Shuts the helper down in order (profiles are flushed).
    Quit,
}

/// The shape of the mouse pointer the page asks for.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    Arrow,
    Cross,
    Hand,
    IBeam,
    Wait,
    Progress,
    Help,
    Move,
    NotAllowed,
    Grab,
    Grabbing,
    ZoomIn,
    ZoomOut,
    Copy,
    Alias,
    Cell,
    ContextMenu,
    VerticalText,
    ColumnResize,
    RowResize,
    ResizeEastWest,
    ResizeNorthSouth,
    ResizeNorthEastSouthWest,
    ResizeNorthWestSouthEast,
    Hidden,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LoadError {
    pub url: String,
    pub code: i32,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub count: u32,
    /// 1-based index of the active match, 0 when there is none.
    pub current: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MenuItem {
    /// What `Command::MenuResult` answers with.
    pub command: i32,
    pub label: String,
    pub enabled: bool,
    pub checked: bool,
    pub separator: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    /// The "leave this page?" question.
    BeforeUnload,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    Open,
    OpenMultiple,
    Folder,
    Save,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Running,
    Done,
    Canceled,
}

/// What the helper tells bateri.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum Event {
    /// The first message, before anything else: the single-use key bateri started the helper with
    /// proves this is that helper.
    Hello {
        protocol: u32,
        /// The helper's own version.
        version: String,
        /// The Chromium it embeds.
        chromium: String,
        pid: u32,
        key: String,
    },
    /// The browser exists; its first state follows.
    Opened {
        browser: u32,
    },
    Closed {
        browser: u32,
    },
    /// The whole state of a browser, sent on every change.
    State {
        browser: u32,
        url: String,
        title: String,
        loading: bool,
        can_back: bool,
        can_forward: bool,
        /// The link under the pointer, as Chromium's status bubble shows it.
        status: String,
        zoom: f64,
        error: Option<LoadError>,
        found: Option<Found>,
    },
    Cursor {
        browser: u32,
        cursor: Cursor,
    },
    /// A popup (a `<select>` list) shows or hides; its frames carry `KIND_POPUP`. `x`, `y`, `width`
    /// and `height` are in device pixels of the view.
    Popup {
        browser: u32,
        show: bool,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    /// The renderer died; only this browser is gone.
    Crashed {
        browser: u32,
        reason: String,
    },
    /// A navigation the helper did not allow (`reason` is `"scheme"`).
    Refused {
        browser: u32,
        url: String,
        reason: String,
    },
    /// The page asked for a new window; the helper opened none.
    Window {
        browser: u32,
        url: String,
        background: bool,
    },
    /// A key neither the page nor the engine used.
    UnhandledKey {
        browser: u32,
        kind: KeyKind,
        key: KeyData,
    },
    Menu {
        browser: u32,
        req: u32,
        x: i32,
        y: i32,
        items: Vec<MenuItem>,
        link: Option<String>,
        source: Option<String>,
        selection: Option<String>,
        editable: bool,
    },
    Dialog {
        browser: u32,
        req: u32,
        kind: DialogKind,
        origin: String,
        message: String,
        default_text: String,
    },
    FileDialog {
        browser: u32,
        req: u32,
        mode: FileMode,
        title: String,
        default_path: String,
        accept: Vec<String>,
    },
    Download {
        browser: u32,
        req: u32,
        url: String,
        name: String,
        mime: String,
        size: Option<u64>,
    },
    DownloadProgress {
        req: u32,
        state: DownloadState,
        received: u64,
        total: Option<u64>,
    },
    AgentResult {
        browser: u32,
        req: u32,
        output: AgentOutput,
    },
    /// Something the helper could not do; `browser` is set when one browser's.
    Error {
        browser: Option<u32>,
        message: String,
    },
}

impl Command {
    /// The browser a command is about; `None` for the ones that are not (`Quit`, and the answers,
    /// which name a request instead).
    pub fn browser(&self) -> Option<u32> {
        match self {
            Command::Open { browser, .. }
            | Command::Close { browser }
            | Command::Navigate { browser, .. }
            | Command::Back { browser }
            | Command::Forward { browser }
            | Command::Reload { browser, .. }
            | Command::Stop { browser }
            | Command::Resize { browser, .. }
            | Command::Focus { browser, .. }
            | Command::Mouse { browser, .. }
            | Command::Wheel { browser, .. }
            | Command::Key { browser, .. }
            | Command::ImeCommit { browser, .. }
            | Command::ImeCompose { browser, .. }
            | Command::ImeCancel { browser }
            | Command::Find { browser, .. }
            | Command::StopFinding { browser, .. }
            | Command::Zoom { browser, .. }
            | Command::DevTools { browser }
            | Command::Agent { browser, .. } => Some(*browser),
            Command::MenuResult { .. }
            | Command::DialogResult { .. }
            | Command::FileResult { .. }
            | Command::DownloadResult { .. }
            | Command::DownloadCancel { .. }
            | Command::Quit => None,
        }
    }

    /// The fixture name of this message kind.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Open { .. } => "open",
            Command::Close { .. } => "close",
            Command::Navigate { .. } => "navigate",
            Command::Back { .. } => "back",
            Command::Forward { .. } => "forward",
            Command::Reload { .. } => "reload",
            Command::Stop { .. } => "stop",
            Command::Resize { .. } => "resize",
            Command::Focus { .. } => "focus",
            Command::Mouse { .. } => "mouse",
            Command::Wheel { .. } => "wheel",
            Command::Key { .. } => "key",
            Command::ImeCommit { .. } => "ime_commit",
            Command::ImeCompose { .. } => "ime_compose",
            Command::ImeCancel { .. } => "ime_cancel",
            Command::Find { .. } => "find",
            Command::StopFinding { .. } => "stop_finding",
            Command::Zoom { .. } => "zoom",
            Command::DevTools { .. } => "dev_tools",
            Command::Agent { .. } => "agent",
            Command::MenuResult { .. } => "menu_result",
            Command::DialogResult { .. } => "dialog_result",
            Command::FileResult { .. } => "file_result",
            Command::DownloadResult { .. } => "download_result",
            Command::DownloadCancel { .. } => "download_cancel",
            Command::Quit => "quit",
        }
    }
}

impl Event {
    /// The fixture name of this message kind.
    pub fn name(&self) -> &'static str {
        match self {
            Event::Hello { .. } => "hello",
            Event::Opened { .. } => "opened",
            Event::Closed { .. } => "closed",
            Event::State { .. } => "state",
            Event::Cursor { .. } => "cursor",
            Event::Popup { .. } => "popup",
            Event::Crashed { .. } => "crashed",
            Event::Refused { .. } => "refused",
            Event::Window { .. } => "window",
            Event::UnhandledKey { .. } => "unhandled_key",
            Event::Menu { .. } => "menu",
            Event::Dialog { .. } => "dialog",
            Event::FileDialog { .. } => "file_dialog",
            Event::Download { .. } => "download",
            Event::DownloadProgress { .. } => "download_progress",
            Event::AgentResult { .. } => "agent_result",
            Event::Error { .. } => "error",
        }
    }

    /// The browser this event is about, when it is about one.
    pub fn browser(&self) -> Option<u32> {
        match self {
            Event::Hello { .. } | Event::DownloadProgress { .. } => None,
            Event::Error { browser, .. } => *browser,
            Event::Opened { browser }
            | Event::Closed { browser }
            | Event::State { browser, .. }
            | Event::Cursor { browser, .. }
            | Event::Popup { browser, .. }
            | Event::Crashed { browser, .. }
            | Event::Refused { browser, .. }
            | Event::Window { browser, .. }
            | Event::UnhandledKey { browser, .. }
            | Event::Menu { browser, .. }
            | Event::Dialog { browser, .. }
            | Event::FileDialog { browser, .. }
            | Event::Download { browser, .. }
            | Event::AgentResult { browser, .. } => Some(*browser),
        }
    }
}
