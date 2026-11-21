//! The helper's own rules, as plain functions: what may be loaded, what a profile folder may be
//! called, how a zoom ratio maps to Chromium's zoom level. No engine in here, so they are tested
//! without one.

use browser_ipc::message::{MouseKind, NewWindows};

/// The longest address the helper takes.
pub const MAX_ADDRESS: usize = 8192;

/// Why an address was not taken.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The scheme is not `http`, `https`, `about:blank` or `data:`.
    Scheme,
    /// Too long, or it has whitespace or something that is not printable ASCII.
    Address,
}

impl Refusal {
    pub fn reason(&self) -> &'static str {
        match self {
            Refusal::Scheme => "scheme",
            Refusal::Address => "address",
        }
    }
}

/// Whether a navigation requested through `Command::Navigate` is taken.
pub fn check_address(url: &str) -> Result<(), Refusal> {
    if url.len() > MAX_ADDRESS || !url.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(Refusal::Address);
    }
    if scheme_allowed(url, true) {
        Ok(())
    } else {
        Err(Refusal::Scheme)
    }
}

/// Whether the engine may navigate a frame to `url` (redirects and links included).
///
/// The set is `http`, `https`, `about:blank` and `data:`. A frame inside a page may also be
/// `about:srcdoc` or a `blob:` the page made itself: those carry the page's own origin and no
/// address of anyone else's, and an ordinary page uses them.
pub fn scheme_allowed(url: &str, main_frame: bool) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:")
        || lower == "about:blank"
        || lower.starts_with("about:blank#")
    {
        return true;
    }
    !main_frame && (lower == "about:srcdoc" || lower.starts_with("blob:"))
}

/// What bateri is told when a page asks for a new window (or the developer tools' page does): the
/// address when the helper loads it, `Refused` when it does not.
pub fn window_event(browser: u32, url: &str, background: bool) -> browser_ipc::message::Event {
    use browser_ipc::message::Event;
    if scheme_allowed(url, true) {
        Event::Window {
            browser,
            url: url.into(),
            background,
        }
    } else {
        Event::Refused {
            browser,
            url: url.into(),
            reason: Refusal::Scheme.reason().into(),
        }
    }
}

/// Whether a download from `url` is taken. The same set as a page's own frames: what the helper
/// loads, and the `blob:` a page makes for itself (the ordinary way a page saves what it
/// produced; it names no address of anyone else's).
pub fn download_allowed(url: &str) -> bool {
    scheme_allowed(url, false)
}

/// Whether a frame of the developer tools' own window may go to `url`. The tools' page is
/// `devtools://`, and nothing else is ever put in its main frame; inside it, the helper's own
/// set is as good as it is anywhere else.
pub fn devtools_frame_allowed(url: &str, main_frame: bool) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("devtools://") || (!main_frame && scheme_allowed(url, false))
}

/// What an update of the engine's download item says to bateri. A download that broke off is over,
/// whatever the engine does about it: it keeps such an item "in progress" while it tries to
/// resume it (measured: `is_interrupted` stays false, the item carries the reason it broke off
/// and the received count goes back and forth), and bateri was not asked to wait for that. The
/// way to tell is the reason (`broke_off`) or the state (`interrupted`).
pub fn download_state(
    complete: bool,
    canceled: bool,
    interrupted: bool,
    broke_off: bool,
) -> browser_ipc::message::DownloadState {
    use browser_ipc::message::DownloadState;
    if complete {
        DownloadState::Done
    } else if canceled || interrupted || broke_off {
        DownloadState::Canceled
    } else {
        DownloadState::Running
    }
}

/// Whether a menu answer names a command the menu held: only those are run, so what the engine
/// has behind its other command numbers (opening `view-source:` among them) cannot be asked for.
pub fn menu_command_offered(offered: &[i32], command: i32) -> bool {
    offered.contains(&command)
}

/// The answer to a question that is still open when its browser closes: the "leave this page?"
/// question is answered yes (nothing is kept the page would have lost), every other one no.
pub fn close_answer(kind: browser_ipc::message::DialogKind) -> bool {
    kind == browser_ipc::message::DialogKind::BeforeUnload
}

/// A profile folder name: one path component, nothing that climbs out or hides.
pub fn profile_dir_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A menu label without its mnemonic marker: the engine's labels carry the Windows convention
/// (`&Back`, `Save &as...`, a literal ampersand written `&&`), which a Mac menu does not show.
pub fn menu_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Zoom ratios the engine is asked for are held to this range.
pub const ZOOM_MIN: f64 = 0.25;
pub const ZOOM_MAX: f64 = 5.0;

/// The page zooms Chrome steps through (its own presets), 25 % to 500 %.
const ZOOM_PRESETS: [f64; 17] = [
    0.25,
    1.0 / 3.0,
    0.5,
    2.0 / 3.0,
    0.75,
    0.8,
    0.9,
    1.0,
    1.1,
    1.25,
    1.5,
    1.75,
    2.0,
    2.5,
    3.0,
    4.0,
    5.0,
];

/// The next of Chrome's preset zooms above `ratio` (`up`) or below it, `None` at the end of the
/// list. A zoom between two presets (one `Command::Zoom` set) goes to the nearer one on its side.
pub fn zoom_step(ratio: f64, up: bool) -> Option<f64> {
    const SAME: f64 = 0.001;
    if up {
        ZOOM_PRESETS.into_iter().find(|p| *p > ratio + SAME)
    } else {
        ZOOM_PRESETS.into_iter().rev().find(|p| *p < ratio - SAME)
    }
}

/// The rows the helper adds to the page's context menu, and the numbers they answer with: the
/// engine keeps `MENU_ID_USER_FIRST` and up for the embedder, and the helper runs these itself
/// (`MenuResult` is answered with one of them and the engine is never asked). The zoom rows and
/// Inspect go at the end; the rows of a link and of an image at the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnRow {
    ZoomIn,
    ZoomOut,
    ActualSize,
    Inspect,
    OpenLink,
    CopyLink,
    OpenImage,
    CopyImage,
}

/// `MENU_ID_USER_FIRST` of the engine (a test holds the two together).
pub const OWN_FIRST: i32 = 26500;

impl OwnRow {
    /// In the order of their numbers: a new row goes last, so that no number moves.
    pub const ALL: [OwnRow; 8] = [
        OwnRow::ZoomIn,
        OwnRow::ZoomOut,
        OwnRow::ActualSize,
        OwnRow::Inspect,
        OwnRow::OpenLink,
        OwnRow::CopyLink,
        OwnRow::OpenImage,
        OwnRow::CopyImage,
    ];

    pub fn command(self) -> i32 {
        OWN_FIRST
            + Self::ALL
                .iter()
                .position(|row| *row == self)
                .unwrap_or_default() as i32
    }

    pub fn of(command: i32) -> Option<OwnRow> {
        Self::ALL.into_iter().find(|row| row.command() == command)
    }

    /// The words of the row. What opens is "in New Tab" where bateri opens a tab for it, and plain
    /// where the browser goes there itself.
    pub fn label(self, windows: NewWindows) -> &'static str {
        match (self, windows) {
            (OwnRow::ZoomIn, _) => "Zoom In",
            (OwnRow::ZoomOut, _) => "Zoom Out",
            (OwnRow::ActualSize, _) => "Actual Size",
            (OwnRow::Inspect, _) => "Inspect",
            (OwnRow::OpenLink, NewWindows::Page) => "Open Link in New Tab",
            (OwnRow::OpenLink, NewWindows::Here) => "Open Link",
            (OwnRow::CopyLink, _) => "Copy Link Address",
            (OwnRow::OpenImage, NewWindows::Page) => "Open Image in New Tab",
            (OwnRow::OpenImage, NewWindows::Here) => "Open Image",
            (OwnRow::CopyImage, _) => "Copy Image Address",
        }
    }

    /// The keys that do the same from the keyboard (bateri's own menu has them); none for the rows
    /// that have none.
    pub fn shortcut(self) -> &'static str {
        match self {
            OwnRow::ZoomIn => "⌘+",
            OwnRow::ZoomOut => "⌘\u{2212}",
            OwnRow::ActualSize => "⌘0",
            OwnRow::Inspect => "⌥⌘I",
            _ => "",
        }
    }

    /// Whether the row can be run at a page zoom of `ratio`: the zoom rows only where they have
    /// somewhere to go.
    pub fn enabled(self, ratio: f64) -> bool {
        match self {
            OwnRow::ZoomIn => zoom_step(ratio, true).is_some(),
            OwnRow::ZoomOut => zoom_step(ratio, false).is_some(),
            OwnRow::ActualSize => (ratio - 1.0).abs() > 0.001,
            _ => true,
        }
    }
}

/// What a menu was asked for: the addresses the rows of a link and of an image work on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuUrls {
    /// The link under the pointer as the engine vouches for it: what is opened.
    pub open_link: String,
    /// The link as the page wrote it: what is copied.
    pub copy_link: String,
    /// The address of the image under the pointer; empty where the element is no image.
    pub image: String,
}

impl MenuUrls {
    /// The groups of rows that start the menu, each followed by a line: a link's, then an image's.
    /// A row that opens is there only for an address the helper loads (`check_address`: a link to
    /// anything else would be refused as the window it asks for), and for a link only if the page's
    /// own address is one too: the engine puts `about:blank#blocked` in the place of a `javascript:`
    /// link it will not vouch for, and that is no link to open. A row that copies is there for any
    /// address, since what it does is write the text.
    pub fn groups(&self) -> Vec<Vec<OwnRow>> {
        let group = |open: &str, copy: &str, rows: [OwnRow; 2]| {
            let mut group = Vec::new();
            if check_address(open).is_ok() && check_address(copy).is_ok() {
                group.push(rows[0]);
            }
            if !copy.is_empty() {
                group.push(rows[1]);
            }
            group
        };
        [
            group(
                &self.open_link,
                &self.copy_link,
                [OwnRow::OpenLink, OwnRow::CopyLink],
            ),
            group(
                &self.image,
                &self.image,
                [OwnRow::OpenImage, OwnRow::CopyImage],
            ),
        ]
        .into_iter()
        .filter(|rows| !rows.is_empty())
        .collect()
    }

    /// The address a row of a link or of an image works on.
    pub fn of(&self, row: OwnRow) -> Option<&str> {
        match row {
            OwnRow::OpenLink => Some(&self.open_link),
            OwnRow::CopyLink => Some(&self.copy_link),
            OwnRow::OpenImage | OwnRow::CopyImage => Some(&self.image),
            _ => None,
        }
        .map(String::as_str)
        .filter(|url| !url.is_empty())
    }
}

/// Chromium's zoom level steps by powers of 1.2.
pub fn ratio_to_level(ratio: f64) -> f64 {
    ratio.clamp(ZOOM_MIN, ZOOM_MAX).ln() / 1.2f64.ln()
}

pub fn level_to_ratio(level: f64) -> f64 {
    1.2f64.powf(level)
}

/// A view's size in points and its scale, held to what a surface can be.
pub fn clamp_view(width: u32, height: u32, scale: f32) -> (i32, i32, f32) {
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        1.0
    };
    (
        width.clamp(1, 8192) as i32,
        height.clamp(1, 8192) as i32,
        scale,
    )
}

/// The protocol call for a side button of the mouse (`back` goes back, otherwise forward) at the
/// point `(x, y)` of the page in CSS pixels. The engine's own mouse calls know three buttons; its
/// protocol knows five, and with it the page gets `mousedown`, `mouseup` and `auxclick` with `button`
/// 3 or 4 and the engine goes back or forward when the button is let go unless the page called
/// `preventDefault` on that `mouseup` (or `pointerup`): what Chrome does, measured. A move has no
/// button of this kind.
pub fn side_button_call(kind: MouseKind, back: bool, x: f64, y: f64) -> Option<serde_json::Value> {
    let (kind, buttons) = match kind {
        // `buttons` is the set held: 8 and 16 are the fourth and fifth, and none after the release.
        MouseKind::Down => ("mousePressed", if back { 8 } else { 16 }),
        MouseKind::Up => ("mouseReleased", 0),
        MouseKind::Move | MouseKind::Leave => return None,
    };
    Some(serde_json::json!({
        "type": kind,
        "x": x,
        "y": y,
        "button": if back { "back" } else { "forward" },
        "buttons": buttons,
        "clickCount": 1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_lose_their_mnemonics() {
        assert_eq!(menu_label("&Back"), "Back");
        assert_eq!(menu_label("Save &as…"), "Save as…");
        assert_eq!(menu_label("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(menu_label("Print…"), "Print…");
        assert_eq!(menu_label(""), "");
    }

    #[test]
    fn the_scheme_set_is_closed() {
        for ok in [
            "http://localhost:3000/",
            "HTTPS://example.com",
            "about:blank",
            "data:text/html,hi",
        ] {
            assert_eq!(check_address(ok), Ok(()), "{ok}");
        }
        for bad in [
            "file:///etc/hosts",
            "chrome://settings",
            "devtools://devtools/bundled/inspector.html",
            "view-source:http://a.test/",
            "javascript:alert(1)",
            "about:srcdoc",
            "blob:http://a.test/1",
            "ftp://a.test/",
            "//a.test/",
            "localhost:3000",
        ] {
            assert_eq!(check_address(bad), Err(Refusal::Scheme), "{bad}");
        }
    }

    #[test]
    fn an_address_is_printable_ascii_without_blanks() {
        assert_eq!(check_address("http://a.test/a b"), Err(Refusal::Address));
        assert_eq!(check_address("http://a.test/\n"), Err(Refusal::Address));
        assert_eq!(check_address("http://ü.test/"), Err(Refusal::Address));
        let long = format!("http://a.test/{}", "a".repeat(MAX_ADDRESS));
        assert_eq!(check_address(&long), Err(Refusal::Address));
    }

    #[test]
    fn frames_inside_a_page_may_be_srcdoc_and_blob() {
        assert!(scheme_allowed("about:srcdoc", false));
        assert!(scheme_allowed("blob:http://a.test/x", false));
        assert!(!scheme_allowed("about:srcdoc", true));
        assert!(!scheme_allowed("file:///", false));
    }

    #[test]
    fn downloads_come_from_what_the_helper_loads_and_a_pages_own_blob() {
        for ok in [
            "http://a.test/f",
            "https://a.test/f",
            "blob:http://a.test/1",
            "data:text/plain,hi",
        ] {
            assert!(download_allowed(ok), "{ok}");
        }
        for bad in [
            "file:///etc/hosts",
            "ftp://a.test/f",
            "view-source:http://a.test/",
            "javascript:1",
            "chrome://x",
        ] {
            assert!(!download_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn a_new_window_is_asked_for_only_at_an_address_the_helper_loads() {
        use browser_ipc::message::Event;
        assert_eq!(
            window_event(3, "http://a.test/", true),
            Event::Window {
                browser: 3,
                url: "http://a.test/".into(),
                background: true
            }
        );
        for bad in [
            "view-source:http://a.test/",
            "file:///etc/hosts",
            "devtools://devtools/x",
            "chrome://settings",
        ] {
            assert_eq!(
                window_event(3, bad, false),
                Event::Refused {
                    browser: 3,
                    url: bad.into(),
                    reason: "scheme".into()
                },
                "{bad}"
            );
        }
    }

    #[test]
    fn the_developer_tools_stay_on_their_own_page() {
        assert!(devtools_frame_allowed(
            "devtools://devtools/bundled/devtools_app.html?x=1",
            true
        ));
        assert!(devtools_frame_allowed(
            "DEVTOOLS://devtools/bundled/inspector.html",
            true
        ));
        for bad in [
            "http://a.test/",
            "https://a.test/",
            "file:///etc/hosts",
            "data:text/html,x",
            "about:blank",
            "view-source:http://a.test/",
        ] {
            assert!(!devtools_frame_allowed(bad, true), "{bad}");
        }
        // Inside the page the helper's set holds, and nothing wider.
        assert!(devtools_frame_allowed("about:blank", false));
        assert!(devtools_frame_allowed("blob:devtools://x/1", false));
        assert!(!devtools_frame_allowed("file:///etc/hosts", false));
        assert!(!devtools_frame_allowed("chrome://settings", false));
    }

    #[test]
    fn a_menu_answer_runs_only_what_the_menu_held() {
        let offered = [100, 102, 113];
        assert!(menu_command_offered(&offered, 113));
        assert!(!menu_command_offered(&offered, 132));
        assert!(!menu_command_offered(&[], 100));
    }

    #[test]
    fn a_download_that_broke_off_is_over_even_while_the_engine_calls_it_in_progress() {
        use browser_ipc::message::DownloadState::*;
        assert_eq!(download_state(false, false, false, false), Running);
        assert_eq!(download_state(true, false, false, false), Done);
        assert_eq!(download_state(false, true, false, false), Canceled);
        assert_eq!(download_state(false, false, true, false), Canceled);
        assert_eq!(download_state(false, false, false, true), Canceled);
        // Finished is finished, whatever it went through on the way.
        assert_eq!(download_state(true, false, false, true), Done);
    }

    #[test]
    fn closing_answers_leave_page_yes_and_the_rest_no() {
        use browser_ipc::message::DialogKind::*;
        assert!(close_answer(BeforeUnload));
        for kind in [Alert, Confirm, Prompt] {
            assert!(!close_answer(kind));
        }
    }

    #[test]
    fn profile_folders_stay_inside_the_root() {
        for ok in ["a", "work-1", "x_y.z"] {
            assert!(profile_dir_ok(ok), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "a/b", "../x", "a b", "ü", "a\0b"] {
            assert!(!profile_dir_ok(bad), "{bad:?}");
        }
        assert!(!profile_dir_ok(&"a".repeat(65)));
    }

    #[test]
    fn a_side_button_is_a_protocol_press_and_release() {
        let down = side_button_call(MouseKind::Down, true, 5.0, 6.5).unwrap();
        assert_eq!(down["type"], "mousePressed");
        assert_eq!(down["button"], "back");
        assert_eq!(down["buttons"], 8);
        assert_eq!(
            (down["x"].as_f64(), down["y"].as_f64()),
            (Some(5.0), Some(6.5))
        );
        let up = side_button_call(MouseKind::Up, false, 5.0, 6.5).unwrap();
        assert_eq!(
            (up["type"].as_str(), up["button"].as_str()),
            (Some("mouseReleased"), Some("forward"))
        );
        assert_eq!(up["buttons"], 0);
        assert_eq!(
            side_button_call(MouseKind::Down, false, 0.0, 0.0).unwrap()["buttons"],
            16
        );
        assert!(side_button_call(MouseKind::Move, true, 0.0, 0.0).is_none());
        assert!(side_button_call(MouseKind::Leave, true, 0.0, 0.0).is_none());
    }

    #[test]
    fn zoom_levels_round_trip_and_clamp() {
        assert_eq!(ratio_to_level(1.0), 0.0);
        let r = level_to_ratio(ratio_to_level(2.0));
        assert!((r - 2.0).abs() < 1e-9);
        assert_eq!(ratio_to_level(100.0), ratio_to_level(ZOOM_MAX));
        assert_eq!(ratio_to_level(0.0), ratio_to_level(ZOOM_MIN));
    }

    #[test]
    fn zoom_steps_are_chromes_presets() {
        assert_eq!(zoom_step(1.0, true), Some(1.1));
        assert_eq!(zoom_step(1.0, false), Some(0.9));
        assert_eq!(zoom_step(1.1, true), Some(1.25));
        assert_eq!(zoom_step(0.9, true), Some(1.0));
        // What the engine hands back after a round trip through its level is the same zoom.
        let held = level_to_ratio(ratio_to_level(1.1));
        assert_eq!(zoom_step(held, true), Some(1.25));
        assert_eq!(zoom_step(held, false), Some(1.0));
        // Between two presets: the one on the side asked for.
        assert_eq!(zoom_step(1.2, true), Some(1.25));
        assert_eq!(zoom_step(1.2, false), Some(1.1));
        // The ends.
        assert_eq!(zoom_step(ZOOM_MAX, true), None);
        assert_eq!(zoom_step(ZOOM_MIN, false), None);
        assert_eq!(zoom_step(ZOOM_MIN, true), Some(1.0 / 3.0));
        // Every step stays inside what a zoom may be.
        let mut at = ZOOM_MIN;
        while let Some(next) = zoom_step(at, true) {
            assert!((ZOOM_MIN..=ZOOM_MAX).contains(&next));
            at = next;
        }
        assert_eq!(at, ZOOM_MAX);
    }

    #[test]
    fn the_rows_the_helper_adds_have_numbers_of_their_own_in_the_embedders_range() {
        let numbers: Vec<i32> = OwnRow::ALL.iter().map(|row| row.command()).collect();
        // The first four are the ones that went out first: a number does not move.
        assert_eq!(
            numbers,
            vec![26500, 26501, 26502, 26503, 26504, 26505, 26506, 26507]
        );
        for row in OwnRow::ALL {
            assert_eq!(OwnRow::of(row.command()), Some(row));
        }
        // The engine's own numbers (Back, Copy, view source) are none of theirs.
        assert_eq!(OwnRow::of(100), None);
        assert_eq!(OwnRow::of(26508), None);
        assert!(OwnRow::ZoomIn.enabled(1.0) && !OwnRow::ZoomIn.enabled(ZOOM_MAX));
        assert!(OwnRow::ZoomOut.enabled(1.0) && !OwnRow::ZoomOut.enabled(ZOOM_MIN));
        assert!(!OwnRow::ActualSize.enabled(1.0) && OwnRow::ActualSize.enabled(1.25));
        assert!(OwnRow::Inspect.enabled(1.0));
    }

    #[test]
    fn what_a_row_opens_is_worded_by_where_bateri_opens_it() {
        assert_eq!(
            OwnRow::OpenLink.label(NewWindows::Page),
            "Open Link in New Tab"
        );
        assert_eq!(OwnRow::OpenLink.label(NewWindows::Here), "Open Link");
        assert_eq!(
            OwnRow::OpenImage.label(NewWindows::Page),
            "Open Image in New Tab"
        );
        assert_eq!(OwnRow::OpenImage.label(NewWindows::Here), "Open Image");
        // What copies says the same either way, and none of them has keys.
        for windows in [NewWindows::Page, NewWindows::Here] {
            assert_eq!(OwnRow::CopyLink.label(windows), "Copy Link Address");
            assert_eq!(OwnRow::CopyImage.label(windows), "Copy Image Address");
        }
        assert!(OwnRow::OpenLink.shortcut().is_empty() && OwnRow::CopyImage.shortcut().is_empty());
    }

    fn urls(link: &str, copy: &str, image: &str) -> MenuUrls {
        MenuUrls {
            open_link: link.into(),
            copy_link: copy.into(),
            image: image.into(),
        }
    }

    #[test]
    fn a_link_and_an_image_each_start_the_menu_with_a_group_of_rows() {
        use OwnRow::*;
        assert_eq!(
            urls("http://a.test/x", "http://a.test/x", "").groups(),
            vec![vec![OpenLink, CopyLink]]
        );
        assert_eq!(
            urls("", "", "data:image/png;base64,AAAA").groups(),
            vec![vec![OpenImage, CopyImage]]
        );
        // An image in a link: the link's rows first.
        assert_eq!(
            urls("https://a.test/", "https://a.test/", "https://a.test/i.png").groups(),
            vec![vec![OpenLink, CopyLink], vec![OpenImage, CopyImage]]
        );
        // Neither: the engine's menu as it is.
        assert!(urls("", "", "").groups().is_empty());
    }

    #[test]
    fn a_row_that_opens_is_there_only_for_an_address_the_helper_loads() {
        use OwnRow::*;
        // The four schemes it loads.
        for url in [
            "http://a.test/",
            "https://a.test/",
            "data:text/html,hi",
            "about:blank",
        ] {
            assert_eq!(
                urls(url, url, "").groups(),
                vec![vec![OpenLink, CopyLink]],
                "{url}"
            );
        }
        // Anything else is copied, and not opened.
        for url in [
            "mailto:a@b.test",
            "javascript:void(0)",
            "file:///etc/passwd",
            "ftp://a.test/",
        ] {
            assert_eq!(urls(url, url, "").groups(), vec![vec![CopyLink]], "{url}");
            assert_eq!(urls("", url, "").groups(), vec![vec![CopyLink]], "{url}");
        }
        // What the engine puts in the place of a link it will not vouch for is not opened, and the
        // address the page wrote is still copied.
        assert_eq!(
            urls("about:blank#blocked", "javascript:void(0)", "").groups(),
            vec![vec![CopyLink]]
        );
        // One that is too long or is not plain ASCII is no address to open either.
        let long = format!("http://a.test/{}", "x".repeat(MAX_ADDRESS));
        assert_eq!(urls(&long, &long, "").groups(), vec![vec![CopyLink]]);
        assert_eq!(
            urls("http://a.test/ü", "http://a.test/ü", "").groups(),
            vec![vec![CopyLink]]
        );
        assert_eq!(
            urls("", "", "ftp://a.test/i.png").groups(),
            vec![vec![CopyImage]]
        );
    }

    #[test]
    fn a_row_works_on_its_own_address() {
        let u = urls(
            "http://open.test/",
            "http://copy.test/",
            "http://image.test/",
        );
        assert_eq!(u.of(OwnRow::OpenLink), Some("http://open.test/"));
        assert_eq!(u.of(OwnRow::CopyLink), Some("http://copy.test/"));
        assert_eq!(u.of(OwnRow::OpenImage), Some("http://image.test/"));
        assert_eq!(u.of(OwnRow::CopyImage), Some("http://image.test/"));
        assert_eq!(u.of(OwnRow::ZoomIn), None);
        assert_eq!(urls("", "", "").of(OwnRow::OpenLink), None);
    }

    #[test]
    fn a_view_is_held_to_what_a_surface_can_be() {
        assert_eq!(clamp_view(0, 99999, f32::NAN), (1, 8192, 1.0));
        assert_eq!(clamp_view(800, 600, 2.0), (800, 600, 2.0));
    }
}
