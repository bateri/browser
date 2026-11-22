//! Emptying what a profile keeps: the engine's HTTP cache, one origin's service workers and Cache
//! Storage, and everything of one host name. Also the choice of going on to a page whose
//! certificate the engine did not trust.
//!
//! The engine has no call that empties a *host*: its protocol clears one origin at a time and an
//! origin is a scheme, a name and a port. The origins of a host are therefore the ones the helper
//! can name: the pages of the profile that are open, the addresses the profile has been to (kept
//! beside the profile, `ORIGINS`), and the folders of its IndexedDB. The cookies are read from the
//! engine and the ones that are sent to the host are deleted.

use crate::records::{self, CookieId, Items};
use crate::state::{browser_of, emit, with, with_entry};
use crate::sync::{self, Call};
use crate::trace;
use browser_ipc::message::{Event, Record};
use cef::*;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

/// The file of a profile's folder that holds the origins it has been to, one per line.
const ORIGINS: &str = "bateri-origins";
/// The most origins the file keeps: the oldest go first.
const MAX_ORIGINS: usize = 2000;

/// The storage types of one origin that a site's data is: everything but the cookies (which are
/// deleted by name, they are not an origin's) and the HTTP cache (the engine cannot empty it by
/// site).
const SITE_TYPES: &str = "local_storage,indexeddb,service_workers,cache_storage,file_systems";
/// What `ClearCache` empties of one origin.
const CACHE_TYPES: &str = "service_workers,cache_storage";

/// What a protocol call of an operation is for (`sync::Call::Clear`).
#[derive(Clone, Debug)]
pub enum Clear {
    /// The profile's cookies, to pick the ones of the host.
    Cookies,
    /// One origin's Local Storage, to list what is deleted.
    Items { origin: String },
    /// A deletion: it only matters by succeeding.
    Delete,
}

enum Kind {
    Cache,
    Site { host: String, dir: Option<String> },
}

enum Phase {
    Read,
    Delete,
}

struct Op {
    browser: u32,
    /// The request the application gave it (`Command::ClearCache`'s `req`): only the answer says it.
    /// Every pane of the application numbers its own requests from the same start, and the helper
    /// is one for all of them, so an operation is kept and its protocol calls are told by `key`.
    req: u32,
    kind: Kind,
    phase: Phase,
    left: usize,
    cookies: Vec<Record>,
    items: Vec<(String, Items)>,
    origins: Vec<String>,
    error: Option<String>,
}

thread_local! {
    static OPS: std::cell::RefCell<HashMap<u32, Op>> = std::cell::RefCell::new(HashMap::new());
    static KEYS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A number no other operation of this run has.
fn next_key() -> u32 {
    KEYS.with(|k| {
        k.set(k.get().wrapping_add(1));
        k.get()
    })
}

fn ops<T>(f: impl FnOnce(&mut HashMap<u32, Op>) -> T) -> T {
    OPS.with_borrow_mut(f)
}

// ---- the pure part ----

/// The host name of an origin (`https://a.test:8443`, with or without a closing slash).
pub fn host_of_origin(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let host = if authority.starts_with('[') {
        authority
            .split_once(']')
            .map(|(h, _)| &authority[..h.len() + 1])?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then_some(host)
}

/// The origin a folder of IndexedDB is named for (`http_localhost_3000`, port 0 is the scheme's
/// own), written as an address's origin.
pub fn origin_of_folder(name: &str) -> Option<String> {
    let (scheme, rest) = name.split_once('_')?;
    let (host, port) = rest.rsplit_once('_')?;
    if !matches!(scheme, "http" | "https") || host.is_empty() || port.parse::<u16>().is_err() {
        return None;
    }
    Some(if port == "0" {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    })
}

/// Whether a cookie of `domain` is sent to `host`: a cookie of a name (without a leading dot)
/// belongs to that one host, a cookie of a domain (with it) to the domain and everything below.
pub fn cookie_goes_to(domain: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let bare = domain.trim_start_matches('.').to_ascii_lowercase();
    if bare.is_empty() {
        return false;
    }
    host == bare || (domain.starts_with('.') && host.ends_with(&format!(".{bare}")))
}

/// What the engine's certificate error codes say, in the words a card shows.
pub fn certificate_reason(code: i32, self_signed: bool) -> Option<&'static str> {
    Some(match code {
        -200 => "wrong host name",
        -201 => "expired",
        -202 if self_signed => "self-signed",
        -202 => "unknown issuer",
        -206 => "revoked",
        -207 | -203 | -208 | -210 | -211 | -212 | -213 | -214 | -215 | -216 | -217 | -218
        | -219 => "not trusted",
        _ => return None,
    })
}

// ---- which origins belong to a host ----

fn origins_path(root: &Path, dir: &str) -> std::path::PathBuf {
    root.join(dir).join(ORIGINS)
}

/// Says the profile has been to `url`'s origin: kept in memory for the run and, for a profile on
/// disk, beside the profile.
pub fn note_address(id: u32, url: &str) {
    let Some(owner) = records::owner_of_url(url) else {
        return;
    };
    let origin = owner.trim_end_matches('/').to_string();
    let Some((context, dir)) = with(|s| {
        let e = s.entries.get(&id)?;
        Some((e.context.clone(), s.contexts.get(&e.context)?.dir.clone()))
    }) else {
        return;
    };
    let fresh = with(|s| {
        s.contexts
            .get_mut(&context)
            .is_some_and(|c| c.origins.insert(origin.clone()))
    });
    let Some(dir) = dir.filter(|_| fresh) else {
        return;
    };
    let path = with(|s| origins_path(&s.root, &dir));
    let mut lines = read_origins(&path);
    if lines.contains(&origin) {
        return;
    }
    lines.push(origin);
    if lines.len() > MAX_ORIGINS {
        lines.drain(..lines.len() - MAX_ORIGINS);
    }
    let _ = std::fs::write(&path, lines.join("\n") + "\n");
}

fn read_origins(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| records::owner_of_url(l).is_some())
        .map(str::to_string)
        .collect()
}

/// The origins of `host` are no longer ones the profile keeps anything of: the run and the profile's
/// file forget them, so a site whose data was cleared leaves no trace of having been visited there.
/// A page of the host still open is noted again when it loads.
fn forget_origins(id: u32, host: &str, dir: Option<&str>) {
    let ours = |o: &String| host_of_origin(o).is_some_and(|h| h.eq_ignore_ascii_case(host));
    with(|s| {
        let context = s.entries.get(&id).map(|e| e.context.clone());
        if let Some(c) = context.and_then(|c| s.contexts.get_mut(&c)) {
            c.origins.retain(|o| !ours(o));
        }
    });
    let Some(dir) = dir else { return };
    let path = with(|s| origins_path(&s.root, dir));
    let lines = read_origins(&path);
    let kept: Vec<&String> = lines.iter().filter(|o| !ours(o)).collect();
    if kept.len() != lines.len() {
        let text: String = kept.iter().map(|o| format!("{o}\n")).collect();
        let _ = std::fs::write(&path, text);
    }
}

/// The origins of `host` the helper can name for the profile of browser `id`.
fn origins_of_host(id: u32, host: &str, dir: Option<&str>) -> BTreeSet<String> {
    let (visited, open, root) = with(|s| {
        let context = s.entries.get(&id).map(|e| e.context.clone());
        let open: Vec<String> = s
            .entries
            .values()
            .filter(|e| Some(&e.context) == context.as_ref())
            .filter_map(|e| records::owner_of_url(&e.url))
            .collect();
        let visited: Vec<String> = context
            .as_ref()
            .and_then(|c| s.contexts.get(c))
            .map(|c| c.origins.iter().cloned().collect())
            .unwrap_or_default();
        (visited, open, s.root.clone())
    });
    let mut all: Vec<String> = visited.into_iter().chain(open).collect();
    if let Some(dir) = dir {
        all.extend(read_origins(&origins_path(&root, dir)));
        all.extend(
            records::idb_origins(&root.join(dir))
                .iter()
                .filter_map(|name| origin_of_folder(name)),
        );
    }
    all.into_iter()
        .filter(|o| host_of_origin(o).is_some_and(|h| h.eq_ignore_ascii_case(host)))
        .map(|o| o.trim_end_matches('/').to_string())
        .collect()
}

// ---- the operations ----

/// A browser of the profile of `id` that shows a page of `owner` (`https://a.test/`).
fn open_at(id: u32, owner: &str) -> Option<u32> {
    with(|s| {
        let context = s.entries.get(&id)?.context.clone();
        s.entries
            .values()
            .find(|e| {
                e.context == context
                    && e.browser.is_some()
                    && !e.closing
                    && records::owner_of_url(&e.url).as_deref() == Some(owner)
            })
            .map(|e| e.id)
    })
}

fn send(browser: u32, key: u32, method: &'static str, params: Value, what: Clear) -> bool {
    sync::send_call(browser, method, params, Call::Clear { key, what })
}

/// `Command::ClearCache`.
pub fn cache(id: u32, req: u32, origin: String) {
    let Some(host) = browser_of(id).and_then(|b| b.host()) else {
        emit(Event::CacheCleared {
            browser: id,
            req,
            error: Some("the browser is gone".into()),
        });
        return;
    };
    let key = next_key();
    // The engine's cache is one call, and a site's caches of the page's origin another: counted
    // before either is made, so that the answer of the first does not end the operation.
    let origin_call = !origin.is_empty() && records::owner_of_url(&format!("{origin}/")).is_some();
    ops(|o| {
        o.insert(
            key,
            Op {
                browser: id,
                req,
                kind: Kind::Cache,
                phase: Phase::Delete,
                left: 1 + usize::from(origin_call),
                cookies: Vec::new(),
                items: Vec::new(),
                origins: Vec::new(),
                error: None,
            },
        )
    });
    // The engine's cache is the profile's: every site's.
    match host.request_context() {
        Some(context) => {
            let mut done = CacheDone::new(key);
            context.clear_http_cache(Some(&mut done));
        }
        None => finish_one(key, Some("the profile has no request context".into())),
    }
    if origin_call
        && !send(
            id,
            key,
            "Storage.clearDataForOrigin",
            json!({"origin": origin, "storageTypes": CACHE_TYPES}),
            Clear::Delete,
        )
    {
        finish_one(key, Some("the protocol call was not sent".into()));
    }
}

wrap_completion_callback! {
    struct CacheDone {
        key: u32,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            trace!("clear {}: the HTTP cache is empty", self.key);
            finish_one(self.key, None);
        }
    }
}

/// `Command::ClearSiteData`.
pub fn site(id: u32, req: u32, host: String) {
    let dir = with(|s| {
        let e = s.entries.get(&id)?;
        s.contexts.get(&e.context)?.dir.clone()
    });
    let named =
        !host.is_empty() && (host.starts_with('[') || !host.contains(['/', ':', ' ', '\\', '@']));
    if !named {
        emit(Event::SiteCleared {
            browser: id,
            req,
            records: Vec::new(),
            error: Some("not a host name".into()),
        });
        return;
    }
    let origins: Vec<String> = origins_of_host(id, &host, dir.as_deref())
        .into_iter()
        .collect();
    trace!("clear {req}: site {host}: origins {origins:?}");
    let key = next_key();
    ops(|o| {
        o.insert(
            key,
            Op {
                browser: id,
                req,
                kind: Kind::Site { host, dir },
                phase: Phase::Read,
                left: 0,
                cookies: Vec::new(),
                items: Vec::new(),
                origins: origins.clone(),
                error: None,
            },
        )
    });
    let mut left = 0;
    if send(id, key, "Network.getAllCookies", json!({}), Clear::Cookies) {
        left += 1;
    }
    // The Local Storage the engine will tell of is the one of a page that is open.
    for origin in &origins {
        let owner = format!("{origin}/");
        let Some(b) = open_at(id, &owner) else {
            continue;
        };
        let call = Clear::Items {
            origin: owner.clone(),
        };
        if send(
            b,
            key,
            "DOMStorage.getDOMStorageItems",
            json!({"storageId": records::storage_id(&owner)}),
            call,
        ) {
            left += 1;
        }
    }
    ops(|o| o.get_mut(&key).map(|op| op.left = left));
    if left == 0 {
        read_done(key);
    }
}

/// The answer to a protocol call of an operation.
pub fn on_result(browser: u32, key: u32, what: Clear, success: bool, json: &Value) {
    let why = || {
        json["message"]
            .as_str()
            .unwrap_or("the protocol call failed")
            .to_string()
    };
    match what {
        Clear::Cookies => {
            if success {
                let all = records::cookies_of(json);
                ops(|o| o.get_mut(&key).map(|op| op.cookies = all));
            }
            finish_one(key, (!success).then(why));
        }
        Clear::Items { origin } => {
            if success {
                let items: Items = records::items_of(json).into_iter().collect();
                ops(|o| o.get_mut(&key).map(|op| op.items.push((origin, items))));
            }
            finish_one(key, (!success).then(why));
        }
        Clear::Delete => finish_one(key, (!success).then(why)),
    }
    let _ = browser;
}

/// A call of an operation will not be answered (its browser went away).
pub fn call_lost(key: u32, why: &str) {
    finish_one(key, Some(why.into()));
}

fn finish_one(key: u32, error: Option<String>) {
    let reached = ops(|o| {
        let op = o.get_mut(&key)?;
        op.left = op.left.saturating_sub(1);
        if op.error.is_none() {
            op.error = error;
        }
        (op.left == 0).then_some(matches!(op.phase, Phase::Read))
    });
    match reached {
        Some(true) => read_done(key),
        Some(false) => answer(key),
        None => {}
    }
}

/// Everything there is to know is known: what goes is listed, and deleted.
fn read_done(key: u32) {
    let Some((browser, host, dir, cookies, items, origins)) = ops(|o| {
        let op = o.get_mut(&key)?;
        op.phase = Phase::Delete;
        let Kind::Site { host, dir } = &op.kind else {
            return None;
        };
        Some((
            op.browser,
            host.clone(),
            dir.clone(),
            std::mem::take(&mut op.cookies),
            std::mem::take(&mut op.items),
            op.origins.clone(),
        ))
    }) else {
        return;
    };
    let mut gone: Vec<Record> = Vec::new();
    let mut calls: Vec<(u32, &'static str, Value)> = Vec::new();
    let mut ids: Vec<CookieId> = Vec::new();
    for cookie in &cookies {
        let Some((id, _)) = records::cookie_id(cookie) else {
            continue;
        };
        if cookie_goes_to(&id.domain, &host) {
            gone.push(id.record(None));
            calls.push((
                browser,
                "Network.deleteCookies",
                records::delete_params(&id),
            ));
            ids.push(id);
        }
    }
    let mut known_items: Vec<(String, Vec<String>)> = Vec::new();
    for (owner, items) in &items {
        known_items.push((owner.clone(), items.keys().cloned().collect()));
    }
    // What the profile's base holds of the host's Local Storage is listed too: the open page
    // may not be where the items are.
    for (owner, keys) in sync::storage_base(dir.as_deref(), &host) {
        match known_items.iter_mut().find(|(o, _)| *o == owner) {
            Some((_, have)) => {
                for k in keys {
                    if !have.contains(&k) {
                        have.push(k);
                    }
                }
            }
            None => known_items.push((owner, keys)),
        }
    }
    for (owner, keys) in &known_items {
        for key in keys {
            gone.push(Record::LocalStorage {
                origin: owner.clone(),
                key: key.clone(),
                value: None,
            });
        }
    }
    let mut folders: Vec<String> = Vec::new();
    if let Some(dir) = &dir {
        let root = with(|s| s.root.clone());
        for name in records::idb_origins(&root.join(dir)) {
            let matches = origin_of_folder(&name)
                .and_then(|o| host_of_origin(&o).map(|h| h.eq_ignore_ascii_case(&host)))
                .unwrap_or(false);
            if matches {
                gone.push(Record::IndexedDb {
                    origin: name.clone(),
                    value: None,
                });
                folders.push(name);
            }
        }
    }
    for origin in &origins {
        calls.push((
            browser,
            "Storage.clearDataForOrigin",
            json!({"origin": origin, "storageTypes": SITE_TYPES}),
        ));
    }
    // What was deleted is not something the profile's watch has to report again.
    if let Some(dir) = &dir {
        sync::forget(dir, &ids, &known_items, &folders);
    }
    forget_origins(browser, &host, dir.as_deref());
    ops(|o| {
        if let Some(op) = o.get_mut(&key) {
            op.cookies = gone;
        }
    });
    let mut left = 0;
    for (b, method, params) in calls {
        if send(b, key, method, params, Clear::Delete) {
            left += 1;
        }
    }
    ops(|o| o.get_mut(&key).map(|op| op.left = left));
    if left == 0 {
        answer(key);
    }
}

fn answer(key: u32) {
    let Some(op) = ops(|o| o.remove(&key)) else {
        return;
    };
    trace!("clear {}: answered, error {:?}", op.req, op.error);
    match op.kind {
        Kind::Cache => emit(Event::CacheCleared {
            browser: op.browser,
            req: op.req,
            error: op.error,
        }),
        Kind::Site { .. } => emit(Event::SiteCleared {
            browser: op.browser,
            req: op.req,
            records: op.cookies,
            error: op.error,
        }),
    }
}

/// A browser's operations that cannot be answered (it never came up): answered at once.
/// `false` for a command of another kind.
pub fn refuse(id: u32, command: &browser_ipc::Command, why: &str) -> bool {
    match command {
        browser_ipc::Command::ClearCache { req, .. } => emit(Event::CacheCleared {
            browser: id,
            req: *req,
            error: Some(why.into()),
        }),
        browser_ipc::Command::ClearSiteData { req, .. } => emit(Event::SiteCleared {
            browser: id,
            req: *req,
            records: Vec::new(),
            error: Some(why.into()),
        }),
        _ => return false,
    }
    true
}

// ---- a certificate the person goes on with ----

/// `Command::Proceed`.
pub fn proceed(id: u32) {
    let Some(browser) = browser_of(id) else {
        return;
    };
    let target = with_entry(id, |e| {
        let error = e.error.as_ref()?;
        error.certificate.as_ref()?;
        let host = host_of_origin(&error.url)?.to_ascii_lowercase();
        let der = e.rejected.get(&host)?.der.clone();
        // A certificate the engine would not show is none to go on with: nothing is accepted by an empty
        // one, which would be every certificate of the host.
        (!der.is_empty()).then(|| (error.url.clone(), host, der, e.context.clone()))
    })
    .flatten();
    let Some((url, host, der, context)) = target else {
        trace!("proceed {id}: there is no certificate to go on with");
        return;
    };
    with(|s| {
        if let Some(c) = s.contexts.get_mut(&context) {
            c.accepted.insert((host, der));
        }
    });
    if let Some(frame) = browser.main_frame() {
        frame.load_url(Some(&url.as_str().into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_of_an_origin_has_no_scheme_port_or_slash() {
        assert_eq!(host_of_origin("http://localhost:3000"), Some("localhost"));
        assert_eq!(host_of_origin("https://a.test/"), Some("a.test"));
        assert_eq!(host_of_origin("http://[::1]:8080/x"), Some("[::1]"));
        assert_eq!(host_of_origin("https://u:p@a.test:1/"), Some("a.test"));
        assert_eq!(host_of_origin("about:blank"), None);
    }

    #[test]
    fn a_folder_of_indexed_db_names_its_origin() {
        assert_eq!(
            origin_of_folder("http_localhost_3000").as_deref(),
            Some("http://localhost:3000")
        );
        assert_eq!(
            origin_of_folder("https_example.com_0").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(origin_of_folder("file__0"), None);
        assert_eq!(origin_of_folder("http_a_b_x"), None);
    }

    #[test]
    fn a_cookie_of_a_domain_goes_to_the_domain_and_what_is_below_it() {
        assert!(cookie_goes_to("localhost", "localhost"));
        assert!(cookie_goes_to(".example.com", "app.example.com"));
        assert!(cookie_goes_to(".example.com", "example.com"));
        assert!(cookie_goes_to("app.example.com", "app.example.com"));
        // A cookie of a name belongs to that name alone; a parent's host-only one is not sent down.
        assert!(!cookie_goes_to("example.com", "app.example.com"));
        assert!(!cookie_goes_to(".app.example.com", "example.com"));
        assert!(!cookie_goes_to(".example.com", "badexample.com"));
        assert!(!cookie_goes_to("", "localhost"));
    }

    #[test]
    fn a_certificate_error_has_its_words() {
        assert_eq!(certificate_reason(-202, true), Some("self-signed"));
        assert_eq!(certificate_reason(-202, false), Some("unknown issuer"));
        assert_eq!(certificate_reason(-200, false), Some("wrong host name"));
        assert_eq!(certificate_reason(-201, false), Some("expired"));
        assert_eq!(certificate_reason(-102, false), None);
        assert_eq!(certificate_reason(-3, false), None);
    }
}
