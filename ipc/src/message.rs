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

/// What a profile's data is made of, at the grain it is kept and sent in: one cookie, one item of
/// one origin's Local Storage, or one origin's whole IndexedDB folder. A record with no `value`
/// says the thing is gone (a tombstone). Neither side reads into a value more than it needs: the
/// profile's home keeps records and tells them apart by their key (`kind` plus the key's fields).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Cookie {
        /// As the engine writes it: a cookie of a domain (not only of a host) starts with a dot.
        domain: String,
        path: String,
        name: String,
        /// The site a partitioned cookie is kept under; `None` is a cookie of no partition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partition: Option<Partition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<CookieValue>,
    },
    /// An item of Local Storage. `origin` is the engine's own name for the storage's owner
    /// (`https://example.com/`), only ever handed back to it.
    LocalStorage {
        origin: String,
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    /// The folder of an origin's IndexedDB, whole: the files are in the stage the message names.
    IndexedDb {
        /// The folder's own name for the origin (`https_example.com_0`).
        origin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<IndexedDbValue>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Partition {
    /// The top-level site the cookie is kept under (`https://example.com`).
    pub site: String,
    /// The cookie was set in a frame that has a cross-site ancestor.
    #[serde(default)]
    pub cross_site: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CookieValue {
    pub value: String,
    /// Seconds since the epoch; `None` is a cookie of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// 0 unspecified, 1 none, 2 lax, 3 strict.
    #[serde(default)]
    pub same_site: u8,
    /// 0 low, 1 medium, 2 high.
    #[serde(default = "medium")]
    pub priority: u8,
    /// 0 unset, 1 non-secure, 2 secure.
    #[serde(default)]
    pub source_scheme: u8,
    /// The port the cookie was set from; -1 is none known.
    #[serde(default = "no_port")]
    pub source_port: i32,
}

fn medium() -> u8 {
    1
}

fn no_port() -> i32 {
    -1
}

/// What an origin's IndexedDB folder is, as the engine that wrote it names it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct IndexedDbValue {
    /// The storage format (`chromium-idb-leveldb`): an engine that does not read it keeps its own.
    pub format: String,
    /// The engine's major version.
    pub major: u16,
    /// The files, by their path below the origin's folder in the stage (`leveldb/000003.log`,
    /// `blob/1/00/1`): the names are the engine's, the bytes are the stage's.
    pub files: Vec<IndexedDbFile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct IndexedDbFile {
    pub path: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Move,
    Leave,
    Down,
    Up,
}

/// What a link or an image the page's menu opens becomes on bateri's side: a tab of the page
/// (`Page`), or the same browser going there (`Here`). The helper asks for a window the same way
/// in both (`Event::Window`); what it needs to know is the words of the menu row, "Open Link in New
/// Tab" or "Open Link".
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NewWindows {
    #[default]
    Page,
    Here,
}

/// The three parts of a trackpad pinch ([`Command::Pinch`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PinchKind {
    Begin,
    Update,
    End,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Middle,
    Right,
    /// The mouse's side button that goes back (`button` 3 of a `mouseup`).
    Back,
    /// The mouse's side button that goes forward (`button` 4).
    Forward,
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

/// What the Edit menu asks of the focused element.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditAction {
    Copy,
    Cut,
    Paste,
    Undo,
    Redo,
    SelectAll,
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
    /// Asks whether a sideways scroll at `(x, y)` would move something (the page, or a box under
    /// the point): the answer is `Event::CanScroll`. `dx` is a wheel's (`Wheel::dx`) and only its
    /// sign is read: it says which way the content would move. This is what tells a two-finger
    /// swipe that belongs to the page from one that goes back or forward.
    CanScroll {
        browser: u32,
        req: u32,
        x: i32,
        y: i32,
        dx: i32,
    },
    /// How bateri takes a window the page or its menu asks for (`Event::Window`). Until it is said
    /// the helper words the menu for `NewWindows::Page`; say it again when it changes.
    Windows {
        browser: u32,
        mode: NewWindows,
    },
    /// A trackpad pinch, as `magnifyWithEvent:` reports it: one `Begin`, any number of `Update`s
    /// and one `End`. `x`, `y` are where the fingers are (points from the view's corner), `scale`
    /// of an `Update` is the change since the one before it (1.05: the fingers spread by 5 %;
    /// 1.0 in `Begin` and `End`).
    ///
    /// The page has it first, as in Chrome on a Mac: every `Update` reaches it as a `wheel` event
    /// with `ctrlKey` set and a `deltaY` of `-100 * ln(scale)` (negative to zoom in), which a map
    /// or a design canvas takes for its own zoom. If the page does not call `preventDefault` on
    /// the first of them, the pinch zooms the page visually: the visual viewport scales (1 to 5;
    /// the layout does not change, `visualViewport.scale` says how far), the point between the
    /// fingers stays where it is, and where the page is clicked afterwards is where it was
    /// aimed. It is not the page zoom of `Zoom` (⌘+). The zoom stays after `End` until the page
    /// is left.
    Pinch {
        browser: u32,
        kind: PinchKind,
        x: i32,
        y: i32,
        #[serde(default = "no_change")]
        scale: f64,
    },
    /// A key. `commands` are the macOS editing commands the system's key bindings made of it
    /// (`interpretKeyEvents` then `doCommandBySelector:`, the selector's name without the colon:
    /// `moveToBeginningOfLine`, `deleteToBeginningOfLine`, `selectAll`). They go to the page with
    /// the key and run only when the page lets the key's default happen and something can take
    /// the command, like in Chrome: ⌘← in a text field moves the caret and is not reported back,
    /// outside one it comes back as `Event::UnhandledKey`. Only `Down` carries them.
    Key {
        browser: u32,
        kind: KeyKind,
        key: KeyData,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        commands: Vec<String>,
    },
    /// An editing command of the Edit menu, for the focused element. A key is not the way to ask
    /// for these: the page would have to bind them itself, and the clipboard is the system's.
    Edit {
        browser: u32,
        action: EditAction,
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
    /// Reads what the browser's profile holds in the engine: its cookies and the Local Storage of
    /// the origins that have a page open. Answered by `Event::ProfileRecords`. What it reads is
    /// the base `ProfileWatch` reports changes from.
    ProfileRead {
        browser: u32,
        req: u32,
    },
    /// Puts records into the browser's profile (what comes from elsewhere): cookies at once, Local
    /// Storage at once where a page of the origin is open and before the first page of it
    /// otherwise. Answered by `Event::ProfileWritten`, after which a navigation sees all of it.
    /// What is written is not reported back by `ProfileWatch`. `IndexedDb` records are not taken
    /// here (`IndexedDbWrite`).
    ProfileWrite {
        browser: u32,
        req: u32,
        records: Vec<Record>,
    },
    /// Starts (or stops) `Event::ProfileChanged` for the browser's profile.
    ProfileWatch {
        browser: u32,
        on: bool,
    },
    /// Copies each origin's IndexedDB folder of the profile in folder `dir` into a stage of the
    /// helper's own and answers with a record for each (`Event::IndexedDbRecords`). Needs no
    /// browser: the folder is read as it lies.
    IndexedDbRead {
        req: u32,
        dir: String,
    },
    /// Replaces origins' IndexedDB folders with the files of `stage` (bateri made it below the
    /// helper's stage folder). Only a profile whose engine is not running takes it: the engine
    /// keeps an opened store and does not see a folder put under it, so a running profile's
    /// records are handed back as deferred. Answered by `Event::IndexedDbWritten`.
    IndexedDbWrite {
        req: u32,
        dir: String,
        stage: String,
        records: Vec<Record>,
    },
    /// The helper may delete a stage it named in an event or bateri made for `IndexedDbWrite`.
    StageDrop {
        stage: String,
    },
    /// Shuts the helper down in order (profiles are flushed).
    Quit,
}

fn no_change() -> f64 {
    1.0
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
    /// The keys that do the same from the keyboard, as the Mac writes them in a menu (`⌘+`,
    /// `⌥⌘I`); empty for a row that has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shortcut: String,
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
    /// A context menu. Rows of the helper's own are among the engine's: for a link "Open Link in
    /// New Tab" and "Copy Link Address", for an image "Open Image in New Tab" and "Copy Image
    /// Address" (each group first, then a separator), and at the end the zoom rows and Inspect
    /// (`MenuItem::shortcut` carries their keys). `MenuResult` answers with the number of a row;
    /// the helper runs its own, so what a row opens goes back as `Event::Window` with
    /// `background` set, wherever bateri puts it (`Command::Windows`).
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
    /// The page moved on (it navigated) while a `Dialog` was open: that question is void and
    /// nobody is waiting for its answer any more; a `DialogResult` for it is ignored. bateri
    /// takes the question down.
    DialogVoid {
        browser: u32,
        req: u32,
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
    /// The answer to `Command::CanScroll`: `can` says something there takes the scroll. A page
    /// that could not be asked answers `true` (it keeps its gesture).
    CanScroll {
        browser: u32,
        req: u32,
        can: bool,
    },
    /// The answer to `Command::ProfileRead`; `error` says why `records` is not the whole of it.
    ProfileRecords {
        browser: u32,
        req: u32,
        records: Vec<Record>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The answer to `Command::ProfileWrite`: every record is in the profile except `deferred`
    /// of them (Local Storage waiting for the first page of its origin).
    ProfileWritten {
        browser: u32,
        req: u32,
        #[serde(default)]
        deferred: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// What the profile gained, changed or lost since the base and since the last of these, once
    /// `Command::ProfileWatch` is on. An `IndexedDb` record's files are in `stage`, which bateri
    /// frees with `Command::StageDrop`.
    ProfileChanged {
        browser: u32,
        records: Vec<Record>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
    },
    /// The answer to `Command::IndexedDbRead`: the files of the records are in `stage`, which
    /// bateri frees with `Command::StageDrop`.
    IndexedDbRecords {
        req: u32,
        records: Vec<Record>,
        stage: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The answer to `Command::IndexedDbWrite`: `written` records are in the folder, `deferred`
    /// are not (the engine of the profile is running, or the records are in a format this
    /// engine does not read).
    IndexedDbWritten {
        req: u32,
        written: u32,
        deferred: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
            | Command::CanScroll { browser, .. }
            | Command::Windows { browser, .. }
            | Command::Pinch { browser, .. }
            | Command::Key { browser, .. }
            | Command::Edit { browser, .. }
            | Command::ImeCommit { browser, .. }
            | Command::ImeCompose { browser, .. }
            | Command::ImeCancel { browser }
            | Command::Find { browser, .. }
            | Command::StopFinding { browser, .. }
            | Command::Zoom { browser, .. }
            | Command::DevTools { browser }
            | Command::ProfileRead { browser, .. }
            | Command::ProfileWrite { browser, .. }
            | Command::ProfileWatch { browser, .. }
            | Command::Agent { browser, .. } => Some(*browser),
            Command::MenuResult { .. }
            | Command::DialogResult { .. }
            | Command::FileResult { .. }
            | Command::DownloadResult { .. }
            | Command::DownloadCancel { .. }
            | Command::IndexedDbRead { .. }
            | Command::IndexedDbWrite { .. }
            | Command::StageDrop { .. }
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
            Command::CanScroll { .. } => "can_scroll",
            Command::Windows { .. } => "windows",
            Command::Pinch { .. } => "pinch",
            Command::Key { .. } => "key",
            Command::Edit { .. } => "edit",
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
            Command::ProfileRead { .. } => "profile_read",
            Command::ProfileWrite { .. } => "profile_write",
            Command::ProfileWatch { .. } => "profile_watch",
            Command::IndexedDbRead { .. } => "indexed_db_read",
            Command::IndexedDbWrite { .. } => "indexed_db_write",
            Command::StageDrop { .. } => "stage_drop",
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
            Event::DialogVoid { .. } => "dialog_void",
            Event::FileDialog { .. } => "file_dialog",
            Event::Download { .. } => "download",
            Event::DownloadProgress { .. } => "download_progress",
            Event::AgentResult { .. } => "agent_result",
            Event::CanScroll { .. } => "can_scroll",
            Event::ProfileRecords { .. } => "profile_records",
            Event::ProfileWritten { .. } => "profile_written",
            Event::ProfileChanged { .. } => "profile_changed",
            Event::IndexedDbRecords { .. } => "indexed_db_records",
            Event::IndexedDbWritten { .. } => "indexed_db_written",
            Event::Error { .. } => "error",
        }
    }

    /// The browser this event is about, when it is about one.
    pub fn browser(&self) -> Option<u32> {
        match self {
            Event::Hello { .. }
            | Event::DownloadProgress { .. }
            | Event::IndexedDbRecords { .. }
            | Event::IndexedDbWritten { .. } => None,
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
            | Event::DialogVoid { browser, .. }
            | Event::FileDialog { browser, .. }
            | Event::Download { browser, .. }
            | Event::AgentResult { browser, .. }
            | Event::ProfileRecords { browser, .. }
            | Event::ProfileWritten { browser, .. }
            | Event::ProfileChanged { browser, .. }
            | Event::CanScroll { browser, .. } => Some(*browser),
        }
    }
}
