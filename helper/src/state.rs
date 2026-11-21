//! What the helper keeps while it runs. CEF calls the handlers on the UI thread, which is the
//! main thread, so the state is thread-local; the few callbacks that come from another thread
//! (proxy credentials, the pump's wake-up) carry what they need in their own fields instead.
//!
//! One rule keeps the `RefCell` honest: nothing here is borrowed across a call into CEF, because
//! CEF may call a handler back before the call returns. Take what you need out (`browser_of`,
//! `with_entry`), let go, then call.

use crate::agent::Job;
use browser_ipc::message::{DialogKind, Event, Found, KeyData, KeyKind, LoadError};
use browser_ipc::{Command, Sender};
use cef::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// What the main loop is woken for.
pub enum Msg {
    Command(Command),
    /// CEF wants its work done (see `on_schedule_message_pump_work`).
    Wake,
    /// The conversation with bateri ended.
    Hangup,
}

static QUIT: AtomicBool = AtomicBool::new(false);
static TX: OnceLock<mpsc::Sender<Msg>> = OnceLock::new();
static OUT: OnceLock<mpsc::Sender<Out>> = OnceLock::new();
static DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);
static TRACE: OnceLock<bool> = OnceLock::new();

/// What the writer thread is given.
pub enum Out {
    Event(Event),
    /// Answered once everything before it has been written.
    Flush(mpsc::Sender<()>),
}

pub fn install_channels(tx: mpsc::Sender<Msg>, out: mpsc::Sender<Out>) {
    let _ = TX.set(tx);
    let _ = OUT.set(out);
}

/// Asks the main loop to leave (it flushes the profiles first).
pub fn request_quit() {
    QUIT.store(true, Ordering::SeqCst);
    wake();
}

pub fn quitting() -> bool {
    QUIT.load(Ordering::SeqCst)
}

pub fn wake() {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::Wake);
    }
}

/// Records when CEF wants to run next (any thread).
pub fn schedule_pump(delay_ms: i64) {
    let at = Instant::now() + Duration::from_millis(delay_ms.max(0) as u64);
    *DEADLINE.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
    wake();
}

/// The time CEF asked to run at, once.
pub fn take_pump_deadline() -> Option<Instant> {
    DEADLINE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

pub fn peek_pump_deadline() -> Option<Instant> {
    *DEADLINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sends an event to bateri (never blocks the caller: a writer thread owns the socket).
pub fn emit(event: Event) {
    if let Some(out) = OUT.get() {
        let _ = out.send(Out::Event(event));
    }
}

/// Waits (at most `limit`) until what was emitted has been written: the last thing before exit.
pub fn flush_output(limit: Duration) {
    if let Some(out) = OUT.get() {
        let (done, wait) = mpsc::channel();
        if out.send(Out::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(limit);
        }
    }
}

/// For a signal handler: only sets the flag (the loop looks at it on its next turn).
pub fn quit_from_signal() {
    QUIT.store(true, Ordering::SeqCst);
}

pub fn emit_error(browser: Option<u32>, message: impl Into<String>) {
    emit(Event::Error {
        browser,
        message: message.into(),
    });
}

/// Debug lines for whoever runs the helper by hand: `BATERI_BROWSER_TRACE=1`.
pub fn trace(what: std::fmt::Arguments<'_>) {
    if *TRACE.get_or_init(|| std::env::var_os("BATERI_BROWSER_TRACE").is_some()) {
        eprintln!("[browser] {what}");
    }
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::state::trace(format_args!($($arg)*)) };
}

/// The proxy credentials of one profile: the identity belongs to the context.
pub struct Creds {
    pub user: String,
    pub pass: String,
    /// When the last question came, and how many came in a row (see `allow`).
    asked: Mutex<(Option<Instant>, u32)>,
}

/// Questions answered in one burst before the helper stops answering.
const MAX_ASKS: u32 = 16;
/// Questions closer together than this are one burst.
const BURST: Duration = Duration::from_secs(2);

impl Creds {
    pub fn new(user: String, pass: String) -> Self {
        Self {
            user,
            pass,
            asked: Mutex::new((None, 0)),
        }
    }

    /// Whether to answer this question. A proxy that keeps refusing the same credentials makes the
    /// engine ask again and again; after a burst of questions the helper says no instead, and the
    /// page fails with the proxy's own answer. (This runs on the IO thread.)
    pub fn allow(&self) -> bool {
        let now = Instant::now();
        let mut asked = self.asked.lock().unwrap_or_else(|e| e.into_inner());
        match asked.0 {
            Some(last) if now.duration_since(last) < BURST => asked.1 += 1,
            _ => asked.1 = 1,
        }
        asked.0 = Some(now);
        asked.1 <= MAX_ASKS
    }
}

pub struct Context {
    pub context: RequestContext,
    /// Why the context may not be used (a preference that keeps the network closed is not in
    /// force): no browser is opened in it.
    pub failed: Option<String>,
    pub creds: Arc<Creds>,
    /// The profile's folder name under the root; `None` is a profile that lives in memory only.
    pub dir: Option<String>,
    pub initialized: bool,
    /// Browsers (open or being opened) using it.
    pub users: u32,
    /// Opens waiting for the context to come up.
    pub waiting: Vec<u32>,
}

/// What a new search needs to ask for its first match.
pub struct FindStart {
    pub pattern: String,
    pub forward: bool,
    pub case: bool,
    /// The first match has been asked for.
    pub asked: bool,
}

#[derive(Default)]
pub struct Entry {
    pub id: u32,
    pub context: String,
    pub browser: Option<Browser>,
    /// The developer-tools observer lives as long as the browser: dropping it unregisters.
    pub observer: Option<Registration>,
    /// Commands that arrived before the browser existed.
    pub queue: Vec<Command>,
    /// The view in points, and the display's pixel ratio.
    pub size: (i32, i32),
    pub scale: f32,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub status: String,
    pub error: Option<LoadError>,
    pub found: Option<Found>,
    /// A new search was started and has not selected its first match yet.
    pub find_activate: Option<FindStart>,
    pub last_state: Option<Event>,
    /// The popup's rectangle in points while one shows.
    pub popup: Option<(i32, i32, i32, i32)>,
    /// A frame was dropped (bateri behind): ask for a fresh one when this time comes.
    pub repaint_at: Option<Instant>,
    pub jobs: HashMap<i32, Job>,
    /// Keys sent through the protocol whose answer has not come, by call number: if the call
    /// fails the key is sent again the engine's own way (see `agent::fire`).
    pub fired: HashMap<i32, Fired>,
    pub next_cdp: i32,
    /// The questions of `Command::CanScroll` the page has not answered, by call number: the `req`
    /// each answer is owed to.
    pub scroll_asks: HashMap<i32, u32>,
    /// How bateri takes a window the browser's menu asks for (`Command::Windows`): the words of
    /// the rows that open.
    pub new_windows: browser_ipc::message::NewWindows,
    /// The trackpad pinch that is under way ([`crate::pinch`]).
    pub pinch: crate::pinch::Machine,
    /// The developer tools were asked for: until this time the helper brings their window to the
    /// front (the engine makes it a moment later, and a background app's windows do not come
    /// forward by themselves).
    pub devtools_front: Option<Instant>,
    pub closing: bool,
}

/// A key sent to the page as a protocol call: what to send the engine's own way if the call fails.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fired {
    pub kind: KeyKind,
    pub key: KeyData,
}

/// A running download.
pub struct Download {
    /// bateri's number for it.
    pub req: u32,
    /// The callback that cancels it, from the engine's latest update.
    pub cancel: Option<DownloadItemCallback>,
}

/// A context menu bateri is choosing from.
pub struct OpenMenu {
    pub callback: RunContextMenuCallback,
    /// The commands the menu held: the only ones an answer may run.
    pub offered: Vec<i32>,
    /// Where it was asked for, in points from the view's corner (what "Inspect" looks at).
    pub at: (i32, i32),
    /// The addresses of the link and of the image it was asked for (what their rows work on).
    pub urls: crate::rules::MenuUrls,
}

/// Answers the helper is waiting for from bateri: `(browser, ...)`.
#[derive(Default)]
pub struct Pending {
    pub menus: HashMap<u32, (u32, OpenMenu)>,
    pub dialogs: HashMap<u32, (u32, DialogKind, JsdialogCallback)>,
    pub files: HashMap<u32, (u32, FileDialogCallback)>,
    pub before_download: HashMap<u32, (u32, BeforeDownloadCallback)>,
    /// Running downloads by (browser, the engine's item number): two profiles' download managers
    /// each count from their own start, so the item number alone is not one download's.
    pub downloads: HashMap<(u32, u32), Download>,
}

#[derive(Default)]
pub struct State {
    pub ready: bool,
    pub entries: HashMap<u32, Entry>,
    pub contexts: HashMap<String, Context>,
    /// Contexts the engine reported ready before they were put away, and why one of them may not
    /// be used (`Context::failed`).
    pub early: HashMap<String, Option<String>>,
    pub frames: Option<Sender>,
    pub seq: u32,
    pub next_req: u32,
    pub pending: Pending,
    pub root: PathBuf,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn with<T>(f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with_borrow_mut(f)
}

pub fn with_entry<T>(id: u32, f: impl FnOnce(&mut Entry) -> T) -> Option<T> {
    STATE.with_borrow_mut(|s| s.entries.get_mut(&id).map(f))
}

pub fn browser_of(id: u32) -> Option<Browser> {
    STATE.with_borrow(|s| s.entries.get(&id).and_then(|e| e.browser.clone()))
}

/// Takes out of `map` every entry `pick` chooses and hands them over. Letting go of an engine
/// handle may call back into a handler, which borrows the state: whoever receives these drops
/// them after the borrow is over, never inside `with`.
pub fn take_where<K: Eq + Hash + Clone, V>(
    map: &mut HashMap<K, V>,
    mut pick: impl FnMut(&K, &V) -> bool,
) -> Vec<(K, V)> {
    let keys: Vec<K> = map
        .iter()
        .filter(|(k, v)| pick(k, v))
        .map(|(k, _)| k.clone())
        .collect();
    keys.into_iter()
        .filter_map(|k| map.remove_entry(&k))
        .collect()
}

/// A number for the next question bateri is asked (menu, dialog, file dialog, download).
pub fn next_req() -> u32 {
    with(|s| {
        s.next_req += 1;
        s.next_req
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    /// Something whose drop calls back into the state, as an engine handle's can.
    struct Handle(Rc<std::cell::Cell<u32>>);

    impl Drop for Handle {
        fn drop(&mut self) {
            // Panics when the state is borrowed: that is the mistake this guards against.
            with(|_| ());
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn what_is_taken_out_is_handed_over_not_dropped_inside_the_borrow() {
        let dropped = Rc::new(std::cell::Cell::new(0));
        let mut map: HashMap<u32, (u32, Handle)> = HashMap::new();
        for (req, browser) in [(1, 7), (2, 7), (3, 8)] {
            map.insert(req, (browser, Handle(dropped.clone())));
        }
        let taken = with(|_| take_where(&mut map, |_, v| v.0 == 7));
        assert_eq!(dropped.get(), 0, "dropped while the state was borrowed");
        assert_eq!(taken.len(), 2);
        assert_eq!(map.len(), 1);
        drop(taken);
        assert_eq!(dropped.get(), 2);
    }

    #[test]
    fn downloads_of_two_profiles_with_one_item_number_are_two_downloads() {
        let mut pending = Pending::default();
        pending.downloads.insert(
            (1, 1),
            Download {
                req: 10,
                cancel: None,
            },
        );
        pending.downloads.insert(
            (2, 1),
            Download {
                req: 11,
                cancel: None,
            },
        );
        let gone = take_where(&mut pending.downloads, |k, _| k.0 == 2);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].1.req, 11);
        assert_eq!(pending.downloads[&(1, 1)].req, 10);
    }
}
