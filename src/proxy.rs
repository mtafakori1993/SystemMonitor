//! Proxy manager: detection across scopes, profiles, apply/backup/restore.
//!
//! Scopes (kept separate on purpose):
//!  * Environment of this process   – read-only, shown for information
//!  * Desktop session               – GNOME gsettings / KDE kioslaverc (desktop-aware apps)
//!  * User session environment      – ~/.config/environment.d + systemd --user (new processes)
//!  * System-wide                   – /etc/environment via PolicyKit helper (all users, next login)
//!  * Application-specific          – git, apt, npm, curl, docker ... (detected, never modified)
#[cfg(feature = "gui")]
use crate::app::Notices;
use crate::linux::*;
#[cfg(feature = "gui")]
use crate::settings::Settings;
#[cfg(feature = "gui")]
use crate::theme::{self, pal, Kind};
#[cfg(feature = "gui")]
use eframe::egui::{self, RichText, Ui};
use serde::{Deserialize, Serialize};
#[cfg(feature = "gui")]
use std::collections::HashMap;
use std::fs;
#[cfg(feature = "gui")]
use std::sync::mpsc::Receiver;

const ENV_FILE: &str = "90-systemmonitor-proxy.conf";
const BLOCK_BEGIN: &str = "# BEGIN SystemMonitor proxy";
const BLOCK_END: &str = "# END SystemMonitor proxy";
const ENV_KEYS: [&str; 8] = ["http_proxy", "https_proxy", "all_proxy", "no_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];
const GNOME_KEYS: [(&str, &str); 14] = [
    ("org.gnome.system.proxy", "mode"),
    ("org.gnome.system.proxy", "ignore-hosts"),
    ("org.gnome.system.proxy", "autoconfig-url"),
    ("org.gnome.system.proxy.http", "host"),
    ("org.gnome.system.proxy.http", "port"),
    ("org.gnome.system.proxy.http", "use-authentication"),
    ("org.gnome.system.proxy.http", "authentication-user"),
    ("org.gnome.system.proxy.https", "host"),
    ("org.gnome.system.proxy.https", "port"),
    ("org.gnome.system.proxy.socks", "host"),
    ("org.gnome.system.proxy.socks", "port"),
    ("org.gnome.system.proxy.ftp", "host"),
    ("org.gnome.system.proxy.ftp", "port"),
    ("org.gnome.system.proxy", "use-same-proxy"),
];

// ---------------------------------------------------------------- model

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum Scheme {
    Http,
    Https,
    Socks4,
    Socks5,
}

impl Scheme {
    fn label(self) -> &'static str {
        match self {
            Scheme::Http => "HTTP",
            Scheme::Https => "HTTPS",
            Scheme::Socks4 => "SOCKS4",
            Scheme::Socks5 => "SOCKS5",
        }
    }
    fn url(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
            Scheme::Socks4 => "socks4",
            Scheme::Socks5 => "socks5",
        }
    }
    fn is_socks(self) -> bool {
        matches!(self, Scheme::Socks4 | Scheme::Socks5)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Slot {
    pub enabled: bool,
    pub scheme: Scheme,
    pub host: String,
    pub port: String,
}

impl Slot {
    fn new(scheme: Scheme) -> Self {
        Self { enabled: false, scheme, host: String::new(), port: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum ProfileKind {
    Direct,
    System,
    Custom,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct ProxyProfile {
    pub name: String,
    pub kind: ProfileKind,
    pub http: Slot,
    pub https: Slot,
    pub all: Slot,
    pub no_proxy: String,
    pub username: String,
    pub has_password: bool,
}

impl ProxyProfile {
    fn builtin(name: &str, kind: ProfileKind) -> Self {
        Self { name: name.into(), kind, http: Slot::new(Scheme::Http), https: Slot::new(Scheme::Http), all: Slot::new(Scheme::Socks5), no_proxy: "localhost,127.0.0.1,::1".into(), username: String::new(), has_password: false }
    }
    fn custom(name: &str) -> Self {
        Self::builtin(name, ProfileKind::Custom)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Targets {
    pub desktop: bool,
    pub user_env: bool,
    pub system: bool,
    pub embed_credentials: bool,
}

impl Default for Targets {
    fn default() -> Self {
        Self { desktop: true, user_env: true, system: false, embed_credentials: false }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Store {
    #[serde(deserialize_with = "crate::store::lenient_vec")]
    profiles: Vec<ProxyProfile>,
    targets: Targets,
}

const STORE_SPEC: crate::store::Spec<Store> = crate::store::Spec { file: "proxy-profiles.json", version: 1, migrate: crate::store::no_migration, validate: validate_store };

/// Drop profiles that would be rejected by `validate` (hand-edited or damaged files must not reach the system).
fn validate_store(s: &mut Store) -> Vec<String> {
    let mut notes = vec![];
    s.profiles.retain(|p| match validate(p) {
        Ok(()) => true,
        Err(e) => {
            notes.push(format!("proxy profile '{}' ignored: {e}", p.name));
            false
        }
    });
    notes
}

fn load_store() -> Store {
    let l = crate::store::load(&app_dir(), &STORE_SPEC);
    for n in &l.notes {
        eprintln!("proxy profiles: {n}");
    }
    l.value
}

fn save_store(s: &Store) {
    if let Err(e) = crate::store::save(&app_dir(), &STORE_SPEC, s) {
        eprintln!("proxy profiles: {e}");
    }
}

pub fn validate(p: &ProxyProfile) -> Result<(), String> {
    if p.kind != ProfileKind::Custom {
        return Ok(());
    }
    if !valid_profile_name(&p.name) {
        return Err("Profile name must be 1-48 printable characters".into());
    }
    let mut any = false;
    for (label, s) in [("HTTP", &p.http), ("HTTPS", &p.https), ("ALL_PROXY", &p.all)] {
        if !s.enabled {
            continue;
        }
        any = true;
        if !valid_host(s.host.trim()) {
            return Err(format!("{label}: '{}' is not a valid host name or IP address", s.host.trim()));
        }
        if parse_port(&s.port).is_none() {
            return Err(format!("{label}: port must be between 1 and 65535"));
        }
    }
    if !any {
        return Err("Enable at least one proxy entry".into());
    }
    for e in p.no_proxy.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        if !valid_bypass(e) {
            return Err(format!("Bypass entry '{e}' contains invalid characters"));
        }
    }
    if !p.username.is_empty() && (p.username.chars().any(|c| c.is_control()) || p.username.len() > 128) {
        return Err("Invalid user name".into());
    }
    Ok(())
}

#[derive(Clone)]
struct Ep {
    scheme: Scheme,
    host: String,
    port: u16,
}

fn ep(s: &Slot) -> Option<Ep> {
    if !s.enabled {
        return None;
    }
    Some(Ep { scheme: s.scheme, host: s.host.trim().to_string(), port: parse_port(&s.port)? })
}

fn pct_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn url(e: &Ep, creds: Option<(&str, &str)>) -> String {
    let host = if e.host.contains(':') { format!("[{}]", e.host) } else { e.host.clone() };
    let auth = creds.map(|(u, p)| if p.is_empty() { format!("{}@", pct_encode(u)) } else { format!("{}:{}@", pct_encode(u), pct_encode(p)) }).unwrap_or_default();
    format!("{}://{auth}{host}:{}", e.scheme.url(), e.port)
}

fn no_proxy_list(p: &ProxyProfile) -> Vec<String> {
    p.no_proxy.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Environment variables for a profile (lower and upper case, as tools differ).
fn env_vars(p: &ProxyProfile, creds: Option<(&str, &str)>) -> Vec<(String, String)> {
    let mut v = vec![];
    if p.kind != ProfileKind::Custom {
        return v;
    }
    let mut push = |k: &str, val: String| {
        v.push((k.to_lowercase(), val.clone()));
        v.push((k.to_uppercase(), val));
    };
    if let Some(e) = ep(&p.http) {
        push("http_proxy", url(&e, creds));
    }
    if let Some(e) = ep(&p.https) {
        push("https_proxy", url(&e, creds));
    }
    if let Some(e) = ep(&p.all) {
        push("all_proxy", url(&e, creds));
    }
    let np = no_proxy_list(p);
    if !np.is_empty() {
        push("no_proxy", np.join(","));
    }
    v
}

struct DesktopMap {
    http: Option<Ep>,
    https: Option<Ep>,
    socks: Option<Ep>,
}

fn desktop_map(p: &ProxyProfile) -> DesktopMap {
    let (mut http, mut https, mut socks) = (ep(&p.http), ep(&p.https), None);
    for e in [ep(&p.http), ep(&p.https), ep(&p.all)].into_iter().flatten() {
        if e.scheme.is_socks() && socks.is_none() {
            socks = Some(e);
        }
    }
    if http.as_ref().map(|e| e.scheme.is_socks()).unwrap_or(false) {
        http = None;
    }
    if https.as_ref().map(|e| e.scheme.is_socks()).unwrap_or(false) {
        https = None;
    }
    if let Some(all) = ep(&p.all) {
        if !all.scheme.is_socks() {
            http = http.or(Some(all.clone()));
            https = https.or(Some(all));
        }
    }
    DesktopMap { http, https, socks }
}

// ---------------------------------------------------------------- detection

#[derive(Clone, Default)]
pub struct Detected {
    pub process_env: Vec<(String, String)>,
    pub user_env_file: Option<String>,
    pub other_env_d: Vec<(String, String)>,
    pub etc_env: Vec<String>,
    pub gnome: Option<Vec<(String, String)>>,
    pub gnome_error: Option<String>,
    pub kde: Option<Vec<(String, String)>>,
    pub app_specific: Vec<(String, String)>,
}

pub fn mask(v: &str) -> String {
    // never display credentials embedded in URLs
    match (v.find("://"), v.rfind('@')) {
        (Some(i), Some(j)) if j > i => format!("{}***@{}", &v[..i + 3], &v[j + 1..]),
        _ => v.to_string(),
    }
}

fn gsettings(args: &[&str]) -> Result<String, String> {
    run("gsettings", args)
}

fn kioslaverc() -> std::path::PathBuf {
    config_home().join("kioslaverc")
}

fn env_d_file() -> std::path::PathBuf {
    config_home().join("environment.d").join(ENV_FILE)
}

fn ini_section(content: &str, section: &str) -> Vec<(String, String)> {
    let mut on = false;
    let mut out = vec![];
    for l in content.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            on = t == format!("[{section}]");
        } else if on {
            if let Some((k, v)) = t.split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

fn ini_set(content: &str, section: &str, kv: &[(&str, Option<String>)]) -> String {
    let header = format!("[{section}]");
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let start = lines.iter().position(|l| l.trim() == header);
    let start = match start {
        Some(s) => s,
        None => {
            if lines.last().map(|l| !l.is_empty()).unwrap_or(false) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.len() - 1
        }
    };
    let end = lines.iter().enumerate().skip(start + 1).find(|(_, l)| l.trim().starts_with('[')).map(|(i, _)| i).unwrap_or(lines.len());
    let mut body: Vec<String> = lines[start + 1..end].to_vec();
    let mut trailing = vec![];
    while body.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        trailing.push(body.pop().unwrap());
    }
    for (k, v) in kv {
        body.retain(|l| l.split_once('=').map(|(a, _)| a.trim() != *k).unwrap_or(true));
        if let Some(v) = v {
            body.push(format!("{k}={v}"));
        }
    }
    let mut out: Vec<String> = lines[..start + 1].to_vec();
    out.extend(body);
    out.extend(trailing);
    out.extend_from_slice(&lines[end..]);
    out.join("\n") + "\n"
}

pub fn detect() -> Detected {
    let mut d = Detected::default();
    for k in ENV_KEYS {
        if let Ok(v) = std::env::var(k) {
            d.process_env.push((k.to_string(), mask(&v)));
        }
    }
    d.user_env_file = fs::read_to_string(env_d_file()).ok().map(|s| s.lines().map(mask).collect::<Vec<_>>().join("\n"));
    if let Ok(rd) = fs::read_dir(config_home().join("environment.d")) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy() == ENV_FILE {
                continue;
            }
            for l in fs::read_to_string(e.path()).unwrap_or_default().lines() {
                if ENV_KEYS.iter().any(|k| l.trim_start().starts_with(&format!("{k}="))) {
                    d.other_env_d.push((e.file_name().to_string_lossy().into_owned(), mask(l.trim())));
                }
            }
        }
    }
    for l in fs::read_to_string("/etc/environment").unwrap_or_default().lines() {
        if ENV_KEYS.iter().any(|k| l.trim_start().starts_with(&format!("{k}="))) {
            d.etc_env.push(mask(l.trim()));
        }
    }
    if which("gsettings").is_some() {
        match gsettings(&["get", "org.gnome.system.proxy", "mode"]) {
            Ok(_) => {
                d.gnome = Some(GNOME_KEYS.iter().filter(|(_, k)| !matches!(*k, "ftp" | "use-same-proxy")).filter_map(|(s, k)| gsettings(&["get", s, k]).ok().map(|v| ({ let sub = s.trim_start_matches("org.gnome.system.proxy").trim_start_matches('.'); if sub.is_empty() { k.to_string() } else { format!("{sub}.{k}") } }, v))).collect())
            }
            Err(e) => d.gnome_error = Some(e),
        }
    }
    if let Ok(s) = fs::read_to_string(kioslaverc()) {
        let kv = ini_section(&s, "Proxy Settings");
        d.kde = Some(kv.into_iter().map(|(k, v)| (k, mask(&v))).collect());
    }
    let home = home();
    let grep = |path: std::path::PathBuf, needles: &[&str], label: &str, out: &mut Vec<(String, String)>| {
        for l in fs::read_to_string(&path).unwrap_or_default().lines() {
            let t = l.trim();
            if !t.starts_with('#') && needles.iter().any(|n| t.to_lowercase().contains(n)) {
                out.push((format!("{label} ({})", path.display()), mask(t)));
            }
        }
    };
    grep(home.join(".gitconfig"), &["proxy ="], "git", &mut d.app_specific);
    grep(home.join(".npmrc"), &["proxy="], "npm", &mut d.app_specific);
    grep(home.join(".curlrc"), &["proxy"], "curl", &mut d.app_specific);
    grep(home.join(".wgetrc"), &["_proxy"], "wget", &mut d.app_specific);
    grep(home.join(".docker/config.json"), &["httpproxy", "httpsproxy"], "docker", &mut d.app_specific);
    if let Ok(rd) = fs::read_dir("/etc/apt/apt.conf.d") {
        for e in rd.flatten() {
            grep(e.path(), &["acquire::http::proxy", "acquire::https::proxy"], "apt", &mut d.app_specific);
        }
    }
    if let Ok(rd) = fs::read_dir("/etc/profile.d") {
        for e in rd.flatten() {
            grep(e.path(), &["_proxy="], "profile.d", &mut d.app_specific);
        }
    }
    grep("/etc/dnf/dnf.conf".into(), &["proxy="], "dnf", &mut d.app_specific);
    d
}

// ---------------------------------------------------------------- backup

#[derive(Serialize, Deserialize, Default)]
struct Backup {
    label: String,
    ts: u64,
    gnome: Option<Vec<(String, String, String)>>,
    kde: Option<String>,
    user_env: Option<String>,
    etc_block: Option<String>,
}

fn etc_block_text() -> Option<String> {
    let s = fs::read_to_string("/etc/environment").ok()?;
    let a = s.find(BLOCK_BEGIN)?;
    let b = s.find(BLOCK_END)?;
    Some(s[a + BLOCK_BEGIN.len()..b].trim().to_string())
}

fn make_backup(label: &str, file_stem: Option<&str>) -> Result<String, String> {
    let mut b = Backup { label: label.into(), ts: now_secs(), ..Default::default() };
    if which("gsettings").is_some() && gsettings(&["get", "org.gnome.system.proxy", "mode"]).is_ok() {
        b.gnome = Some(GNOME_KEYS.iter().filter_map(|(s, k)| gsettings(&["get", s, k]).ok().map(|v| (s.to_string(), k.to_string(), v))).collect());
    }
    b.kde = fs::read_to_string(kioslaverc()).ok();
    b.user_env = fs::read_to_string(env_d_file()).ok();
    b.etc_block = etc_block_text();
    let stem = file_stem.map(String::from).unwrap_or_else(|| format!("{}", now_millis()));
    let path = backup_dir("proxy").join(format!("{stem}.json"));
    write_atomic(&path, &serde_json::to_string_pretty(&b).map_err(|e| e.to_string())?, 0o600)?;
    Ok(stem)
}

pub fn ensure_original() {
    let p = backup_dir("proxy").join("0-original.json");
    if !p.exists() {
        let _ = make_backup("Original configuration (first run)", Some("0-original"));
    }
}

fn restore_backup(path: &std::path::Path, desktop_only: bool, sys: bool) -> Result<String, String> {
    let b: Backup = serde_json::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?).map_err(|e| format!("corrupt backup: {e}"))?;
    let mut done = vec![];
    if let Some(g) = &b.gnome {
        for (s, k, v) in g {
            if gsettings(&["set", s, k, v]).is_err() && !k.contains("ftp") {
                // some keys may not exist on older versions; ignore individually
            }
        }
        done.push("GNOME settings");
    }
    if let Some(k) = &b.kde {
        write_atomic(&kioslaverc(), k, 0o600)?;
        notify_kde();
        done.push("KDE settings");
    }
    if !desktop_only {
        match &b.user_env {
            Some(c) => write_atomic(&env_d_file(), c, 0o600)?,
            None => {
                let _ = fs::remove_file(env_d_file());
            }
        }
        sync_systemd_env(&env_vars_from_file(b.user_env.as_deref().unwrap_or("")));
        done.push("user environment");
        if sys {
            helper_etc_environment(b.etc_block.as_deref().unwrap_or(""))?;
            done.push("system-wide environment");
        }
    }
    Ok(format!("Restored: {}", done.join(", ")))
}

// ---------------------------------------------------------------- apply

/// Proxy variables new login sessions receive from SystemMonitor (empty when the proxy is disabled).
pub fn session_env() -> Vec<(String, String)> {
    fs::read_to_string(env_d_file()).map(|c| env_vars_from_file(&c.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join("\n"))).unwrap_or_default()
}

fn env_vars_from_file(c: &str) -> Vec<(String, String)> {
    c.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

fn sync_systemd_env(vars: &[(String, String)]) {
    if which("systemctl").is_none() {
        return;
    }
    let _ = run("systemctl", &["--user", "unset-environment", "http_proxy", "https_proxy", "all_proxy", "no_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]);
    if !vars.is_empty() {
        let kv: Vec<String> = vars.iter().filter(|(k, _)| ENV_KEYS.contains(&k.as_str())).map(|(k, v)| format!("{k}={v}")).collect();
        let mut args = vec!["--user", "set-environment"];
        args.extend(kv.iter().map(String::as_str));
        let _ = run("systemctl", &args);
    }
}

fn notify_kde() {
    // Ask running KIO slaves to re-read kioslaverc.
    let _ = block_on(async {
        let c = zbus::Connection::session().await?;
        c.emit_signal(None::<&str>, "/KIO/Scheduler", "org.kde.KIO.Scheduler", "reparseSlaveConfiguration", &("",)).await
    });
}

/// Run the privileged helper (this same executable) through pkexec.
fn helper_etc_environment(block_body: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = app_dir().join("staging-etc-environment");
    write_atomic(&staged, block_body, 0o600)?;
    let out = std::process::Command::new("pkexec").arg(&exe).arg("--helper-etc-environment").arg(&staged).output().map_err(|e| format!("pkexec: {e}"))?;
    let _ = fs::remove_file(&staged);
    match out.status.code() {
        Some(0) => Ok(()),
        Some(126) => Err("Authentication was cancelled or not authorized".into()),
        Some(127) => Err("PolicyKit could not authenticate (no authentication agent running?)".into()),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}

/// Entry point of `SystemMonitor --helper-etc-environment <staged>` (runs as root through pkexec).
/// Only rewrites a clearly delimited proxy block in /etc/environment after strict validation.
pub fn helper_main(staged: &str) -> i32 {
    use std::os::unix::fs::MetadataExt;
    let target = if unsafe { libc::geteuid() } == 0 {
        std::path::PathBuf::from("/etc/environment")
    } else if let Some(t) = std::env::var_os("SYSMON_TEST_ETC_ENVIRONMENT") {
        std::path::PathBuf::from(t) // only honoured when unprivileged (tests)
    } else {
        eprintln!("helper must run through pkexec");
        return 2;
    };
    let md = match fs::symlink_metadata(staged) {
        Ok(m) if m.is_file() => m,
        _ => {
            eprintln!("staged file is not a regular file");
            return 2;
        }
    };
    if let Some(uid) = std::env::var("PKEXEC_UID").ok().and_then(|u| u.parse::<u32>().ok()) {
        if md.uid() != uid {
            eprintln!("staged file is not owned by the invoking user");
            return 2;
        }
    }
    if md.len() > 8192 {
        eprintln!("staged file too large");
        return 2;
    }
    let body = fs::read_to_string(staged).unwrap_or_default();
    let mut lines = vec![];
    for l in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some((k, v)) = l.split_once('=') else {
            eprintln!("invalid line");
            return 2;
        };
        if !ENV_KEYS.contains(&k) || v.len() > 1024 || !v.chars().all(|c| c.is_ascii_alphanumeric() || ":/.,*_-[]%@+".contains(c)) {
            eprintln!("rejected line for {k}");
            return 2;
        }
        lines.push(l.to_string());
    }
    let current = fs::read_to_string(&target).unwrap_or_default();
    let _ = fs::write(format!("{}.systemmonitor.bak", target.display()), &current);
    let mut kept = vec![];
    let mut skipping = false;
    for l in current.lines() {
        if l.trim() == BLOCK_BEGIN {
            skipping = true;
        } else if l.trim() == BLOCK_END {
            skipping = false;
        } else if !skipping {
            kept.push(l.to_string());
        }
    }
    while kept.last().map(|l| l.is_empty()).unwrap_or(false) {
        kept.pop();
    }
    if !lines.is_empty() {
        kept.push(BLOCK_BEGIN.into());
        kept.extend(lines);
        kept.push(BLOCK_END.into());
    }
    let tmp = format!("{}.systemmonitor.tmp", target.display());
    if let Err(e) = fs::write(&tmp, kept.join("\n") + "\n").and_then(|_| {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
        fs::rename(&tmp, &target)
    }) {
        eprintln!("write failed: {e}");
        return 1;
    }
    0
}

fn gset(schema: &str, key: &str, val: &str) -> Result<(), String> {
    gsettings(&["set", schema, key, val]).map(|_| ())
}

/// What the GNOME side of this session can actually do. Nothing here changes any setting.
#[derive(Clone, Debug, Default)]
pub struct GnomeProbe {
    pub gnome_session: bool,
    pub gsettings: bool,
    pub schema: bool,
    pub backend: String,
    pub dconf_module: bool,
    pub problems: Vec<String>,
}

impl GnomeProbe {
    /// Safe to write and expect persistence (the write is still verified by reading it back).
    pub fn usable(&self) -> bool {
        self.gsettings && self.schema && self.dconf_module && self.backend != "memory"
    }
}

fn dconf_module_installed() -> bool {
    ["/usr/lib64/gio/modules", "/usr/lib/gio/modules", "/usr/lib/x86_64-linux-gnu/gio/modules", "/usr/lib/aarch64-linux-gnu/gio/modules", "/usr/lib/arm-linux-gnueabihf/gio/modules"].iter().any(|d| std::path::Path::new(d).join("libdconfsettings.so").exists())
}

pub fn gnome_probe(desktop: &Desktop) -> GnomeProbe {
    let mut g = GnomeProbe {
        gnome_session: desktop.gnome_like || std::env::var("XDG_SESSION_DESKTOP").map(|v| v.to_lowercase().contains("gnome")).unwrap_or(false) || std::env::var_os("GNOME_DESKTOP_SESSION_ID").is_some(),
        gsettings: which("gsettings").is_some(),
        backend: std::env::var("GSETTINGS_BACKEND").unwrap_or_else(|_| "dconf".into()),
        dconf_module: dconf_module_installed(),
        ..Default::default()
    };
    if !g.gsettings {
        g.problems.push("gsettings is not installed (package: glib2 / libglib2.0-bin)".into());
        return g;
    }
    g.schema = gsettings(&["list-schemas"]).map(|l| l.lines().any(|x| x.trim() == "org.gnome.system.proxy")).unwrap_or(false);
    if !g.schema {
        g.problems.push("schema org.gnome.system.proxy is missing (package: gsettings-desktop-schemas)".into());
    }
    if g.backend == "memory" {
        g.problems.push("GSETTINGS_BACKEND=memory: settings would be discarded when the command exits".into());
    } else if !g.dconf_module {
        g.problems.push("the dconf GSettings backend is not installed, so gsettings would use a throw-away memory backend (package: dconf / dconf-gsettings-backend)".into());
    }
    g
}

fn gv_norm(v: &str) -> String {
    v.trim().replace("@as ", "").replace("uint32 ", "").replace(", ", ",").replace(" ]", "]").replace("[ ", "[")
}

/// Write the profile into GSettings, verify every key by reading it back (each `gsettings` call is a
/// separate process, so a non-persistent backend cannot fake the readback), and restore the previous
/// values if anything differs. Passwords are never written here: dconf is not a secret store.
fn apply_gnome(p: &ProxyProfile, username: &str) -> Result<(), String> {
    const S: &str = "org.gnome.system.proxy";
    let mut want: Vec<(&str, &str, String)> = vec![];
    if p.kind != ProfileKind::Custom {
        want.push((S, "mode", "'none'".into()));
    } else {
        let m = desktop_map(p);
        for (schema, e) in [("org.gnome.system.proxy.http", &m.http), ("org.gnome.system.proxy.https", &m.https), ("org.gnome.system.proxy.socks", &m.socks)] {
            match e {
                Some(e) => {
                    want.push((schema, "host", format!("'{}'", e.host)));
                    want.push((schema, "port", e.port.to_string()));
                }
                None => {
                    want.push((schema, "host", "''".into()));
                    want.push((schema, "port", "0".into()));
                }
            }
        }
        let hosts = no_proxy_list(p).iter().map(|h| format!("'{h}'")).collect::<Vec<_>>().join(", ");
        want.push((S, "ignore-hosts", format!("[{hosts}]")));
        if !username.is_empty() && m.http.is_some() {
            want.push(("org.gnome.system.proxy.http", "use-authentication", "true".into()));
            want.push(("org.gnome.system.proxy.http", "authentication-user", format!("'{}'", username.replace('\'', ""))));
        } else {
            want.push(("org.gnome.system.proxy.http", "use-authentication", "false".into()));
        }
        want.push((S, "mode", "'manual'".into()));
    }
    // previous values, for rollback
    let before: Vec<(&str, &str, String)> = want.iter().map(|(s, k, _)| (*s, *k, gsettings(&["get", s, k]).unwrap_or_default())).collect();
    let rollback = |why: String| -> String {
        let mut failed = vec![];
        for (s, k, v) in before.iter().rev() {
            if !v.is_empty() && gset(s, k, v).is_err() {
                failed.push(format!("{s} {k}"));
            }
        }
        if failed.is_empty() { format!("{why}; previous GNOME settings were restored") } else { format!("{why}; rollback also failed for: {}", failed.join(", ")) }
    };
    for (s, k, v) in &want {
        if let Err(e) = gset(s, k, v) {
            return Err(rollback(format!("gsettings set {s} {k} failed: {e}")));
        }
    }
    for (s, k, v) in &want {
        match gsettings(&["get", s, k]) {
            Ok(got) if gv_norm(&got) == gv_norm(v) => {}
            Ok(got) => return Err(rollback(format!("{s} {k} reads back as {got}, expected {v}: the GSettings backend (dconf) is not persisting changes in this session"))),
            Err(e) => return Err(rollback(format!("cannot read back {s} {k}: {e}"))),
        }
    }
    Ok(())
}

fn apply_kde(p: &ProxyProfile) -> Result<(), String> {
    let cur = fs::read_to_string(kioslaverc()).unwrap_or_default();
    let kv: Vec<(&str, Option<String>)> = if p.kind != ProfileKind::Custom {
        vec![("ProxyType", Some("0".into()))]
    } else {
        let m = desktop_map(p);
        let f = |e: &Option<Ep>, socks: bool| e.as_ref().map(|e| format!("{}://{}:{}", if socks { "socks" } else { "http" }, e.host, e.port));
        vec![("ProxyType", Some("1".into())), ("httpProxy", f(&m.http, false)), ("httpsProxy", f(&m.https, false)), ("socksProxy", f(&m.socks, true)), ("NoProxyFor", Some(no_proxy_list(p).join(",")))]
    };
    write_atomic(&kioslaverc(), &ini_set(&cur, "Proxy Settings", &kv), 0o600)?;
    notify_kde();
    Ok(())
}

pub fn apply(p: &ProxyProfile, t: Targets, password: Option<String>, services: &Services, desktop: &Desktop) -> Result<String, String> {
    validate(p)?;
    make_backup(&format!("Before applying '{}'", p.name), None)?;
    let mut done = vec![];
    let mut warns = vec![];
    let creds_ok = t.embed_credentials && !p.username.is_empty();
    let creds = if creds_ok { Some((p.username.as_str(), password.as_deref().unwrap_or(""))) } else { None };
    if t.desktop {
        let probe = gnome_probe(desktop);
        let gnome_ok = services.gsettings && probe.usable();
        if services.gsettings && !probe.usable() && !probe.problems.is_empty() && (probe.gnome_session || !(desktop.kde || kioslaverc().exists())) {
            warns.push(format!("GNOME/GTK settings not applied: {}", probe.problems.join("; ")));
        }
        let kde_ok = desktop.kde || kioslaverc().exists();
        if p.kind == ProfileKind::System {
            // "System Default": give control back by restoring the first-run desktop state.
            let orig = backup_dir("proxy").join("0-original.json");
            if orig.exists() {
                restore_backup(&orig, true, false)?;
                done.push("desktop settings restored to original".to_string());
            }
        } else {
            let mut gnome_done = false;
            if gnome_ok {
                match apply_gnome(p, &p.username) {
                    Ok(()) => {
                        gnome_done = true;
                        done.push("GNOME/GTK settings".to_string());
                        if !p.username.is_empty() && p.kind == ProfileKind::Custom {
                            warns.push("proxy password NOT written to dconf (it would be stored in plain text); it stays in the Secret Service / GNOME Keyring and GNOME apps will ask for it".to_string());
                        }
                    }
                    Err(e) if kde_ok => warns.push(format!("GNOME/GTK settings skipped: {e}")),
                    Err(e) => return Err(e),
                }
            }
            if kde_ok {
                apply_kde(p)?;
                done.push("KDE settings".into());
            }
            if !gnome_done && !kde_ok {
                warns.push("no supported desktop proxy backend found (GNOME gsettings or KDE)".into());
            }
        }
    }
    if t.user_env {
        let vars = if p.kind == ProfileKind::System { vec![] } else { env_vars(p, creds) };
        let file = env_d_file();
        if vars.is_empty() {
            let _ = fs::remove_file(&file);
        } else {
            let mode = if creds.is_some() { 0o600 } else { 0o644 };
            let body: String = vars.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
            write_atomic(&file, &format!("# Managed by SystemMonitor profile '{}'\n{body}", p.name.replace('\n', " ")), mode)?;
        }
        // shell snippets (bash/zsh `proxy.env`, fish `proxy.fish`): written or removed together with the session file
        crate::shellenv::write_env_files(&crate::shellenv::Dirs::current(), &vars)?;
        crate::shellenv::set_process_proxy(&vars);
        sync_systemd_env(&vars);
        done.push("user session environment (new processes only; open terminals: eval \"$(SystemMonitor --cli proxy-env bash)\")".into());
    }
    if t.system {
        if !services.pkexec {
            return Err("System-wide changes need PolicyKit (pkexec), which is not installed".into());
        }
        // never write credentials to a world-readable file
        let vars = if p.kind == ProfileKind::System || p.kind == ProfileKind::Direct { vec![] } else { env_vars(p, None) };
        let body: String = vars.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        helper_etc_environment(&body)?;
        done.push("system-wide /etc/environment (next login)".into());
        if creds_ok {
            warns.push("credentials are never written system-wide".into());
        }
    }
    if done.is_empty() {
        return Err(if warns.is_empty() { "No apply target selected".into() } else { format!("Nothing was applied: {}", warns.join("; ")) });
    }
    let mut msg = format!("Applied '{}': {}.", p.name, done.join("; "));
    if !warns.is_empty() {
        msg += &format!(" Note: {}.", warns.join("; "));
    }
    Ok(msg)
}

pub fn test_endpoint(host: &str, port: u16) -> Result<u128, String> {
    use std::net::ToSocketAddrs;
    let addr = (host, port).to_socket_addrs().map_err(|e| format!("cannot resolve {host}: {e}"))?.next().ok_or("no address")?;
    let t = std::time::Instant::now();
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(4)).map_err(|e| format!("{addr}: {e}"))?;
    Ok(t.elapsed().as_millis())
}

// ---------------------------------------------------------------- UI

#[cfg(feature = "gui")]
enum Act {
    Save,
    Test,
    Dup,
    Delete,
}

#[cfg(feature = "gui")]
enum Msg {
    Detected(Box<Detected>),
    Applied(String, Result<String, String>),
    Restored(Result<String, String>),
    PasswordSaved(String, Result<(), String>),
    Tested(Result<u128, String>),
}

#[cfg(feature = "gui")]
pub struct ProxyUi {
    store: Store,
    selected: usize,
    detected: Option<Detected>,
    rx: Vec<Receiver<Msg>>,
    busy: bool,
    password: String,
    session_pw: HashMap<String, String>,
    backups: Vec<(std::path::PathBuf, String)>,
    confirm_restore: Option<std::path::PathBuf>,
    confirm_delete: Option<usize>,
    error: Option<String>,
    secret_service: bool,
    edit_name: String,
}

#[cfg(feature = "gui")]
fn builtins() -> Vec<ProxyProfile> {
    vec![ProxyProfile::builtin("System Default", ProfileKind::System), ProxyProfile::builtin("Direct Connection", ProfileKind::Direct)]
}

#[cfg(feature = "gui")]
impl ProxyUi {
    pub fn new(settings: &Settings, _d: &Desktop, ctx: &egui::Context) -> Self {
        let mut store = load_store();
        store.profiles.retain(|p| p.kind == ProfileKind::Custom);
        let mut all = builtins();
        all.extend(store.profiles.drain(..));
        store.profiles = all;
        let selected = store.profiles.iter().position(|p| p.name == settings.active_proxy).unwrap_or(0);
        let mut s = Self { store, selected, detected: None, rx: vec![], busy: false, password: String::new(), session_pw: HashMap::new(), backups: vec![], confirm_restore: None, confirm_delete: None, error: None, secret_service: false, edit_name: String::new() };
        s.edit_name = s.store.profiles[s.selected].name.clone();
        s.rescan(ctx);
        s
    }

    fn save(&self) {
        let mut st = Store { profiles: self.store.profiles.iter().filter(|p| p.kind == ProfileKind::Custom).cloned().collect(), targets: self.store.targets };
        save_store(&st);
        st.profiles.clear();
    }

    pub fn on_services(&mut self, s: &Services) {
        self.secret_service = s.secret_service;
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        self.backups = list_backups("proxy");
        let rx = {
            let (tx, rx) = std::sync::mpsc::channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                ensure_original();
                let _ = tx.send(Msg::Detected(Box::new(detect())));
                ctx.request_repaint();
            });
            rx
        };
        self.rx.push(rx);
    }

    pub fn poll(&mut self, n: &mut Notices, settings: &mut Settings) {
        let mut msgs = vec![];
        let mut lost = 0;
        self.rx.retain(|rx| match rx.try_recv() {
            Ok(m) => {
                msgs.push(m);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => {
                lost += 1;
                false
            }
        });
        if lost > 0 {
            // a worker died without answering: do not stay "busy" forever
            self.busy = false;
            self.error = Some("A background task ended unexpectedly; please try again.".into());
        }
        for m in msgs {
            match m {
                Msg::Detected(d) => self.detected = Some(*d),
                Msg::Applied(name, r) => {
                    self.busy = false;
                    match r {
                        Ok(s) => {
                            settings.active_proxy = name;
                            settings.save();
                            n.ok(s);
                        }
                        Err(e) => n.err(format!("Proxy change failed: {e}")),
                    }
                    self.backups = list_backups("proxy");
                }
                Msg::Restored(r) => {
                    self.busy = false;
                    match r {
                        Ok(s) => n.ok(s),
                        Err(e) => n.err(format!("Restore failed: {e}")),
                    }
                }
                Msg::PasswordSaved(name, r) => match r {
                    Ok(()) => {
                        if let Some(p) = self.store.profiles.iter_mut().find(|p| p.name == name) {
                            p.has_password = true;
                        }
                        self.save();
                        n.ok("Password saved to the system keyring");
                    }
                    Err(e) => n.push(Kind::Warning, format!("Keyring unavailable ({e}); password kept in memory for this session only")),
                },
                Msg::Tested(r) => match r {
                    Ok(ms) => n.ok(format!("Proxy reachable (TCP connect {ms} ms)")),
                    Err(e) => n.err(format!("Proxy test failed: {e}")),
                },
            }
        }
    }

    fn start_apply(&mut self, ctx: &egui::Context, idx: usize, services: &Services, desktop: &Desktop) {
        let p = self.store.profiles[idx].clone();
        if let Err(e) = validate(&p) {
            self.error = Some(e);
            return;
        }
        self.error = None;
        self.busy = true;
        let (t, services, desktop) = (self.store.targets, services.clone(), desktop.clone());
        let sess = self.session_pw.get(&p.name).cloned();
        let ctx2 = ctx.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut pw = sess;
            if t.embed_credentials && pw.is_none() && p.has_password {
                pw = block_on(crate::secrets::lookup("proxy", &p.name)).ok().flatten();
            }
            let name = p.name.clone();
            let _ = tx.send(Msg::Applied(name, apply(&p, t, pw, &services, &desktop)));
            ctx2.request_repaint();
        });
        self.rx.push(rx);
    }

    pub fn quick_switch(&mut self, ui: &mut Ui, settings: &mut Settings, desktop: &Desktop, services: &Services, _n: &mut Notices) {
        let mut pick = None;
        ui.add_enabled_ui(!self.busy, |ui| {
            egui::ComboBox::from_id_salt("quick_proxy").selected_text(format!("Proxy: {}", settings.active_proxy)).show_ui(ui, |ui| {
                for (i, p) in self.store.profiles.iter().enumerate() {
                    if ui.selectable_label(p.name == settings.active_proxy, &p.name).clicked() {
                        pick = Some(i);
                    }
                }
            });
        });
        if let Some(i) = pick {
            self.selected = i;
            self.edit_name = self.store.profiles[i].name.clone();
            let ctx = ui.ctx().clone();
            self.start_apply(&ctx, i, services, desktop);
        }
    }

    pub fn page(&mut self, ui: &mut Ui, settings: &mut Settings, desktop: &Desktop, services: &Services, n: &mut Notices) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        theme::page_title(ui, "Proxy Manager", "Detect, edit and switch proxy configuration for each scope");
        if let Some(e) = &self.error {
            theme::note(ui, Kind::Error, e);
            ui.add_space(8.0);
        }
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Applying configuration (a PolicyKit prompt may appear)...");
            });
        }
        // ---- scope overview
        let mut rescan = false;
        theme::container_with(ui, "Configuration scopes", |ui| {
            rescan = ui.button("Rescan").clicked();
        }, |ui| {
            let Some(d) = &self.detected else {
                ui.spinner();
                return;
            };
            let row = |ui: &mut Ui, name: &str, scope: &str, active: bool, details: String| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(egui::vec2(250.0, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(250.0);
                        ui.label(RichText::new(name).strong());
                        ui.add(egui::Label::new(RichText::new(scope).small().color(p.text_dim)).wrap());
                    });
                    ui.allocate_ui_with_layout(egui::vec2(100.0, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(100.0);
                        theme::status(ui, if active { Kind::Info } else { Kind::Inactive }, if active { "Configured" } else { "Not set" });
                    });
                    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        if details.is_empty() {
                            ui.label(RichText::new("—").color(p.text_dim));
                        } else {
                            ui.add(egui::Label::new(RichText::new(details).monospace().size(12.0)).wrap());
                        }
                    });
                });
                ui.separator();
            };
            {
                row(ui, "Environment variables", "This application's process; inherited from your login session", !d.process_env.is_empty(), d.process_env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"));
                let mode = d.gnome.as_ref().and_then(|g| g.iter().find(|(k, _)| k == "mode").map(|(_, v)| v.clone()));
                row(ui, "GNOME / GTK desktop", "Desktop session; honoured by desktop-aware applications", mode.as_deref().map(|m| m != "'none'").unwrap_or(false), match (&d.gnome, &d.gnome_error) {
                    (Some(g), _) => g.iter().filter(|(k, v)| !v.is_empty() && v != "''" && v != "0" && v != "[]" && v != "false" || k == "mode").map(|(k, v)| format!("{k} = {v}")).collect::<Vec<_>>().join("\n"),
                    (_, Some(e)) => format!("unavailable: {e}"),
                    _ => "gsettings not installed".into(),
                });
                let ptype = d.kde.as_ref().and_then(|k| k.iter().find(|(k, _)| k == "ProxyType").map(|(_, v)| v.clone()));
                row(ui, "KDE Plasma", "Desktop session (~/.config/kioslaverc); KDE/KIO applications", matches!(ptype.as_deref(), Some("1") | Some("2") | Some("3") | Some("4")), match &d.kde {
                    Some(k) => k.iter().map(|(a, b)| format!("{a} = {b}")).collect::<Vec<_>>().join("\n"),
                    None => "no kioslaverc".into(),
                });
                row(ui, "User session environment", "~/.config/environment.d (systemd --user); new processes after login", d.user_env_file.is_some() || !d.other_env_d.is_empty(), [d.user_env_file.clone().unwrap_or_default(), d.other_env_d.iter().map(|(f, l)| format!("{f}: {l}")).collect::<Vec<_>>().join("\n")].join("\n").trim().to_string());
                row(ui, "System-wide", "/etc/environment; all users, next login", !d.etc_env.is_empty(), d.etc_env.join("\n"));
                row(ui, "Application-specific", "git, npm, curl, wget, apt, dnf, docker...; detected only, never modified", !d.app_specific.is_empty(), d.app_specific.iter().map(|(a, b)| format!("{a}: {b}")).collect::<Vec<_>>().join("\n"));
            }
        });
        if rescan {
            self.rescan(&ctx);
        }
        ui.add_space(8.0);

        // ---- profiles
        let mut apply_idx = None;
        let mut delete_idx = None;
        let mut act: Option<Act> = None;
        theme::container(ui, "Profiles", |ui| {
            ui.horizontal_wrapped(|ui| {
                for (i, pr) in self.store.profiles.iter().enumerate() {
                    let active = pr.name == settings.active_proxy;
                    let label = if active { format!("{}  (active)", pr.name) } else { pr.name.clone() };
                    if ui.selectable_label(self.selected == i, label).clicked() {
                        self.selected = i;
                        self.edit_name = pr.name.clone();
                        self.password.clear();
                    }
                }
                if ui.button("+ New profile").clicked() {
                    let mut name = "New profile".to_string();
                    let mut k = 2;
                    while self.store.profiles.iter().any(|p| p.name == name) {
                        name = format!("New profile {k}");
                        k += 1;
                    }
                    self.store.profiles.push(ProxyProfile::custom(&name));
                    self.selected = self.store.profiles.len() - 1;
                    self.edit_name = name;
                }
            });
            ui.separator();
            let idx = self.selected.min(self.store.profiles.len() - 1);
            let kind = self.store.profiles[idx].kind;
            match kind {
                ProfileKind::Direct => ui.label("Direct Connection: disables proxies in every selected scope."),
                ProfileKind::System => ui.label("System Default: removes the overrides made by this application and restores the desktop settings captured at first launch."),
                ProfileKind::Custom => ui.label(""),
            };
            if kind == ProfileKind::Custom {
                let can_pw = self.secret_service;
                let pr = &mut self.store.profiles[idx];
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut self.edit_name).desired_width(240.0));
                });
                egui::Grid::new("slots").num_columns(5).spacing([10.0, 8.0]).show(ui, |ui| {
                    for h in ["", "Entry", "Type", "Host", "Port"] {
                        ui.label(RichText::new(h).strong().color(p.text_dim));
                    }
                    ui.end_row();
                    for (label, slot, choices) in [
                        ("HTTP_PROXY", &mut pr.http, vec![Scheme::Http, Scheme::Https, Scheme::Socks4, Scheme::Socks5]),
                        ("HTTPS_PROXY", &mut pr.https, vec![Scheme::Http, Scheme::Https, Scheme::Socks4, Scheme::Socks5]),
                        ("ALL_PROXY", &mut pr.all, vec![Scheme::Socks5, Scheme::Socks4, Scheme::Http, Scheme::Https]),
                    ] {
                        ui.checkbox(&mut slot.enabled, "");
                        ui.label(label);
                        egui::ComboBox::from_id_salt(label).selected_text(slot.scheme.label()).width(90.0).show_ui(ui, |ui| {
                            for c in choices {
                                ui.selectable_value(&mut slot.scheme, c, c.label());
                            }
                        });
                        ui.add_enabled(slot.enabled, egui::TextEdit::singleline(&mut slot.host).desired_width(240.0).hint_text("proxy.example.com"));
                        ui.add_enabled(slot.enabled, egui::TextEdit::singleline(&mut slot.port).desired_width(70.0).hint_text("8080"));
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Bypass (NO_PROXY)");
                    ui.add(egui::TextEdit::singleline(&mut pr.no_proxy).desired_width(420.0).hint_text("localhost,127.0.0.1,::1,.corp.example"));
                });
                ui.horizontal(|ui| {
                    ui.label("Authentication  user");
                    ui.add(egui::TextEdit::singleline(&mut pr.username).desired_width(150.0));
                    ui.label("password");
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(150.0));
                    if pr.has_password {
                        theme::badge(ui, if can_pw { "stored in keyring" } else { "stored" }, p.success);
                    }
                });
                if !can_pw {
                    ui.label(RichText::new("No Secret Service (keyring) detected: the password is kept in memory for this session only and never written to disk.").color(p.warning).small());
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, "Save profile").clicked() {
                        act = Some(Act::Save);
                    }
                    if ui.button("Test connection").on_hover_text("TCP connect to the first enabled proxy").clicked() {
                        act = Some(Act::Test);
                    }
                    if ui.button("Duplicate").clicked() {
                        act = Some(Act::Dup);
                    }
                    if ui.button("Delete").clicked() {
                        act = Some(Act::Delete);
                    }
                });
            }
            ui.separator();
            ui.label(RichText::new("Apply to").strong());
            let t = &mut self.store.targets;
            ui.checkbox(&mut t.desktop, "Desktop session  (GNOME/GTK gsettings and KDE kioslaverc)");
            ui.checkbox(&mut t.user_env, "User session environment  (environment.d + systemd --user; new processes only)");
            ui.add_enabled(services.pkexec, egui::Checkbox::new(&mut t.system, "System-wide  (/etc/environment via PolicyKit; all users, next login)"));
            ui.checkbox(&mut t.embed_credentials, "Embed credentials in the user environment file (plain text, mode 0600)");
            if t.embed_credentials {
                ui.label(RichText::new("Passwords are never written to desktop settings or system-wide files.").small().color(p.text_dim));
            }
            ui.add_space(4.0);
            ui.add_enabled_ui(!self.busy, |ui| {
                if theme::primary_button(ui, &format!("Apply '{}'", self.store.profiles[idx].name)).clicked() {
                    apply_idx = Some(idx);
                }
            });
        });
        let idx = self.selected.min(self.store.profiles.len() - 1);
        match act {
            Some(Act::Delete) => delete_idx = Some(idx),
            Some(Act::Dup) => {
                let mut c = self.store.profiles[idx].clone();
                c.name = format!("{} copy", c.name);
                c.has_password = false;
                self.store.profiles.push(c);
                self.selected = self.store.profiles.len() - 1;
                self.edit_name = self.store.profiles[self.selected].name.clone();
            }
            Some(Act::Test) => {
                let pr = &self.store.profiles[idx];
                match [&pr.http, &pr.https, &pr.all].into_iter().find(|s| s.enabled).cloned() {
                    Some(s) if valid_host(s.host.trim()) && parse_port(&s.port).is_some() => {
                        let (h, po) = (s.host.trim().to_string(), parse_port(&s.port).unwrap());
                        self.rx.push(spawn(&ctx, move || Msg::Tested(test_endpoint(&h, po))));
                    }
                    _ => self.error = Some("Enter a valid host and port first".into()),
                }
            }
            Some(Act::Save) => {
                let old = self.store.profiles[idx].name.clone();
                self.store.profiles[idx].name = self.edit_name.trim().to_string();
                let snapshot = self.store.profiles[idx].clone();
                let dup = self.store.profiles.iter().enumerate().any(|(i, o)| i != idx && o.name == snapshot.name);
                match validate(&snapshot) {
                    Err(e) => {
                        self.store.profiles[idx].name = old;
                        self.error = Some(e);
                    }
                    Ok(()) if dup => {
                        self.store.profiles[idx].name = old;
                        self.error = Some("A profile with this name already exists".into());
                    }
                    Ok(()) => {
                        self.error = None;
                        if settings.active_proxy == old {
                            settings.active_proxy = snapshot.name.clone();
                            settings.save();
                        }
                        if !self.password.is_empty() {
                            let pw = std::mem::take(&mut self.password);
                            self.session_pw.insert(snapshot.name.clone(), pw.clone());
                            let nm = snapshot.name.clone();
                            let (tx, rx) = std::sync::mpsc::channel();
                            let ctx2 = ctx.clone();
                            std::thread::spawn(move || {
                                let r = block_on(crate::secrets::store("proxy", &nm, &format!("SystemMonitor proxy '{nm}'"), &pw));
                                let _ = tx.send(Msg::PasswordSaved(nm, r));
                                ctx2.request_repaint();
                            });
                            self.rx.push(rx);
                        }
                        self.save();
                        n.ok("Profile saved");
                    }
                }
            }
            None => {}
        }
        if let Some(i) = delete_idx {
            self.confirm_delete = Some(i);
        }
        if let Some(i) = apply_idx {
            self.save();
            self.start_apply(&ctx, i, services, desktop);
        }
        if let Some(i) = self.confirm_delete {
            let name = self.store.profiles[i].name.clone();
            let m = egui::Modal::new(egui::Id::new("del_proxy")).show(&ctx, |ui| {
                ui.heading("Delete profile?");
                ui.label(format!("\"{name}\" and its stored password will be removed."));
                let mut done = None;
                ui.horizontal(|ui| {
                    if theme::danger_button(ui, "Delete").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
                done
            });
            if let Some(yes) = m.inner {
                if yes {
                    self.store.profiles.remove(i);
                    self.selected = 0;
                    self.edit_name = self.store.profiles[0].name.clone();
                    self.save();
                    let nm = name.clone();
                    std::thread::spawn(move || {
                        let _ = block_on(crate::secrets::delete("proxy", &nm));
                    });
                }
                self.confirm_delete = None;
            } else if m.should_close() {
                self.confirm_delete = None;
            }
        }
        ui.add_space(8.0);

        // ---- backups
        let mut make = false;
        theme::container_with(ui, "Backup and restore", |ui| {
            make = ui.button("Create backup now").clicked();
        }, |ui| {
            ui.label("A backup is taken automatically before every change. Restoring also reverts the user environment (and /etc/environment if selected above).");
            for (path, name) in self.backups.clone().iter().take(8) {
                let b: Option<Backup> = fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(b.as_ref().map(|b| fmt_ts(b.ts)).unwrap_or_else(|| name.clone())).monospace());
                    ui.label(b.map(|b| b.label).unwrap_or_default());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.busy, egui::Button::new("Restore")).clicked() {
                            self.confirm_restore = Some(path.clone());
                        }
                    });
                });
            }
        });
        if make {
            match make_backup("Manual backup", None) {
                Ok(_) => {
                    n.ok("Backup created");
                    self.backups = list_backups("proxy");
                }
                Err(e) => n.err(e),
            }
        }
        if let Some(path) = self.confirm_restore.clone() {
            let m = egui::Modal::new(egui::Id::new("restore_proxy")).show(&ctx, |ui| {
                ui.heading("Restore this backup?");
                ui.label("Current desktop and user-environment proxy settings will be replaced.");
                let mut done = None;
                ui.horizontal(|ui| {
                    if theme::primary_button(ui, "Restore").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
                done
            });
            if let Some(yes) = m.inner {
                if yes {
                    self.busy = true;
                    let sys = self.store.targets.system;
                    self.rx.push(spawn(&ctx, move || Msg::Restored(restore_backup(&path, false, sys))));
                    settings.active_proxy = "System Default".into();
                    settings.save();
                }
                self.confirm_restore = None;
            } else if m.should_close() {
                self.confirm_restore = None;
            }
        }
    }
}

#[cfg(test)]
mod gnome_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `gsettings` backed by files. With `stale_mode` its `get mode` ignores writes, like a
    /// backend that accepts sets but does not persist them.
    fn fake_gsettings(stale_mode: bool) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sysmon-fakegs-{}-{}", std::process::id(), crate::linux::now_millis()));
        fs::create_dir_all(d.join("db")).unwrap();
        let script = format!(
            "#!/bin/sh\ndb='{db}'\nk=\"$2.$3\"\ncase \"$1\" in\n  set) echo \"$4\" > \"$db/$k\" ;;\n  get) if [ \"{stale}\" = 1 ] && [ \"$3\" = mode ]; then echo \"'none'\"; elif [ -f \"$db/$k\" ]; then cat \"$db/$k\"; else echo \"''\"; fi ;;\n  list-schemas) echo org.gnome.system.proxy ;;\nesac\n",
            db = d.join("db").display(),
            stale = stale_mode as u8
        );
        fs::write(d.join("gsettings"), script).unwrap();
        fs::set_permissions(d.join("gsettings"), fs::Permissions::from_mode(0o755)).unwrap();
        d
    }

    fn profile() -> ProxyProfile {
        let mut p = ProxyProfile::custom("T");
        p.http = Slot { enabled: true, scheme: Scheme::Http, host: "proxy.test.invalid".into(), port: "3128".into() };
        p
    }

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn unpersisted_write_is_an_error_and_is_rolled_back() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = fake_gsettings(true);
        // previous state: a user value that must survive the failed attempt
        fs::write(d.join("db/org.gnome.system.proxy.http.host"), "'old.example'\n").unwrap();
        let path = format!("{}:{}", d.display(), std::env::var("PATH").unwrap_or_default());
        unsafe { std::env::set_var("PATH", &path) };
        let r = apply_gnome(&profile(), "");
        let host = fs::read_to_string(d.join("db/org.gnome.system.proxy.http.host")).unwrap();
        assert!(r.as_ref().err().map(|e| e.contains("not persisting") && e.contains("restored")).unwrap_or(false), "{r:?}");
        assert_eq!(host.trim(), "'old.example'", "previous value restored");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn persisted_write_verifies() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = fake_gsettings(false);
        let path = format!("{}:{}", d.display(), std::env::var("PATH").unwrap_or_default());
        unsafe { std::env::set_var("PATH", &path) };
        assert!(apply_gnome(&profile(), "").is_ok());
        assert_eq!(fs::read_to_string(d.join("db/org.gnome.system.proxy mode".replace(' ', "."))).unwrap().trim(), "'manual'");
        let _ = fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    fn gdump() -> Vec<String> {
        GNOME_KEYS.iter().map(|(s, k)| format!("{s} {k} = {}", gsettings(&["get", s, k]).unwrap_or_default())).collect()
    }
    fn sysenv() -> String {
        run("systemctl", &["--user", "show-environment"]).unwrap_or_default().lines().filter(|l| l.to_lowercase().contains("_proxy=")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn ini_editing() {
        let src = "[General]\na=1\n\n[Proxy Settings]\nProxyType=0\nhttpProxy=old\n\n[Other]\nz=9\n";
        let out = ini_set(src, "Proxy Settings", &[("ProxyType", Some("1".into())), ("httpProxy", None), ("NoProxyFor", Some("a,b".into()))]);
        assert_eq!(out, "[General]\na=1\n\n[Proxy Settings]\nProxyType=1\nNoProxyFor=a,b\n\n[Other]\nz=9\n");
        assert!(ini_set("", "Proxy Settings", &[("ProxyType", Some("1".into()))]).contains("[Proxy Settings]\nProxyType=1"));
    }

    #[test]
    fn url_building() {
        let e = Ep { scheme: Scheme::Socks5, host: "::1".into(), port: 1080 };
        assert_eq!(url(&e, None), "socks5://[::1]:1080");
        let e = Ep { scheme: Scheme::Http, host: "p.example".into(), port: 3128 };
        assert_eq!(url(&e, Some(("us er", "p@ss:w/rd$"))), "http://us%20er:p%40ss%3Aw%2Frd%24@p.example:3128");
        assert_eq!(mask("http://u:secret@h:1"), "http://***@h:1");
    }

    #[test]
    #[ignore = "modifies live gsettings / systemd --user env, then restores"]
    fn apply_and_restore() {
        let tmp = std::env::temp_dir().join(format!("sysmon-live-{}", std::process::id()));
        // dconf clients read $XDG_CONFIG_HOME/dconf/user while the running dconf-service writes to the real
        // database; keep both views identical or the test would read defaults and "roll back" to them.
        let real_dconf = config_home().join("dconf");
        fs::create_dir_all(&tmp).unwrap();
        let _ = std::os::unix::fs::symlink(&real_dconf, tmp.join("dconf"));
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) };
        let services = detect_services();
        let desktop = Desktop { kde: true, ..Default::default() };
        let (g0, e0) = (gdump(), sysenv());
        let mut p = ProxyProfile::custom("Test");
        p.http = Slot { enabled: true, scheme: Scheme::Http, host: "proxy.test.invalid".into(), port: "3128".into() };
        p.all = Slot { enabled: true, scheme: Scheme::Socks5, host: "127.0.0.1".into(), port: "1080".into() };
        p.username = "alice".into();
        let t = Targets { desktop: true, user_env: true, system: false, embed_credentials: true };
        let r = apply(&p, t, Some("s3cr:et".into()), &services, &desktop);
        println!("apply -> {r:?}");
        r.unwrap();
        println!("gsettings now: {:#?}", gdump());
        println!("env.d file:\n{}", fs::read_to_string(env_d_file()).unwrap());
        println!("kioslaverc:\n{}", fs::read_to_string(kioslaverc()).unwrap());
        println!("systemd env: {}", sysenv());
        println!("mode of env file: {:o}", std::os::unix::fs::MetadataExt::mode(&fs::metadata(env_d_file()).unwrap()) & 0o777);
        assert!(sysenv().contains("http_proxy=http://alice:s3cr%3Aet@proxy.test.invalid:3128"));
        let d = detect();
        assert!(d.user_env_file.unwrap().contains("***@"), "credentials masked in UI");
        // Direct profile, then restore the pre-apply backup
        let direct = ProxyProfile::builtin("Direct Connection", ProfileKind::Direct);
        apply(&direct, t, None, &services, &desktop).unwrap();
        assert!(!env_d_file().exists() && sysenv().is_empty());
        let first = list_backups("proxy").into_iter().last().unwrap().0; // oldest = before 'Test'
        println!("restore -> {:?}", restore_backup(&first, false, false));
        println!("kioslaverc after restore exists: {}", kioslaverc().exists());
        assert_eq!(gdump(), g0, "gsettings restored");
        assert_eq!(sysenv(), e0, "systemd env restored");
        assert!(!env_d_file().exists());
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    #[ignore = "uses the desktop keyring"]
    fn keyring_roundtrip() {
        block_on(async {
            crate::secrets::store("test", "sysmon-test", "SystemMonitor test item", "p@ss wörd").await.expect("store");
            let got = crate::secrets::lookup("test", "sysmon-test").await.expect("lookup");
            assert_eq!(got.as_deref(), Some("p@ss wörd"));
            crate::secrets::delete("test", "sysmon-test").await.expect("delete");
            assert_eq!(crate::secrets::lookup("test", "sysmon-test").await.unwrap(), None);
        });
    }
}
