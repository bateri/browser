//! A profile's data between the engine and bateri: what the profile holds is read as records,
//! what comes from elsewhere is written into it, and what changes in it is reported (see
//! `browser_ipc::message::Record`). Cookies and Local Storage go through the engine's protocol
//! (`Network`, `DOMStorage`), IndexedDB goes by folder (`records`).
//!
//! Every report is a difference from a base the helper keeps for the profile (what was read, plus
//! everything written since): what bateri wrote is in the base before the engine says anything
//! about it, so it never comes back as a change, and a change two pages of one origin both see is
//! said once. The cookies are looked at every second (the protocol has no event for a cookie the
//! page's script sets) and at once when a response sets one; Local Storage is said by the protocol's
//! own events; the folders of IndexedDB are looked at every second and copied when they have been
//! still for a moment.
//!
//! A Local Storage of an origin that has no page open cannot be read or written through the
//! protocol (it needs the origin's frame). Reads wait for the first page of the origin; writes
//! wait there too, as a script the protocol has every new document of the browser run first (a
//! "seed"), which is gone once a page of the origin has loaded.
//!
//! Everything here runs on the main thread, outside any borrow of the state.

use crate::records::{self, CookieId, Items};
use crate::state::{browser_of, emit, with, with_entry};
use crate::trace;
use browser_ipc::Command;
use browser_ipc::message::{CookieValue, Event, Record};
use cef::*;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the cookies are looked at, and the folders of IndexedDB.
const POLL: Duration = Duration::from_secs(1);
/// How long a folder of IndexedDB has to lie still before it is copied.
const QUIET: Duration = Duration::from_secs(2);
/// Where the copies of IndexedDB folders are made, below the helper's root.
pub const STAGES: &str = ".stage";

/// What a protocol call of this module is for: the answer is routed by it (`on_result`).
#[derive(Clone, Debug)]
pub enum Call {
    /// Only its failure matters, and not to anyone.
    Quiet,
    /// The cookies a `ProfileRead` asked for.
    ReadCookies { req: u32 },
    /// The cookies the poll looks at.
    PollCookies,
    /// An origin's items for a `ProfileRead`.
    ReadItems { req: u32, origin: String },
    /// An origin's items, the first time a page of it has loaded.
    VisitItems { origin: String },
    /// A part of a `ProfileWrite` that matters only by succeeding.
    Part { req: Option<u32> },
    /// The registration of a seed script; the answer names it. `generation` is the browser's
    /// count when it was asked for: an answer to an older one names a script that is taken away.
    AddScript { req: Option<u32>, generation: u64 },
    /// A call of the operations that empty a profile (`clear`).
    Clear { key: u32, what: crate::clear::Clear },
}

enum Op {
    Read {
        browser: u32,
        left: usize,
        cookies: Vec<Record>,
        items: Vec<Record>,
        error: Option<String>,
    },
    Write {
        browser: u32,
        left: usize,
        deferred: u32,
        error: Option<String>,
    },
}

/// What the helper keeps for one profile (by the name of its folder).
#[derive(Default)]
struct Prof {
    watching: bool,
    /// The cookies as they were last looked at; `None` until the first look.
    cookies: Option<HashMap<CookieId, CookieValue>>,
    polling: bool,
    poll_at: Option<Instant>,
    /// Local Storage by origin, and the origins that have been read whole.
    storage: HashMap<String, Items>,
    storage_read: std::collections::HashSet<String>,
    /// Items waiting for the first page of their origin.
    seeds: BTreeMap<String, BTreeMap<String, Option<String>>>,
    /// IndexedDB: the signature of each origin's folders as last reported (or written), whether
    /// the first look has been taken, and the folders that changed and are not yet still.
    idb: HashMap<String, u64>,
    idb_known: bool,
    idb_moving: HashMap<String, (u64, Instant)>,
    idb_at: Option<Instant>,
}

#[derive(Default)]
struct Sync {
    profiles: HashMap<String, Prof>,
    ops: HashMap<u32, Op>,
    next_stage: u32,
}

thread_local! {
    static SYNC: RefCell<Sync> = RefCell::new(Sync::default());
}

fn sync<T>(f: impl FnOnce(&mut Sync) -> T) -> T {
    SYNC.with_borrow_mut(f)
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The folder of the profile a browser is in; `None` for a profile that lives in memory.
fn dir_of(id: u32) -> Option<String> {
    with(|s| {
        let entry = s.entries.get(&id)?;
        s.contexts.get(&entry.context)?.dir.clone()
    })
}

/// The browsers of a profile that are up, lowest number first.
fn browsers_of(dir: &str) -> Vec<u32> {
    let mut ids: Vec<u32> = with(|s| {
        s.entries
            .values()
            .filter(|e| e.browser.is_some() && !e.closing)
            .filter(|e| s.contexts.get(&e.context).and_then(|c| c.dir.as_deref()) == Some(dir))
            .map(|e| e.id)
            .collect()
    });
    ids.sort();
    ids
}

/// The browser of a profile that has a page of `origin` open.
fn browser_at(dir: &str, origin: &str) -> Option<u32> {
    browsers_of(dir).into_iter().find(|id| {
        with_entry(*id, |e| records::owner_of_url(&e.url))
            .flatten()
            .as_deref()
            == Some(origin)
    })
}

/// Sends a protocol call and remembers what it is for. `false` when it could not be sent.
fn send(id: u32, method: &'static str, params: Value, call: Call) -> bool {
    let Some(host) = browser_of(id).and_then(|b| b.host()) else {
        return false;
    };
    let Some(message_id) = with_entry(id, |e| {
        e.next_cdp += 1;
        e.syncs.insert(e.next_cdp, call);
        e.next_cdp
    }) else {
        return false;
    };
    trace!("sync {id}#{message_id} {method}");
    let mut dict = crate::agent::to_dict(&params);
    let sent =
        host.execute_dev_tools_method(message_id, Some(&method.into()), Some(&mut dict)) != 0;
    if !sent {
        with_entry(id, |e| e.syncs.remove(&message_id));
    }
    sent
}

/// `send` for the operations of `clear`.
pub fn send_call(id: u32, method: &'static str, params: Value, call: Call) -> bool {
    send(id, method, params, call)
}

/// What the base of the profile `dir` holds of Local Storage for origins of `host`: the owner and
/// the keys.
pub fn storage_base(dir: Option<&str>, host: &str) -> Vec<(String, Vec<String>)> {
    let Some(dir) = dir else { return Vec::new() };
    sync(|s| {
        let Some(p) = s.profiles.get(dir) else {
            return Vec::new();
        };
        p.storage
            .iter()
            .filter(|(owner, _)| {
                crate::clear::host_of_origin(owner).is_some_and(|h| h.eq_ignore_ascii_case(host))
            })
            .map(|(owner, items)| (owner.clone(), items.keys().cloned().collect()))
            .collect()
    })
}

/// What was deleted is taken out of the base of the profile `dir`, so that its watch does not
/// report it as gone a second time.
pub fn forget(
    dir: &str,
    cookies: &[records::CookieId],
    storage: &[(String, Vec<String>)],
    folders: &[String],
) {
    sync(|s| {
        let Some(p) = s.profiles.get_mut(dir) else {
            return;
        };
        if let Some(base) = p.cookies.as_mut() {
            for cookie in cookies {
                base.remove(cookie);
            }
        }
        for (owner, _) in storage {
            p.storage.remove(owner);
        }
        for name in folders {
            p.idb.remove(name);
            p.idb_moving.remove(name);
        }
    });
}

/// The protocol's storage and network events are asked for in every browser of the profile.
fn ensure_domains(dir: &str) {
    for id in browsers_of(dir) {
        let fresh =
            with_entry(id, |e| !std::mem::replace(&mut e.sync_ready, true)).unwrap_or(false);
        if fresh {
            send(id, "Network.enable", json!({}), Call::Quiet);
            send(id, "DOMStorage.enable", json!({}), Call::Quiet);
            // Without the page domain the engine does not run the scripts it was told to run before a
            // new document's own (`reseed`).
            send(id, "Page.enable", json!({}), Call::Quiet);
        }
    }
}

/// A browser came up: it joins what the profile is doing.
pub fn browser_ready(id: u32) {
    let Some(dir) = dir_of(id) else { return };
    let (watching, source) = sync(|s| {
        let p = s.profiles.get(&dir);
        (
            p.is_some_and(|p| p.watching),
            p.and_then(|p| records::seed_script(&p.seeds)),
        )
    });
    // The page domain is what makes the engine run a script before a document's own: it is on
    // for a script as well as for a watch.
    if watching || source.is_some() {
        ensure_domains(&dir);
    }
    if let Some(source) = source {
        let generation = with_entry(id, |e| {
            e.seed_gen += 1;
            e.seed_gen
        })
        .unwrap_or(0);
        send(
            id,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": source}),
            Call::AddScript {
                req: None,
                generation,
            },
        );
    }
}

/// A browser is going away: what was asked of it is answered with that.
pub fn browser_gone(id: u32) {
    let calls = with_entry(id, |e| std::mem::take(&mut e.syncs)).unwrap_or_default();
    let mut dirs = Vec::new();
    if let Some(dir) = dir_of(id) {
        dirs.push(dir);
    }
    for (_, call) in calls {
        match call {
            Call::PollCookies => {
                for dir in &dirs {
                    sync(|s| {
                        if let Some(p) = s.profiles.get_mut(dir) {
                            // The poll that was out is not coming back: the next one is asked
                            // for, or the profile's cookies are never read again.
                            p.polling = false;
                            p.poll_at = Some(Instant::now());
                        }
                    });
                }
            }
            Call::ReadCookies { req }
            | Call::ReadItems { req, .. }
            | Call::Part { req: Some(req) }
            | Call::AddScript { req: Some(req), .. } => {
                call_done(req, Some("the browser was closed".into()));
            }
            Call::Clear { key, .. } => crate::clear::call_lost(key, "the browser was closed"),
            _ => {}
        }
    }
}

// ---- reading ----

pub fn read(id: u32, req: u32) {
    let Some(dir) = dir_of(id) else {
        emit(Event::ProfileRecords {
            browser: id,
            req,
            records: Vec::new(),
            error: Some("the profile is not kept on disk".into()),
        });
        return;
    };
    ensure_domains(&dir);
    let mut left = 0;
    if send(
        id,
        "Network.getAllCookies",
        json!({}),
        Call::ReadCookies { req },
    ) {
        left += 1;
    }
    let mut seen = Vec::new();
    for b in browsers_of(&dir) {
        let Some(origin) = with_entry(b, |e| records::owner_of_url(&e.url)).flatten() else {
            continue;
        };
        if seen.contains(&origin) {
            continue;
        }
        let call = Call::ReadItems {
            req,
            origin: origin.clone(),
        };
        if send(
            b,
            "DOMStorage.getDOMStorageItems",
            json!({"storageId": records::storage_id(&origin)}),
            call,
        ) {
            left += 1;
            seen.push(origin);
        }
    }
    sync(|s| {
        s.ops.insert(
            req,
            Op::Read {
                browser: id,
                left,
                cookies: Vec::new(),
                items: Vec::new(),
                error: None,
            },
        );
    });
    check_done(req);
}

/// One call of an op is over: when it was the last, the op is answered. `error` is the first
/// thing that went wrong, kept for the answer.
fn call_done(req: u32, error: Option<String>) {
    complete(req, true, error);
}

/// Nothing is outstanding of an op that sent nothing (or everything was refused): it is answered.
fn check_done(req: u32) {
    complete(req, false, None);
}

fn complete(req: u32, one_done: bool, error: Option<String>) {
    let done = sync(|s| {
        let op = s.ops.get_mut(&req)?;
        let (left, slot) = match op {
            Op::Read { left, error, .. } | Op::Write { left, error, .. } => (left, error),
        };
        if one_done {
            *left = left.saturating_sub(1);
        }
        if slot.is_none() {
            *slot = error;
        }
        if *left == 0 { s.ops.remove(&req) } else { None }
    });
    if let Some(op) = done {
        answer(req, op);
    }
}

fn answer(req: u32, op: Op) {
    match op {
        Op::Read {
            browser,
            cookies,
            items,
            error,
            ..
        } => {
            let mut records = cookies;
            records.extend(items);
            emit(Event::ProfileRecords {
                browser,
                req,
                records,
                error,
            });
        }
        Op::Write {
            browser,
            deferred,
            error,
            ..
        } => emit(Event::ProfileWritten {
            browser,
            req,
            deferred,
            error,
        }),
    }
}

// ---- writing ----

pub fn write(id: u32, req: u32, records: Vec<Record>) {
    let Some(dir) = dir_of(id) else {
        emit(Event::ProfileWritten {
            browser: id,
            req,
            deferred: 0,
            error: Some("the profile is not kept on disk".into()),
        });
        return;
    };
    ensure_domains(&dir);
    let mut sets = Vec::new();
    let mut deletes = Vec::new();
    let mut items: Vec<(u32, &'static str, Value)> = Vec::new();
    let mut deferred = 0;
    let mut error = None;
    let mut seeds_changed = false;
    for record in records {
        match record {
            Record::Cookie { .. } => {
                let Some((cookie, value)) = records::cookie_id(&record) else {
                    continue;
                };
                let known = sync(|s| s.profiles.entry(dir.clone()).or_default().cookies.is_some());
                match value {
                    Some(value) => {
                        let partition = cookie.partition.clone().map(|(site, cross_site)| {
                            browser_ipc::message::Partition { site, cross_site }
                        });
                        sets.push(records::set_params(
                            &cookie.domain,
                            &cookie.path,
                            &cookie.name,
                            partition.as_ref(),
                            value,
                        ));
                        if known {
                            let value = value.clone();
                            sync(|s| {
                                if let Some(base) =
                                    s.profiles.entry(dir.clone()).or_default().cookies.as_mut()
                                {
                                    base.insert(cookie, value);
                                }
                            });
                        }
                    }
                    None => {
                        deletes.push(records::delete_params(&cookie));
                        if known {
                            sync(|s| {
                                if let Some(base) =
                                    s.profiles.entry(dir.clone()).or_default().cookies.as_mut()
                                {
                                    base.remove(&cookie);
                                }
                            });
                        }
                    }
                }
            }
            Record::LocalStorage { origin, key, value } => {
                sync(|s| {
                    let held = s
                        .profiles
                        .entry(dir.clone())
                        .or_default()
                        .storage
                        .entry(origin.clone())
                        .or_default();
                    match &value {
                        Some(v) => held.insert(key.clone(), v.clone()),
                        None => held.remove(&key),
                    };
                });
                match browser_at(&dir, &origin) {
                    Some(b) => {
                        let id = records::storage_id(&origin);
                        match value {
                            Some(v) => items.push((
                                b,
                                "DOMStorage.setDOMStorageItem",
                                json!({"storageId": id, "key": key, "value": v}),
                            )),
                            None => items.push((
                                b,
                                "DOMStorage.removeDOMStorageItem",
                                json!({"storageId": id, "key": key}),
                            )),
                        }
                    }
                    None => {
                        sync(|s| {
                            s.profiles
                                .entry(dir.clone())
                                .or_default()
                                .seeds
                                .entry(origin)
                                .or_default()
                                .insert(key, value);
                        });
                        seeds_changed = true;
                        deferred += 1;
                    }
                }
            }
            Record::IndexedDb { .. } => {
                error.get_or_insert_with(|| "IndexedDB records go with IndexedDbWrite".to_string());
            }
        }
    }
    let mut left = 0;
    if !sets.is_empty()
        && send(
            id,
            "Network.setCookies",
            json!({"cookies": sets}),
            Call::Part { req: Some(req) },
        )
    {
        left += 1;
    }
    for params in deletes {
        if send(
            id,
            "Network.deleteCookies",
            params,
            Call::Part { req: Some(req) },
        ) {
            left += 1;
        }
    }
    for (b, method, params) in items {
        if send(b, method, params, Call::Part { req: Some(req) }) {
            left += 1;
        }
    }
    if seeds_changed {
        left += reseed(&dir, Some(req));
    }
    sync(|s| {
        s.ops.insert(
            req,
            Op::Write {
                browser: id,
                left,
                deferred,
                error,
            },
        );
    });
    check_done(req);
}

/// The seed script of every browser of the profile is replaced by one that holds what is waiting
/// now; returns how many calls were sent.
fn reseed(dir: &str, req: Option<u32>) -> usize {
    let source = sync(|s| {
        s.profiles
            .get(dir)
            .and_then(|p| records::seed_script(&p.seeds))
    });
    let mut sent = 0;
    for b in browsers_of(dir) {
        // Whatever script was asked for before this is out of date, whether or not it has been
        // named yet.
        let generation = with_entry(b, |e| {
            e.seed_gen += 1;
            e.seed_gen
        })
        .unwrap_or(0);
        if let Some(old) = with_entry(b, |e| e.seed_script.take()).flatten()
            && send(
                b,
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({"identifier": old}),
                Call::Part { req },
            )
        {
            sent += 1;
        }
        if let Some(source) = &source
            && send(
                b,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": source}),
                Call::AddScript { req, generation },
            )
        {
            sent += 1;
        }
    }
    sent
}

// ---- watching ----

pub fn watch(id: u32, on: bool) {
    let Some(dir) = dir_of(id) else { return };
    sync(|s| {
        let p = s.profiles.entry(dir.clone()).or_default();
        p.watching = on;
        if on {
            p.poll_at = Some(Instant::now());
            p.idb_at = Some(Instant::now());
        }
    });
    if on {
        ensure_domains(&dir);
    }
}

/// A page finished loading in a browser: the seeds of its origin are in (the script ran before
/// the page's own), and a watched profile reads the origin's Local Storage the first time.
pub fn page_loaded(id: u32) {
    let Some(dir) = dir_of(id) else { return };
    let Some(origin) = with_entry(id, |e| records::owner_of_url(&e.url)).flatten() else {
        return;
    };
    let (seeded, visit) = sync(|s| {
        let Some(p) = s.profiles.get_mut(&dir) else {
            return (false, false);
        };
        (
            p.seeds.remove(&origin).is_some(),
            p.watching && p.storage_read.insert(origin.clone()),
        )
    });
    if seeded {
        reseed(&dir, None);
    }
    if visit {
        let id_params = json!({"storageId": records::storage_id(&origin)});
        send(
            id,
            "DOMStorage.getDOMStorageItems",
            id_params,
            Call::VisitItems { origin },
        );
    }
}

/// An event of the protocol, from a browser.
pub fn on_event(id: u32, method: &str, params: &[u8]) {
    let storage = method.starts_with("DOMStorage.");
    let cookie = method == "Network.responseReceivedExtraInfo";
    if !storage && !cookie {
        return;
    }
    let Some(dir) = dir_of(id) else { return };
    let json: Value = serde_json::from_slice(params).unwrap_or(Value::Null);
    if cookie {
        let sets_one = json["headers"]
            .as_object()
            .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("set-cookie")));
        if sets_one {
            sync(|s| {
                if let Some(p) = s.profiles.get_mut(&dir).filter(|p| p.watching) {
                    p.poll_at = Some(Instant::now());
                }
            });
        }
        return;
    }
    let changes = sync(|s| match s.profiles.get_mut(&dir) {
        Some(p) if p.watching => records::storage_event(&mut p.storage, method, &json),
        _ => Vec::new(),
    });
    if !changes.is_empty() {
        emit(Event::ProfileChanged {
            browser: id,
            records: changes,
            stage: None,
        });
    }
}

/// The answer to a call of this module's. `false` when the call is not one of its.
pub fn on_result(id: u32, message_id: i32, success: bool, bytes: &[u8]) -> bool {
    let Some(Some(call)) = with_entry(id, |e| e.syncs.remove(&message_id)) else {
        return false;
    };
    let json: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let why = || {
        json["message"]
            .as_str()
            .unwrap_or("the protocol call failed")
            .to_string()
    };
    trace!("sync {id}#{message_id} -> ok={success}");
    match call {
        Call::Quiet => {}
        Call::Clear { key, what } => crate::clear::on_result(id, key, what, success, &json),
        Call::PollCookies => {
            let Some(dir) = dir_of(id) else { return true };
            let changes = sync(|s| {
                let p = s.profiles.get_mut(&dir)?;
                p.polling = false;
                p.poll_at = Some(Instant::now() + POLL);
                if !success {
                    return None;
                }
                let now = records::cookies_of(&json);
                match &mut p.cookies {
                    None => {
                        p.cookies = Some(records::cookie_base(&now));
                        None
                    }
                    Some(base) => {
                        let changes = records::cookie_changes(base, now, now_secs());
                        (p.watching && !changes.is_empty()).then_some(changes)
                    }
                }
            });
            if let Some(records) = changes {
                emit(Event::ProfileChanged {
                    browser: id,
                    records,
                    stage: None,
                });
            }
        }
        Call::ReadCookies { req } => {
            if success {
                let now = records::cookies_of(&json);
                if let Some(dir) = dir_of(id) {
                    sync(|s| {
                        s.profiles.entry(dir).or_default().cookies =
                            Some(records::cookie_base(&now))
                    });
                }
                sync(|s| {
                    if let Some(Op::Read { cookies, .. }) = s.ops.get_mut(&req) {
                        *cookies = now;
                    }
                });
                call_done(req, None);
            } else {
                call_done(req, Some(why()));
            }
        }
        Call::ReadItems { req, origin } => {
            if success {
                let items = records::items_of(&json);
                if let Some(dir) = dir_of(id) {
                    let map: Items = items.iter().cloned().collect();
                    let origin = origin.clone();
                    sync(|s| {
                        let p = s.profiles.entry(dir).or_default();
                        p.storage.insert(origin.clone(), map);
                        p.storage_read.insert(origin);
                    });
                }
                sync(|s| {
                    if let Some(Op::Read { items: out, .. }) = s.ops.get_mut(&req) {
                        out.extend(items.into_iter().map(|(key, value)| Record::LocalStorage {
                            origin: origin.clone(),
                            key,
                            value: Some(value),
                        }));
                    }
                });
                call_done(req, None);
            } else {
                call_done(req, Some(why()));
            }
        }
        Call::VisitItems { origin } => {
            let Some(dir) = dir_of(id) else { return true };
            if !success {
                // Asked again at the next page of the origin.
                sync(|s| {
                    if let Some(p) = s.profiles.get_mut(&dir) {
                        p.storage_read.remove(&origin);
                    }
                });
                return true;
            }
            let items = records::items_of(&json);
            let changes = sync(|s| {
                let p = s.profiles.get_mut(&dir)?;
                Some(records::storage_changes(&mut p.storage, &origin, items))
                    .filter(|c| p.watching && !c.is_empty())
            });
            if let Some(records) = changes {
                emit(Event::ProfileChanged {
                    browser: id,
                    records,
                    stage: None,
                });
            }
        }
        Call::Part { req } => {
            if !success && let Some(dir) = dir_of(id) {
                // What was written was put in the base before the browser took it: a call that
                // failed leaves a base that has what the browser lacks, and the next read would
                // call that a deletion by the page and say so to every other computer. The bases
                // are taken again from the browser as it is.
                sync(|s| {
                    if let Some(p) = s.profiles.get_mut(&dir) {
                        p.cookies = None;
                        p.storage.clear();
                        p.storage_read.clear();
                    }
                });
            }
            if let Some(req) = req {
                call_done(req, (!success).then(why));
            }
        }
        Call::AddScript { req, generation } => {
            if success && let Some(script) = json["identifier"].as_str() {
                let script = script.to_string();
                let current = with_entry(id, |e| e.seed_gen == generation).unwrap_or(false);
                if current {
                    with_entry(id, |e| e.seed_script = Some(script));
                } else {
                    // A newer script was asked for while this one was on its way: this one would
                    // put the older items back on every page that opens.
                    send(
                        id,
                        "Page.removeScriptToEvaluateOnNewDocument",
                        json!({"identifier": script}),
                        Call::Quiet,
                    );
                }
            }
            if let Some(req) = req {
                call_done(req, (!success).then(why));
            }
        }
    }
    true
}

/// A write or a read that could not be taken (its browser never came up): answered at once.
/// `false` for a command of another kind.
pub fn refuse(id: u32, command: &Command, why: &str) -> bool {
    match command {
        Command::ProfileRead { req, .. } => emit(Event::ProfileRecords {
            browser: id,
            req: *req,
            records: Vec::new(),
            error: Some(why.into()),
        }),
        Command::ProfileWrite { req, .. } => emit(Event::ProfileWritten {
            browser: id,
            req: *req,
            deferred: 0,
            error: Some(why.into()),
        }),
        _ => return false,
    }
    true
}

// ---- time ----

/// When `tick` next has something to do.
pub fn next_due() -> Option<Instant> {
    sync(|s| {
        s.profiles
            .values()
            .filter(|p| p.watching)
            .flat_map(|p| [if p.polling { None } else { p.poll_at }, p.idb_at])
            .flatten()
            .min()
    })
}

pub fn tick() {
    let now = Instant::now();
    let due: Vec<(String, bool, bool)> = sync(|s| {
        s.profiles
            .iter_mut()
            .filter(|(_, p)| p.watching)
            .filter_map(|(dir, p)| {
                let cookies = !p.polling && p.poll_at.is_some_and(|t| t <= now);
                let idb = p.idb_at.is_some_and(|t| t <= now);
                if cookies {
                    p.poll_at = None;
                }
                if idb {
                    p.idb_at = Some(now + POLL);
                }
                (cookies || idb).then(|| (dir.clone(), cookies, idb))
            })
            .collect()
    });
    for (dir, cookies, idb) in due {
        if cookies {
            let sent = browsers_of(&dir)
                .first()
                .is_some_and(|b| send(*b, "Network.getAllCookies", json!({}), Call::PollCookies));
            sync(|s| {
                if let Some(p) = s.profiles.get_mut(&dir) {
                    if sent {
                        p.polling = true;
                    } else {
                        p.poll_at = Some(now + POLL);
                    }
                }
            });
        }
        if idb {
            look_at_folders(&dir, now);
        }
    }
}

fn profile_path(dir: &str) -> PathBuf {
    with(|s| s.root.join(dir))
}

/// The engine's major version, as the folders' records say it.
fn major() -> u16 {
    crate::chromium_version()
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

fn new_stage() -> (String, PathBuf) {
    let n = sync(|s| {
        s.next_stage += 1;
        s.next_stage
    });
    let name = format!("{STAGES}/{n}");
    let path = with(|s| s.root.join(&name));
    (name, path)
}

/// The folders of IndexedDB are looked at: the first look is the base, later a folder that
/// changed and then lay still for `QUIET` is copied and reported, and one that is gone is
/// reported as gone.
fn look_at_folders(dir: &str, now: Instant) {
    let profile = profile_path(dir);
    let names = records::idb_origins(&profile);
    let mut ready: Vec<String> = Vec::new();
    let mut gone: Vec<String> = Vec::new();
    sync(|s| {
        let Some(p) = s.profiles.get_mut(dir) else {
            return;
        };
        let mut now_sigs: HashMap<String, u64> = HashMap::new();
        for name in &names {
            if let Some(sig) = records::idb_signature(&profile, name) {
                now_sigs.insert(name.clone(), sig);
            }
        }
        if !p.idb_known {
            p.idb = now_sigs;
            p.idb_known = true;
            return;
        }
        for (name, sig) in &now_sigs {
            if p.idb.get(name) == Some(sig) {
                p.idb_moving.remove(name);
                continue;
            }
            match p.idb_moving.get(name) {
                Some((seen, since)) if seen == sig => {
                    if now.duration_since(*since) >= QUIET {
                        ready.push(name.clone());
                    }
                }
                _ => {
                    p.idb_moving.insert(name.clone(), (*sig, now));
                }
            }
        }
        for name in p.idb.keys() {
            if !now_sigs.contains_key(name) {
                gone.push(name.clone());
            }
        }
    });
    if ready.is_empty() && gone.is_empty() {
        return;
    }
    // A change nobody can carry is left as it is: the folders are looked at again when a browser
    // is there, or the change would be taken for the base and never said.
    let Some(browser) = browsers_of(dir).first().copied() else {
        return;
    };
    let (stage_name, stage) = new_stage();
    let mut records = Vec::new();
    for name in ready {
        match records::idb_copy_out(&profile, &name, &stage) {
            Ok((sig, files)) => {
                sync(|s| {
                    if let Some(p) = s.profiles.get_mut(dir) {
                        p.idb.insert(name.clone(), sig);
                        p.idb_moving.remove(&name);
                    }
                });
                records.push(records::idb_record(&name, major(), files));
            }
            Err(e) => trace!("the folders of {name} were not copied: {e}"),
        }
    }
    for name in gone {
        sync(|s| {
            if let Some(p) = s.profiles.get_mut(dir) {
                p.idb.remove(&name);
                p.idb_moving.remove(&name);
            }
        });
        records.push(Record::IndexedDb {
            origin: name,
            value: None,
        });
    }
    if records.is_empty() {
        let _ = std::fs::remove_dir_all(&stage);
        return;
    }
    emit(Event::ProfileChanged {
        browser,
        records,
        stage: Some(stage_name),
    });
}

// ---- IndexedDB by folder ----

pub fn idb_read(req: u32, dir: String) {
    let fail = |why: String| {
        emit(Event::IndexedDbRecords {
            req,
            records: Vec::new(),
            stage: String::new(),
            error: Some(why),
        })
    };
    if !crate::rules::profile_dir_ok(&dir) {
        return fail(format!("not a profile folder name: {dir:?}"));
    }
    let profile = profile_path(&dir);
    let (stage_name, stage) = new_stage();
    let mut records = Vec::new();
    let mut error = None;
    let mut sigs = HashMap::new();
    for name in records::idb_origins(&profile) {
        match records::idb_copy_out(&profile, &name, &stage) {
            Ok((sig, files)) => {
                sigs.insert(name.clone(), sig);
                records.push(records::idb_record(&name, major(), files));
            }
            Err(e) => {
                error.get_or_insert(format!("{name}: {e}"));
            }
        }
    }
    sync(|s| {
        let p = s.profiles.entry(dir).or_default();
        p.idb.extend(sigs);
        p.idb_known = true;
    });
    emit(Event::IndexedDbRecords {
        req,
        records,
        stage: stage_name,
        error,
    });
}

/// A stage name from a message: one of ours, below the stages' folder, nothing else.
fn stage_path(stage: &str) -> Option<PathBuf> {
    let name = stage.strip_prefix(&format!("{STAGES}/"))?;
    records::safe_origin(name).then(|| with(|s| s.root.join(STAGES).join(name)))
}

pub fn idb_write(req: u32, dir: String, stage: String, records: Vec<Record>) {
    let fail = |why: String| {
        emit(Event::IndexedDbWritten {
            req,
            written: 0,
            deferred: 0,
            error: Some(why),
        })
    };
    if !crate::rules::profile_dir_ok(&dir) {
        return fail(format!("not a profile folder name: {dir:?}"));
    }
    let Some(stage) = stage_path(&stage) else {
        return fail("not a stage of the helper's".into());
    };
    // An engine that has the profile open keeps the stores it opened and does not see a folder
    // put under it.
    let running = with(|s| {
        s.contexts
            .values()
            .any(|c| c.dir.as_deref() == Some(dir.as_str()))
    });
    let profile = profile_path(&dir);
    let (mut written, mut deferred, mut error) = (0, 0, None);
    for record in records {
        let Record::IndexedDb { origin, value } = record else {
            error.get_or_insert_with(|| "only IndexedDB records are taken here".to_string());
            continue;
        };
        if running {
            deferred += 1;
            continue;
        }
        match value {
            None => {
                records::idb_remove(&profile, &origin);
                sync(|s| {
                    if let Some(p) = s.profiles.get_mut(&dir) {
                        p.idb.remove(&origin);
                    }
                });
                written += 1;
            }
            Some(v) if v.format != records::IDB_FORMAT || v.major > major() => deferred += 1,
            Some(v) => match records::idb_copy_in(&profile, &origin, &stage, &v.files) {
                Ok(()) => {
                    if let Some(sig) = records::idb_signature(&profile, &origin) {
                        sync(|s| {
                            s.profiles
                                .entry(dir.clone())
                                .or_default()
                                .idb
                                .insert(origin, sig);
                        });
                    }
                    written += 1;
                }
                Err(e) => {
                    error.get_or_insert(format!("{origin}: {e}"));
                }
            },
        }
    }
    emit(Event::IndexedDbWritten {
        req,
        written,
        deferred,
        error,
    });
}

pub fn stage_drop(stage: &str) {
    if let Some(path) = stage_path(stage) {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// The helper starts: the copies of an earlier run are not anyone's any more.
pub fn clear_stages(root: &std::path::Path) {
    let _ = std::fs::remove_dir_all(root.join(STAGES));
}
