//! Versioned, crash-safe JSON persistence.
//!
//! * Atomic replace: temp file in the same directory, `fsync`, `rename`, `fsync` of the directory.
//! * Last-known-good copy (`<file>.bak`) is refreshed before each replace and used for recovery.
//! * Every document carries `"schema_version"`; older versions are migrated step by step after a
//!   copy of the original is saved under `backups/migrations/`. Newer versions are never overwritten.
//! * Unparsable files are quarantined as `<file>.corrupt-<time>` (never silently deleted); fields that
//!   still parse are salvaged one by one.
//! * An advisory `flock` on `.lock` serializes writers across threads and processes.
//! * Files are created `0600` inside a `0700` directory.
//!
//! JSON stays the format: the data is a handful of small documents, so SQLite would add a dependency
//! (and a C library, which this project avoids for static builds) without a measurable benefit.
use crate::linux::{now_secs, write_atomic};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const VERSION_KEY: &str = "schema_version";

pub struct Spec<T> {
    pub file: &'static str,
    /// Current schema version written by this build.
    pub version: u32,
    /// Upgrade a document from version `from` to `from + 1`.
    pub migrate: fn(Value, u32) -> Result<Value, String>,
    /// Clamp / drop invalid values after loading; returns human-readable notes.
    pub validate: fn(&mut T) -> Vec<String>,
}

pub struct Loaded<T> {
    pub value: T,
    /// What happened while loading (migration, recovery, validation); empty on a clean load.
    pub notes: Vec<String>,
    /// The file on disk was written by a newer version; saving is refused so nothing is lost.
    pub read_only: bool,
}

/// Exclusive lock for the whole config directory; released on drop.
pub struct Lock(fs::File);

impl Lock {
    pub fn acquire(dir: &Path) -> Result<Lock, String> {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let f = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock")).map_err(|e| format!("lock: {e}"))?;
        // blocking: writers are short, and the holder releases on drop or process exit
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("lock: {}", std::io::Error::last_os_error()));
        }
        Ok(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn ts() -> String {
    format!("{}", now_secs())
}

fn tighten(path: &Path, notes: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = fs::metadata(path) {
        if m.permissions().mode() & 0o077 != 0 {
            if fs::set_permissions(path, fs::Permissions::from_mode(0o600)).is_ok() {
                notes.push(format!("{}: permissions were too open, set to 0600", path.display()));
            }
        }
    }
}

fn quarantine(path: &Path, notes: &mut Vec<String>) {
    let q = path.with_extension(format!("json.corrupt-{}", ts()));
    match fs::rename(path, &q) {
        Ok(()) => notes.push(format!("unreadable file moved to {}", q.display())),
        Err(e) => notes.push(format!("could not quarantine {}: {e}", path.display())),
    }
}

/// Keep every key that still deserializes (starting from the defaults); report the keys that did not.
fn salvage<T: Serialize + DeserializeOwned + Default>(v: &Value, notes: &mut Vec<String>) -> Option<T> {
    let obj = v.as_object()?;
    let mut base = serde_json::to_value(T::default()).ok()?;
    let b = base.as_object_mut()?;
    for (k, val) in obj {
        if k == VERSION_KEY {
            continue;
        }
        let old = b.insert(k.clone(), val.clone());
        if serde_json::from_value::<T>(Value::Object(b.clone())).is_err() {
            notes.push(format!("ignored invalid value for '{k}'"));
            match old {
                Some(o) => b.insert(k.clone(), o),
                None => b.remove(k),
            };
        }
    }
    serde_json::from_value(Value::Object(b.clone())).ok()
}

fn parse<T: Serialize + DeserializeOwned + Default>(spec: &Spec<T>, text: &str, notes: &mut Vec<String>) -> Result<(T, u32, bool), String> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let from = v.get(VERSION_KEY).and_then(Value::as_u64).unwrap_or(0) as u32; // legacy files have no version
    if from > spec.version {
        return Err(format!("written by a newer version (schema {from} > {})", spec.version));
    }
    for step in from..spec.version {
        v = (spec.migrate)(v, step).map_err(|e| format!("migration {step}->{} failed: {e}", step + 1))?;
    }
    let migrated = from < spec.version;
    match serde_json::from_value::<T>(v.clone()) {
        Ok(t) => Ok((t, from, migrated)),
        Err(e) => {
            let t = salvage::<T>(&v, notes).ok_or_else(|| format!("invalid structure: {e}"))?;
            Ok((t, from, migrated))
        }
    }
}

/// Load `spec.file` from `dir`, repairing what can be repaired. Never fails: worst case is the default value.
pub fn load<T: Serialize + DeserializeOwned + Default>(dir: &Path, spec: &Spec<T>) -> Loaded<T> {
    let path = dir.join(spec.file);
    let mut notes = vec![];
    let finish = |mut value: T, mut notes: Vec<String>, read_only: bool| {
        notes.extend((spec.validate)(&mut value));
        Loaded { value, notes, read_only }
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // a crash between "move aside" and "write" leaves only the .bak
            let bak = path.with_extension("json.bak");
            if let Ok(t) = fs::read_to_string(&bak) {
                if let Ok((v, _, _)) = parse(spec, &t, &mut notes) {
                    notes.push(format!("{} was missing; restored from its backup", spec.file));
                    return finish(v, notes, false);
                }
            }
            return finish(T::default(), notes, false);
        }
        Err(e) => {
            notes.push(format!("{}: {e}", path.display()));
            return finish(T::default(), notes, false);
        }
    };
    tighten(&path, &mut notes);
    match parse(spec, &text, &mut notes) {
        Ok((value, from, migrated)) => {
            if migrated {
                backup_for_migration(dir, spec.file, from, &text, &mut notes);
                notes.push(format!("{} migrated from schema {from} to {}", spec.file, spec.version));
                if let Ok(_l) = Lock::acquire(dir) {
                    if let Err(e) = write_doc(dir, spec, &value) {
                        notes.push(format!("could not save migrated file: {e}"));
                    }
                }
            }
            finish(value, notes, false)
        }
        Err(e) if e.starts_with("written by a newer") => {
            notes.push(format!("{}: {e}; using defaults and leaving the file untouched", spec.file));
            finish(T::default(), notes, true)
        }
        Err(e) => {
            notes.push(format!("{}: {e}", spec.file));
            let bak = path.with_extension("json.bak");
            quarantine(&path, &mut notes);
            if let Ok(t) = fs::read_to_string(&bak) {
                if let Ok((v, _, _)) = parse(spec, &t, &mut notes) {
                    notes.push(format!("recovered {} from the last good backup", spec.file));
                    if let Ok(_l) = Lock::acquire(dir) {
                        let _ = write_doc(dir, spec, &v);
                    }
                    return finish(v, notes, false);
                }
            }
            notes.push("no usable backup; starting from defaults".into());
            finish(T::default(), notes, false)
        }
    }
}

fn backup_for_migration(dir: &Path, file: &str, from: u32, text: &str, notes: &mut Vec<String>) {
    let d = dir.join("backups").join("migrations");
    let name = format!("{}.v{from}.{}", file.trim_end_matches(".json"), ts());
    match write_atomic(&d.join(format!("{name}.json")), text, 0o600) {
        Ok(()) => notes.push(format!("original saved to {}", d.join(format!("{name}.json")).display())),
        Err(e) => notes.push(format!("could not back up before migration: {e}")),
    }
    // keep the 10 newest per file
    if let Ok(rd) = fs::read_dir(&d) {
        let prefix = format!("{}.v", file.trim_end_matches(".json"));
        let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with(&prefix)).unwrap_or(false)).collect();
        v.sort();
        while v.len() > 10 {
            let _ = fs::remove_file(v.remove(0));
        }
    }
}

/// Serialize with the version key and replace the file atomically. Caller must hold the `Lock`.
fn write_doc<T: Serialize>(dir: &Path, spec: &Spec<T>, value: &T) -> Result<(), String> {
    let mut v = serde_json::to_value(value).map_err(|e| e.to_string())?;
    match &mut v {
        Value::Object(m) => {
            m.insert(VERSION_KEY.into(), Value::from(spec.version));
        }
        _ => return Err("top-level value must be a JSON object".into()),
    }
    let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    let path = dir.join(spec.file);
    // last known good = the file we are about to replace, if it parses
    if let Ok(old) = fs::read_to_string(&path) {
        if serde_json::from_str::<Value>(&old).is_ok() {
            let _ = write_atomic(&path.with_extension("json.bak"), &old, 0o600);
        }
    }
    write_atomic(&path, &text, 0o600)
}

pub fn save<T: Serialize>(dir: &Path, spec: &Spec<T>, value: &T) -> Result<(), String> {
    let _l = Lock::acquire(dir)?;
    refuse_if_newer(dir, spec)?;
    write_doc(dir, spec, value)
}

fn refuse_if_newer<T>(dir: &Path, spec: &Spec<T>) -> Result<(), String> {
    if let Ok(t) = fs::read_to_string(dir.join(spec.file)) {
        if let Some(n) = serde_json::from_str::<Value>(&t).ok().and_then(|v| v.get(VERSION_KEY).and_then(Value::as_u64)) {
            if n as u32 > spec.version {
                return Err(format!("{} was written by a newer version (schema {n}); not overwriting", spec.file));
            }
        }
    }
    Ok(())
}

/// Read-modify-write under the lock, so concurrent updaters (threads or processes) never lose each other's changes.
pub fn update<T: Serialize + DeserializeOwned + Default>(dir: &Path, spec: &Spec<T>, f: impl FnOnce(&mut T)) -> Result<(), String> {
    let _l = Lock::acquire(dir)?;
    refuse_if_newer(dir, spec)?;
    let mut cur = load_unlocked(dir, spec);
    f(&mut cur);
    write_doc(dir, spec, &cur)
}

fn load_unlocked<T: Serialize + DeserializeOwned + Default>(dir: &Path, spec: &Spec<T>) -> T {
    let mut n = vec![];
    fs::read_to_string(dir.join(spec.file)).ok().and_then(|t| parse(spec, &t, &mut n).ok()).map(|x| x.0).unwrap_or_default()
}

/// Deserialize a list, dropping elements that do not parse instead of failing the whole file.
pub fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let v = Vec::<Value>::deserialize(d)?;
    Ok(v.into_iter().filter_map(|x| serde_json::from_value(x).ok()).collect())
}

pub fn no_migration(v: Value, from: u32) -> Result<Value, String> {
    let _ = from;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
    #[serde(default)]
    struct Doc {
        name: String,
        n: u32,
        #[serde(deserialize_with = "lenient_vec")]
        items: Vec<u32>,
    }

    fn clamp(d: &mut Doc) -> Vec<String> {
        if d.n > 100 {
            d.n = 100;
            return vec!["n clamped".into()];
        }
        vec![]
    }
    fn mig(mut v: Value, from: u32) -> Result<Value, String> {
        if from == 0 {
            // v0 stored "title"; v1 calls it "name"
            if let Some(t) = v.as_object_mut().and_then(|m| m.remove("title")) {
                v["name"] = t;
            }
        }
        Ok(v)
    }
    const SPEC: Spec<Doc> = Spec { file: "doc.json", version: 1, migrate: mig, validate: clamp };

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sysmon-store-{tag}-{}-{}", std::process::id(), crate::linux::now_millis()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_perms_and_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("rt");
        save(&d, &SPEC, &Doc { name: "a".into(), n: 3, items: vec![1, 2] }).unwrap();
        let l = load(&d, &SPEC);
        assert_eq!(l.value, Doc { name: "a".into(), n: 3, items: vec![1, 2] });
        assert!(l.notes.is_empty(), "{:?}", l.notes);
        assert_eq!(fs::metadata(d.join("doc.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(fs::read_to_string(d.join("doc.json")).unwrap().contains("\"schema_version\": 1"));
        assert!(fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains("tmp")));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn migrates_legacy_file_with_backup() {
        let d = tmp("mig");
        fs::write(d.join("doc.json"), r#"{"title":"old","n":7}"#).unwrap();
        let l = load(&d, &SPEC);
        assert_eq!(l.value.name, "old");
        assert!(l.notes.iter().any(|n| n.contains("migrated from schema 0")), "{:?}", l.notes);
        let bk: Vec<_> = fs::read_dir(d.join("backups/migrations")).unwrap().flatten().collect();
        assert_eq!(bk.len(), 1);
        assert!(fs::read_to_string(bk[0].path()).unwrap().contains("\"title\""), "backup holds the original");
        assert!(fs::read_to_string(d.join("doc.json")).unwrap().contains("schema_version"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn corrupt_file_recovers_from_bak_then_defaults() {
        let d = tmp("rec");
        save(&d, &SPEC, &Doc { name: "v1".into(), ..Default::default() }).unwrap();
        save(&d, &SPEC, &Doc { name: "v2".into(), ..Default::default() }).unwrap();
        fs::write(d.join("doc.json"), "{ truncated").unwrap();
        let l = load(&d, &SPEC);
        assert_eq!(l.value.name, "v1", "falls back to the previous good copy");
        assert!(l.notes.iter().any(|n| n.contains("recovered")), "{:?}", l.notes);
        assert!(fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("corrupt-")), "bad file kept for inspection");
        fs::write(d.join("doc.json"), "garbage").unwrap();
        fs::write(d.join("doc.json.bak"), "garbage").unwrap();
        let l = load(&d, &SPEC);
        assert_eq!(l.value, Doc::default());
        assert!(l.notes.iter().any(|n| n.contains("defaults")));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn partial_corruption_keeps_good_fields_and_drops_bad_list_items() {
        let d = tmp("part");
        fs::write(d.join("doc.json"), r#"{"schema_version":1,"name":"keep","n":"not a number","items":[1,"x",3]}"#).unwrap();
        let l = load(&d, &SPEC);
        assert_eq!(l.value.name, "keep");
        assert_eq!(l.value.n, 0);
        assert_eq!(l.value.items, vec![1, 3]);
        assert!(l.notes.iter().any(|n| n.contains("'n'")), "{:?}", l.notes);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn validation_clamps_and_newer_files_are_protected() {
        let d = tmp("new");
        fs::write(d.join("doc.json"), r#"{"schema_version":1,"n":5000}"#).unwrap();
        assert_eq!(load(&d, &SPEC).value.n, 100);
        fs::write(d.join("doc.json"), r#"{"schema_version":9,"name":"future"}"#).unwrap();
        let l = load(&d, &SPEC);
        assert!(l.read_only);
        assert!(save(&d, &SPEC, &Doc::default()).is_err());
        assert!(fs::read_to_string(d.join("doc.json")).unwrap().contains("future"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn concurrent_updates_do_not_lose_writes() {
        let d = tmp("conc");
        save(&d, &SPEC, &Doc::default()).unwrap();
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let d = d.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        update(&d, &SPEC, |x: &mut Doc| x.items.push(1)).unwrap();
                    }
                })
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(load(&d, &SPEC).value.items.len(), 100);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn interrupted_write_leaves_old_file_intact() {
        let d = tmp("int");
        save(&d, &SPEC, &Doc { name: "good".into(), ..Default::default() }).unwrap();
        // simulate a crash after the temp file was written but before the rename
        fs::write(d.join(".doc.json.tmp.1.1"), "{ half").unwrap();
        assert_eq!(load(&d, &SPEC).value.name, "good");
        fs::remove_dir_all(d).unwrap();
    }
}
