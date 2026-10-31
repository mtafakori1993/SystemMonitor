//! Audit trail of every Docker change made from this application, plus small persisted state.
//!
//! `~/.config/SystemMonitor/docker-audit.log`: one JSON object per line, mode 0600, rotated at 1 MiB
//! (one previous file kept). Entries hold the action, the object name and the outcome; never values of
//! settings. As a second line of defence every text is passed through `scrub`, which removes
//! credentials embedded in URLs.
use crate::linux::{app_dir, now_secs};
use crate::store;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

const MAX_LOG: u64 = 1 << 20;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Entry {
    pub ts: u64,
    pub user: String,
    pub action: String,
    pub target: String,
    pub ok: bool,
    pub error: String,
}

pub fn log_path() -> PathBuf {
    app_dir().join("docker-audit.log")
}

/// `scheme://user:pass@host` → `scheme://***@host`, anywhere in the text.
pub fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',').unwrap_or(tail.len());
        let auth = &tail[..end];
        match auth.find('/').map(|j| &auth[..j]).unwrap_or(auth).rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&auth[at..]);
            }
            None => out.push_str(auth),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| unsafe { libc::getuid() }.to_string())
}

pub fn record<T>(action: &str, target: &str, result: &Result<T, impl std::fmt::Display>) {
    let e = Entry { ts: now_secs(), user: user(), action: scrub(action), target: scrub(target), ok: result.is_ok(), error: result.as_ref().err().map(|e| scrub(&e.to_string())).unwrap_or_default() };
    let path = log_path();
    let _lock = store::Lock::acquire(&app_dir());
    if std::fs::metadata(&path).map(|m| m.len() > MAX_LOG).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let line = serde_json::to_string(&e).unwrap_or_default() + "\n";
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Newest first.
pub fn recent(n: usize) -> Vec<Entry> {
    let s = std::fs::read_to_string(log_path()).unwrap_or_default();
    s.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(n).collect()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct State {
    pub last_cleanup: Option<u64>,
    pub last_reclaimed: u64,
}

const SPEC: store::Spec<State> = store::Spec { file: "docker-state.json", version: 1, migrate: store::no_migration, validate: |_| vec![] };

pub fn load_state() -> State {
    store::load(&app_dir(), &SPEC).value
}

pub fn save_cleanup(reclaimed: u64) {
    let s = State { last_cleanup: Some(now_secs()), last_reclaimed: reclaimed };
    if let Err(e) = store::save(&app_dir(), &SPEC, &s) {
        eprintln!("docker state: {e}");
    }
}
