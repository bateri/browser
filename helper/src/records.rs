//! A profile's data as records (`browser_ipc::message::Record`): what the engine's protocol says
//! turned into records and back, what a record is next to the base it is compared with, and the
//! folders of IndexedDB. Nothing here calls the engine, so all of it is tested without one.

use browser_ipc::message::{CookieValue, IndexedDbFile, IndexedDbValue, Partition, Record};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

/// What the engine calls the format of the IndexedDB folders it writes (LevelDB under the
/// origin's folder). A folder of another kind is not a record.
pub const IDB_FORMAT: &str = "chromium-idb-leveldb";

/// Longest path of a file in an IndexedDB stage, and the deepest folder it may be in.
const MAX_PATH: usize = 200;
const MAX_DEPTH: usize = 8;

// ---- cookies ----

/// What tells one cookie from another.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CookieId {
    pub domain: String,
    pub path: String,
    pub name: String,
    pub partition: Option<(String, bool)>,
}

impl CookieId {
    pub fn record(&self, value: Option<CookieValue>) -> Record {
        Record::Cookie {
            domain: self.domain.clone(),
            path: self.path.clone(),
            name: self.name.clone(),
            partition: self
                .partition
                .clone()
                .map(|(site, cross_site)| Partition { site, cross_site }),
            value,
        }
    }
}

pub fn cookie_id(record: &Record) -> Option<(CookieId, Option<&CookieValue>)> {
    match record {
        Record::Cookie {
            domain,
            path,
            name,
            partition,
            value,
        } => Some((
            CookieId {
                domain: domain.clone(),
                path: path.clone(),
                name: name.clone(),
                partition: partition.as_ref().map(|p| (p.site.clone(), p.cross_site)),
            },
            value.as_ref(),
        )),
        _ => None,
    }
}

fn same_site_code(name: Option<&str>) -> u8 {
    match name {
        Some("None") => 1,
        Some("Lax") => 2,
        Some("Strict") => 3,
        _ => 0,
    }
}

fn same_site_name(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("None"),
        2 => Some("Lax"),
        3 => Some("Strict"),
        _ => None,
    }
}

fn priority_code(name: Option<&str>) -> u8 {
    match name {
        Some("Low") => 0,
        Some("High") => 2,
        _ => 1,
    }
}

fn priority_name(code: u8) -> &'static str {
    match code {
        0 => "Low",
        2 => "High",
        _ => "Medium",
    }
}

fn scheme_code(name: Option<&str>) -> u8 {
    match name {
        Some("NonSecure") => 1,
        Some("Secure") => 2,
        _ => 0,
    }
}

fn scheme_name(code: u8) -> &'static str {
    match code {
        1 => "NonSecure",
        2 => "Secure",
        _ => "Unset",
    }
}

/// The cookies a `Network.getAllCookies` answer holds, as records.
pub fn cookies_of(answer: &Value) -> Vec<Record> {
    let Some(list) = answer["cookies"].as_array() else {
        return Vec::new();
    };
    list.iter().filter_map(cookie_of).collect()
}

fn cookie_of(cookie: &Value) -> Option<Record> {
    let partition = match &cookie["partitionKey"] {
        Value::String(site) => Some(Partition {
            site: site.clone(),
            cross_site: false,
        }),
        Value::Object(key) => Some(Partition {
            site: key.get("topLevelSite")?.as_str()?.to_string(),
            cross_site: key
                .get("hasCrossSiteAncestor")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    };
    let session = cookie["session"].as_bool().unwrap_or(false);
    let expires = cookie["expires"].as_f64().filter(|e| !session && *e > 0.0);
    Some(Record::Cookie {
        domain: cookie["domain"].as_str()?.to_string(),
        path: cookie["path"].as_str()?.to_string(),
        name: cookie["name"].as_str()?.to_string(),
        partition,
        value: Some(CookieValue {
            value: cookie["value"].as_str()?.to_string(),
            expires,
            secure: cookie["secure"].as_bool().unwrap_or(false),
            http_only: cookie["httpOnly"].as_bool().unwrap_or(false),
            same_site: same_site_code(cookie["sameSite"].as_str()),
            priority: priority_code(cookie["priority"].as_str()),
            source_scheme: scheme_code(cookie["sourceScheme"].as_str()),
            source_port: cookie["sourcePort"]
                .as_i64()
                .and_then(|p| i32::try_from(p).ok())
                .unwrap_or(-1),
        }),
    })
}

fn partition_key(partition: &Partition) -> Value {
    json!({
        "topLevelSite": partition.site,
        "hasCrossSiteAncestor": partition.cross_site,
    })
}

/// One cookie as the protocol's `Network.setCookies` takes it.
pub fn set_params(
    domain: &str,
    path: &str,
    name: &str,
    partition: Option<&Partition>,
    value: &CookieValue,
) -> Value {
    let mut cookie = json!({
        "name": name,
        "value": value.value,
        "domain": domain,
        "path": path,
        "secure": value.secure,
        "httpOnly": value.http_only,
        "priority": priority_name(value.priority),
        "sourceScheme": scheme_name(value.source_scheme),
    });
    let map = cookie.as_object_mut().expect("an object");
    if let Some(site) = same_site_name(value.same_site) {
        map.insert("sameSite".into(), json!(site));
    }
    if let Some(expires) = value.expires {
        map.insert("expires".into(), json!(expires));
    }
    if value.source_port >= 0 {
        map.insert("sourcePort".into(), json!(value.source_port));
    }
    if let Some(partition) = partition {
        map.insert("partitionKey".into(), partition_key(partition));
    }
    cookie
}

/// The parameters of the protocol's `Network.deleteCookies` for one cookie.
pub fn delete_params(id: &CookieId) -> Value {
    let mut params = json!({"name": id.name, "domain": id.domain, "path": id.path});
    if let Some((site, cross_site)) = &id.partition {
        params["partitionKey"] = partition_key(&Partition {
            site: site.clone(),
            cross_site: *cross_site,
        });
    }
    params
}

/// What the cookies of `now` change in `base`: the base is brought up to date and the records that
/// say how come back. A cookie that is gone because its time ran out is no deletion (every
/// machine's engine lets it go by itself); `at` is the time, in seconds since the epoch.
pub fn cookie_changes(
    base: &mut HashMap<CookieId, CookieValue>,
    now: Vec<Record>,
    at: f64,
) -> Vec<Record> {
    let mut changes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for record in &now {
        let Some((id, Some(value))) = cookie_id(record) else {
            continue;
        };
        seen.insert(id.clone());
        if base.get(&id) != Some(value) {
            base.insert(id, value.clone());
            changes.push(record.clone());
        }
    }
    let gone: Vec<CookieId> = base
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    for id in gone {
        let value = base.remove(&id);
        let expired = value
            .as_ref()
            .and_then(|v| v.expires)
            .is_some_and(|e| e <= at);
        if !expired {
            changes.push(id.record(None));
        }
    }
    changes
}

/// The base that `now` is, taken whole.
pub fn cookie_base(now: &[Record]) -> HashMap<CookieId, CookieValue> {
    now.iter()
        .filter_map(cookie_id)
        .filter_map(|(id, value)| value.map(|v| (id, v.clone())))
        .collect()
}

// ---- Local Storage ----

/// The owner of a storage in a protocol `StorageId` (`isLocalStorage` only).
pub fn storage_owner(id: &Value) -> Option<String> {
    if !id["isLocalStorage"].as_bool().unwrap_or(false) {
        return None;
    }
    if let Some(key) = id["storageKey"].as_str() {
        return Some(key.to_string());
    }
    id["securityOrigin"].as_str().map(|o| format!("{o}/"))
}

/// The `StorageId` of a Local Storage the protocol is asked about.
pub fn storage_id(origin: &str) -> Value {
    json!({"storageKey": origin, "isLocalStorage": true})
}

/// The address's origin as a Local Storage's owner is named (`https://example.com/`), for the
/// two schemes the helper loads.
pub fn owner_of_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    // A name with credentials has no place in an owner.
    let host = host.rsplit('@').next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}/"))
}

pub type Items = BTreeMap<String, String>;

/// A whole Local Storage read from the engine against what the base held: the base becomes it and
/// the records that say how come back.
pub fn storage_changes(
    base: &mut HashMap<String, Items>,
    origin: &str,
    items: Vec<(String, String)>,
) -> Vec<Record> {
    let now: Items = items.into_iter().collect();
    let held = base.entry(origin.to_string()).or_default();
    let mut changes = Vec::new();
    for (key, value) in &now {
        if held.get(key) != Some(value) {
            changes.push(item(origin, key, Some(value.clone())));
        }
    }
    for key in held.keys() {
        if !now.contains_key(key) {
            changes.push(item(origin, key, None));
        }
    }
    *held = now;
    changes
}

fn item(origin: &str, key: &str, value: Option<String>) -> Record {
    Record::LocalStorage {
        origin: origin.into(),
        key: key.into(),
        value,
    }
}

/// What one of the protocol's storage events (`DOMStorage.domStorageItemAdded` and the others)
/// changes in the base: nothing when the base already says so (a write of the helper's own, or
/// the same change seen from a second page of the origin).
pub fn storage_event(
    base: &mut HashMap<String, Items>,
    method: &str,
    params: &Value,
) -> Vec<Record> {
    let Some(origin) = storage_owner(&params["storageId"]) else {
        return Vec::new();
    };
    let held = base.entry(origin.clone()).or_default();
    match method {
        "DOMStorage.domStorageItemAdded" | "DOMStorage.domStorageItemUpdated" => {
            let (Some(key), Some(value)) = (params["key"].as_str(), params["newValue"].as_str())
            else {
                return Vec::new();
            };
            if held.get(key).map(String::as_str) == Some(value) {
                return Vec::new();
            }
            held.insert(key.into(), value.into());
            vec![item(&origin, key, Some(value.into()))]
        }
        "DOMStorage.domStorageItemRemoved" => {
            let Some(key) = params["key"].as_str() else {
                return Vec::new();
            };
            // A key the base never held is a removal the home may not know: it says so.
            held.remove(key);
            vec![item(&origin, key, None)]
        }
        "DOMStorage.domStorageItemsCleared" => {
            let gone: Vec<String> = held.keys().cloned().collect();
            held.clear();
            gone.iter().map(|key| item(&origin, key, None)).collect()
        }
        _ => Vec::new(),
    }
}

/// The items a `DOMStorage.getDOMStorageItems` answer holds.
pub fn items_of(answer: &Value) -> Vec<(String, String)> {
    answer["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| Some((e[0].as_str()?.to_string(), e[1].as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// The script that puts items into the Local Storage of the page it runs in, before any script of
/// the page does: `seeds` are the items waiting for the first page of their origin, `None` a key
/// to remove. `None` when there is nothing to put.
pub fn seed_script(seeds: &BTreeMap<String, BTreeMap<String, Option<String>>>) -> Option<String> {
    if seeds.is_empty() {
        return None;
    }
    let table: serde_json::Map<String, Value> = seeds
        .iter()
        .map(|(origin, items)| {
            let list: Vec<Value> = items.iter().map(|(k, v)| json!([k, v])).collect();
            (origin.clone(), Value::Array(list))
        })
        .collect();
    Some(format!(
        "(()=>{{try{{const t={};const l=t[location.origin+'/'];if(!l)return;\
for(const [k,v] of l){{if(v===null)localStorage.removeItem(k);else localStorage.setItem(k,v);}}}}catch(e){{}}}})()",
        Value::Object(table)
    ))
}

// ---- IndexedDB ----

const LEVELDB: &str = ".indexeddb.leveldb";
const BLOB: &str = ".indexeddb.blob";

/// A name of an origin's folder the helper puts on disk: one component, nothing hidden.
pub fn safe_origin(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PATH
        && !name.starts_with('.')
        && !name.contains("..")
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
}

/// A file's path in an origin's stage: `leveldb/…` or `blob/…`, no way out of the folder.
pub fn safe_file(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_PATH || path.contains(['\\', '\0']) {
        return false;
    }
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() >= 2
        && parts.len() <= MAX_DEPTH
        && matches!(parts[0], "leveldb" | "blob")
        && parts
            .iter()
            .all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.chars().any(char::is_control))
}

fn idb_dir(profile: &Path) -> PathBuf {
    profile.join("IndexedDB")
}

/// The origins the profile's IndexedDB holds a folder for, by the folder's name.
pub fn idb_origins(profile: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(idb_dir(profile)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(LEVELDB))
                .map(str::to_string)
        })
        .filter(|n| safe_origin(n))
        .collect();
    names.sort();
    names
}

/// The files of one folder below `base`, by their path from it (a folder that is not there has
/// none). Links are not followed and not taken.
fn files_below(base: &Path) -> Vec<(String, std::fs::Metadata)> {
    let mut out = Vec::new();
    let mut stack = vec![(base.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.filter_map(Result::ok) {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if meta.is_dir() {
                stack.push((entry.path(), rel));
            } else if meta.is_file() && name != "LOCK" {
                out.push((rel, meta));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// What an origin's folders look like now: changes when a file is added, removed, resized or
/// written, and not otherwise. `None` when the origin has no folder.
pub fn idb_signature(profile: &Path, origin: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let level = idb_dir(profile).join(format!("{origin}{LEVELDB}"));
    if !level.is_dir() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (prefix, base) in [
        ("leveldb", level),
        ("blob", idb_dir(profile).join(format!("{origin}{BLOB}"))),
    ] {
        for (rel, meta) in files_below(&base) {
            prefix.hash(&mut hasher);
            rel.hash(&mut hasher);
            meta.len().hash(&mut hasher);
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .hash(&mut hasher);
        }
    }
    Some(hasher.finish())
}

fn make_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

fn copy_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = to.parent() {
        make_dir(parent)?;
    }
    let mut input = std::fs::File::open(from)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    io::copy(&mut input, &mut output)?;
    Ok(())
}

/// Copies an origin's folders into `stage/<origin>/{leveldb,blob}` and names the files. The engine
/// may be writing: the copy is taken again when the folders changed while it ran (a few times),
/// and it is the one the signature before and after agree on.
pub fn idb_copy_out(
    profile: &Path,
    origin: &str,
    stage: &Path,
) -> io::Result<(u64, Vec<IndexedDbFile>)> {
    for _ in 0..4 {
        let Some(before) = idb_signature(profile, origin) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such folder"));
        };
        let target = stage.join(origin);
        let _ = std::fs::remove_dir_all(&target);
        let mut files = Vec::new();
        let mut broke = false;
        for (prefix, base) in [
            (
                "leveldb",
                idb_dir(profile).join(format!("{origin}{LEVELDB}")),
            ),
            ("blob", idb_dir(profile).join(format!("{origin}{BLOB}"))),
        ] {
            for (rel, meta) in files_below(&base) {
                let path = format!("{prefix}/{rel}");
                if !safe_file(&path) {
                    continue;
                }
                match copy_file(&base.join(&rel), &target.join(&path)) {
                    Ok(()) => files.push(IndexedDbFile {
                        path,
                        size: meta.len(),
                    }),
                    // A file the engine removed while it was being copied: the folders changed.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => broke = true,
                    Err(e) => return Err(e),
                }
            }
        }
        if !broke && idb_signature(profile, origin) == Some(before) {
            // The size of a file written to while it was copied is the size copied.
            for file in &mut files {
                if let Ok(meta) = std::fs::metadata(target.join(&file.path)) {
                    file.size = meta.len();
                }
            }
            return Ok((before, files));
        }
    }
    Err(io::Error::other(
        "the folder kept changing while it was copied",
    ))
}

/// The record of an origin copied into a stage.
pub fn idb_record(origin: &str, major: u16, files: Vec<IndexedDbFile>) -> Record {
    Record::IndexedDb {
        origin: origin.into(),
        value: Some(IndexedDbValue {
            format: IDB_FORMAT.into(),
            major,
            files,
        }),
    }
}

/// Puts the files a record names from `stage/<origin>/` in place of the origin's folders in the
/// profile. The new folders are made beside the old and swapped in, so a failure leaves the old.
pub fn idb_copy_in(
    profile: &Path,
    origin: &str,
    stage: &Path,
    files: &[IndexedDbFile],
) -> io::Result<()> {
    if !safe_origin(origin) || !files.iter().all(|f| safe_file(&f.path)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a name that is no folder's",
        ));
    }
    let idb = idb_dir(profile);
    make_dir(&idb)?;
    let source = stage.join(origin);
    for (prefix, suffix) in [("leveldb", LEVELDB), ("blob", BLOB)] {
        let target = idb.join(format!("{origin}{suffix}"));
        let fresh = idb.join(format!(".{origin}{suffix}.new"));
        let _ = std::fs::remove_dir_all(&fresh);
        make_dir(&fresh)?;
        let mut any = false;
        for file in files
            .iter()
            .filter(|f| f.path.split('/').next() == Some(prefix))
        {
            let rel = file.path.split_once('/').map(|(_, r)| r).unwrap_or("");
            copy_file(&source.join(&file.path), &fresh.join(rel))?;
            any = true;
        }
        let _ = std::fs::remove_dir_all(&target);
        if any || prefix == "leveldb" {
            std::fs::rename(&fresh, &target)?;
        } else {
            let _ = std::fs::remove_dir_all(&fresh);
        }
    }
    Ok(())
}

/// Removes an origin's folders from the profile.
pub fn idb_remove(profile: &Path, origin: &str) {
    if !safe_origin(origin) {
        return;
    }
    let idb = idb_dir(profile);
    let _ = std::fs::remove_dir_all(idb.join(format!("{origin}{LEVELDB}")));
    let _ = std::fs::remove_dir_all(idb.join(format!("{origin}{BLOB}")));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, expires: Option<f64>) -> Record {
        Record::Cookie {
            domain: "example.com".into(),
            path: "/".into(),
            name: name.into(),
            partition: None,
            value: Some(CookieValue {
                value: value.into(),
                expires,
                secure: false,
                http_only: false,
                same_site: 0,
                priority: 1,
                source_scheme: 0,
                source_port: -1,
            }),
        }
    }

    #[test]
    fn what_the_protocol_says_of_a_cookie_is_a_record_and_back() {
        let answer = json!({"cookies": [{
            "name": "sid", "value": "v", "domain": ".example.com", "path": "/", "expires": 1900000000.5,
            "size": 4, "httpOnly": true, "secure": true, "session": false, "sameSite": "Lax",
            "priority": "High", "sourceScheme": "Secure", "sourcePort": 443,
            "partitionKey": {"topLevelSite": "https://a.test", "hasCrossSiteAncestor": true}
        }, {
            "name": "tmp", "value": "x", "domain": "localhost", "path": "/", "expires": -1.0,
            "session": true, "httpOnly": false, "secure": false
        }]});
        let records = cookies_of(&answer);
        assert_eq!(records.len(), 2);
        let Record::Cookie {
            partition, value, ..
        } = &records[0]
        else {
            panic!("not a cookie")
        };
        let value = value.as_ref().unwrap();
        assert_eq!(
            partition,
            &Some(Partition {
                site: "https://a.test".into(),
                cross_site: true
            })
        );
        assert_eq!(
            (value.same_site, value.priority, value.source_scheme),
            (2, 2, 2)
        );
        assert_eq!(value.expires, Some(1900000000.5));
        // A cookie of the session has no end, whatever number the engine puts in `expires`.
        let Record::Cookie { value, .. } = &records[1] else {
            panic!("not a cookie")
        };
        assert_eq!(value.as_ref().unwrap().expires, None);

        let Record::Cookie {
            domain,
            path,
            name,
            partition,
            value: Some(value),
        } = &records[0]
        else {
            panic!("not a cookie")
        };
        let params = set_params(domain, path, name, partition.as_ref(), value);
        assert_eq!(params["sameSite"], "Lax");
        assert_eq!(params["partitionKey"]["topLevelSite"], "https://a.test");
        assert_eq!(params["partitionKey"]["hasCrossSiteAncestor"], true);
        assert_eq!(params["priority"], "High");
        assert_eq!(params["sourcePort"], 443);
        // Written and read again, it is the same record.
        let again = cookies_of(&json!({"cookies": [params]}));
        assert_eq!(again[0], records[0]);
    }

    #[test]
    fn a_cookie_that_ran_out_is_not_a_deletion_and_one_that_was_deleted_is() {
        let mut base = cookie_base(&[
            cookie("old", "1", Some(100.0)),
            cookie("kept", "1", Some(5000.0)),
            cookie("gone", "1", None),
        ]);
        let changes = cookie_changes(&mut base, vec![cookie("kept", "1", Some(5000.0))], 200.0);
        assert_eq!(changes.len(), 1, "{changes:?}");
        let (id, value) = cookie_id(&changes[0]).unwrap();
        assert_eq!((id.name.as_str(), value), ("gone", None));
        // Nothing is said twice.
        assert!(
            cookie_changes(&mut base, vec![cookie("kept", "1", Some(5000.0))], 200.0).is_empty()
        );
        // A new value is said once.
        let changes = cookie_changes(&mut base, vec![cookie("kept", "2", Some(5000.0))], 200.0);
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn a_change_the_base_already_holds_is_not_reported() {
        let mut base = HashMap::new();
        let items = vec![("a".to_string(), "1".to_string())];
        assert_eq!(
            storage_changes(&mut base, "https://x.test/", items.clone()).len(),
            1
        );
        assert!(storage_changes(&mut base, "https://x.test/", items).is_empty());
        let id = json!({"storageKey": "https://x.test/", "isLocalStorage": true});
        // The echo of the helper's own write.
        let echo = json!({"storageId": id, "key": "a", "newValue": "1"});
        assert!(storage_event(&mut base, "DOMStorage.domStorageItemUpdated", &echo).is_empty());
        let local = json!({"storageId": id, "key": "a", "newValue": "2"});
        assert_eq!(
            storage_event(&mut base, "DOMStorage.domStorageItemUpdated", &local).len(),
            1
        );
        let removed = json!({"storageId": id, "key": "a"});
        assert_eq!(
            storage_event(&mut base, "DOMStorage.domStorageItemRemoved", &removed),
            vec![Record::LocalStorage {
                origin: "https://x.test/".into(),
                key: "a".into(),
                value: None
            }]
        );
        // The session storage is not this.
        let session = json!({"storageId": {"storageKey": "https://x.test/", "isLocalStorage": false}, "key": "a", "newValue": "3"});
        assert!(storage_event(&mut base, "DOMStorage.domStorageItemAdded", &session).is_empty());
    }

    #[test]
    fn an_owner_is_the_scheme_and_host_with_a_slash() {
        assert_eq!(
            owner_of_url("http://page.test:8080/a?b#c").as_deref(),
            Some("http://page.test:8080/")
        );
        assert_eq!(
            owner_of_url("https://u:p@h.test/x").as_deref(),
            Some("https://h.test/")
        );
        assert_eq!(owner_of_url("about:blank"), None);
        assert_eq!(owner_of_url("data:text/html,x"), None);
    }

    #[test]
    fn the_seed_script_is_one_source_with_every_origin() {
        let mut seeds = BTreeMap::new();
        seeds.insert(
            "https://x.test/".to_string(),
            BTreeMap::from([
                ("k".to_string(), Some("v\"</script>".to_string())),
                ("g".to_string(), None),
            ]),
        );
        let source = seed_script(&seeds).unwrap();
        assert!(source.contains("https://x.test/"), "{source}");
        assert!(source.contains(r#"["k","v\"</script>"]"#), "{source}");
        assert!(source.contains(r#"["g",null]"#), "{source}");
        assert_eq!(seed_script(&BTreeMap::new()), None);
    }

    #[test]
    fn names_that_lead_out_of_a_folder_are_refused() {
        assert!(safe_origin("https_example.com_0"));
        for bad in ["", ".hidden", "a/b", "..", "a..b", "a\0b"] {
            assert!(!safe_origin(bad), "{bad:?}");
        }
        assert!(safe_file("leveldb/000003.log"));
        assert!(safe_file("blob/1/00/1"));
        for bad in [
            "",
            "leveldb",
            "/etc/passwd",
            "leveldb/../x",
            "other/a",
            "leveldb//a",
            "blob/./a",
        ] {
            assert!(!safe_file(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_folder_goes_out_and_in_and_a_change_changes_its_signature() {
        let root = std::env::temp_dir().join(format!("records-idb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let profile = root.join("p");
        let level = profile.join("IndexedDB/https_x.test_0.indexeddb.leveldb");
        std::fs::create_dir_all(&level).unwrap();
        std::fs::write(level.join("000003.log"), b"one").unwrap();
        std::fs::write(level.join("LOCK"), b"").unwrap();
        let blob = profile.join("IndexedDB/https_x.test_0.indexeddb.blob/1/00");
        std::fs::create_dir_all(&blob).unwrap();
        std::fs::write(blob.join("1"), b"blob").unwrap();

        assert_eq!(idb_origins(&profile), vec!["https_x.test_0".to_string()]);
        let stage = root.join("stage");
        let (sig, files) = idb_copy_out(&profile, "https_x.test_0", &stage).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["leveldb/000003.log", "blob/1/00/1"]);

        std::fs::write(level.join("000003.log"), b"one and more").unwrap();
        assert_ne!(idb_signature(&profile, "https_x.test_0"), Some(sig));

        // Into another profile: the same bytes under the same names.
        let other = root.join("q");
        idb_copy_in(&other, "https_x.test_0", &stage, &files).unwrap();
        assert_eq!(
            std::fs::read(other.join("IndexedDB/https_x.test_0.indexeddb.leveldb/000003.log"))
                .unwrap(),
            b"one"
        );
        assert_eq!(
            std::fs::read(other.join("IndexedDB/https_x.test_0.indexeddb.blob/1/00/1")).unwrap(),
            b"blob"
        );
        assert!(idb_signature(&other, "https_x.test_0").is_some());
        idb_remove(&other, "https_x.test_0");
        assert!(idb_origins(&other).is_empty());

        // A record that names a way out is refused before a file is written.
        let bad = [IndexedDbFile {
            path: "leveldb/../../x".into(),
            size: 1,
        }];
        assert!(idb_copy_in(&other, "https_x.test_0", &stage, &bad).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
