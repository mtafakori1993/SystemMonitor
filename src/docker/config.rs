//! Docker daemon configuration (`/etc/docker/daemon.json`) and Docker proxy settings.
//!
//! * Reading is unprivileged. Writing and restarting go through `pkexec` to this same executable
//!   (`--helper-docker write <staged>` / `--helper-docker restart`), which re-validates the file,
//!   keeps a root-only backup, writes atomically, and runs a fixed `systemctl restart docker` argv.
//! * Changes are merged into the existing document: keys this module does not edit are preserved.
//! * Daemon proxy (`"proxies"` in daemon.json, Docker 23+) is used by the daemon itself: pulls, pushes,
//!   registry logins. It needs a daemon restart. The client proxy (`~/.docker/config.json`,
//!   `proxies.default`) is injected by the docker CLI into new containers and builds; no restart.
//! * Proxy passwords end up inside these files (Docker has no other place for them). They are shown
//!   masked, never logged, and user-side backups are 0600 in the 0700 config directory.
use crate::linux::{app_dir, backup_dir, home, now_millis, now_secs, parse_dns_ip, parse_port, valid_bypass, valid_host, which, write_atomic};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const DAEMON_JSON: &str = "/etc/docker/daemon.json";
const MAX_CONFIG: u64 = 256 << 10;
const ROOT_BACKUPS_KEPT: usize = 10;

/// daemon.json location; tests may redirect it, but only when not running as root.
pub fn daemon_json_path() -> PathBuf {
    if unsafe { libc::geteuid() } != 0 {
        if let Some(d) = std::env::var_os("SYSMON_TEST_DOCKER_DIR") {
            return PathBuf::from(d).join("daemon.json");
        }
    }
    PathBuf::from(DAEMON_JSON)
}

/// Current document (`{}` when the file does not exist) and whether it exists.
pub fn read_daemon_json() -> Result<(Map<String, Value>, bool), String> {
    let p = daemon_json_path();
    match fs::read_to_string(&p) {
        Ok(s) if s.trim().is_empty() => Ok((Map::new(), true)),
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => Ok((m, true)),
            Ok(_) => Err(format!("{} is not a JSON object", p.display())),
            Err(e) => Err(format!("{} is not valid JSON: {e}", p.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Map::new(), false)),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

pub fn pretty(m: &Map<String, Value>) -> String {
    serde_json::to_string_pretty(&Value::Object(m.clone())).unwrap_or_else(|_| "{}".into()) + "\n"
}

/// Set (`Some`) or remove (`None`) top-level keys; every other key is kept as it was.
pub fn merge(base: &Map<String, Value>, changes: &[(&str, Option<Value>)]) -> Map<String, Value> {
    let mut m = base.clone();
    for (k, v) in changes {
        match v {
            Some(v) => m.insert(k.to_string(), v.clone()),
            None => m.remove(*k),
        };
    }
    m
}

// ---------------------------------------------------------------- validation

#[derive(Default, Debug, Clone, PartialEq)]
pub struct Check {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Check {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

const LOG_DRIVERS: [&str; 12] = ["json-file", "local", "journald", "syslog", "none", "fluentd", "gelf", "awslogs", "splunk", "gcplogs", "etwlogs", "logentries"];
/// Keys with a dedicated check below; others are passed to `dockerd --validate` and preserved untouched.
const KNOWN: [&str; 17] = ["registry-mirrors", "insecure-registries", "dns", "dns-search", "dns-opts", "log-driver", "log-level", "log-opts", "data-root", "storage-driver", "storage-opts", "live-restore", "debug", "proxies", "hosts", "max-concurrent-downloads", "max-concurrent-uploads"];

fn strings<'a>(v: &'a Value, key: &str, c: &mut Check) -> Vec<&'a str> {
    match v.as_array() {
        Some(a) if a.iter().all(Value::is_string) => a.iter().filter_map(Value::as_str).collect(),
        _ => {
            c.errors.push(format!("\"{key}\" must be a list of strings"));
            vec![]
        }
    }
}

/// `http[s]://host[:port][/path]` without spaces.
pub fn valid_http_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) else { return false };
    let hostport = rest.split('/').next().unwrap_or("");
    let (h, p) = split_host_port(hostport);
    !u.chars().any(|c| c.is_whitespace() || c.is_control()) && valid_host(h) && p.map(|p| parse_port(p).is_some()).unwrap_or(true)
}

fn split_host_port(hp: &str) -> (&str, Option<&str>) {
    if let Some(r) = hp.strip_prefix('[') {
        let (h, after) = r.split_once(']').unwrap_or((r, ""));
        return (h, after.strip_prefix(':'));
    }
    match hp.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, Some(p)),
        _ => (hp, None),
    }
}

/// Size such as "10m", "1g", "512k", "100" (bytes).
fn valid_size(s: &str) -> bool {
    let t = s.trim_end_matches(|c: char| "kmgKMG".contains(c)).trim_end_matches(['b', 'B']);
    let t = if t.len() == s.len() { s } else { t };
    !t.is_empty() && t.len() <= 12 && t.bytes().all(|b| b.is_ascii_digit()) && t != "0"
}

pub fn validate_daemon(m: &Map<String, Value>) -> Check {
    let mut c = Check::default();
    for (k, v) in m {
        match k.as_str() {
            "registry-mirrors" => {
                for u in strings(v, k, &mut c) {
                    if !valid_http_url(u) {
                        c.errors.push(format!("registry mirror '{u}' must be an http:// or https:// URL"));
                    } else if u.starts_with("http://") {
                        c.warnings.push(format!("registry mirror {u} uses plain HTTP"));
                    }
                }
            }
            "insecure-registries" => {
                for r in strings(v, k, &mut c) {
                    let (h, p) = split_host_port(r);
                    let ok = crate::docker::api::valid_cidr(r) || (valid_host(h) && p.map(|p| parse_port(p).is_some()).unwrap_or(true));
                    if !ok {
                        c.errors.push(format!("insecure registry '{r}' must be host[:port] or a CIDR"));
                    }
                }
                if v.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                    c.warnings.push("insecure registries are contacted without TLS verification".into());
                }
            }
            "dns" => {
                for d in strings(v, k, &mut c) {
                    if let Err(e) = parse_dns_ip(d) {
                        c.errors.push(e);
                    }
                }
            }
            "dns-search" | "dns-opts" | "storage-opts" => {
                strings(v, k, &mut c);
            }
            "log-driver" => match v.as_str() {
                Some(d) if LOG_DRIVERS.contains(&d) => {}
                Some(d) => c.warnings.push(format!("log driver '{d}' is not a built-in driver (a plugin must be installed)")),
                None => c.errors.push("\"log-driver\" must be a string".into()),
            },
            "log-level" => {
                if !matches!(v.as_str(), Some("debug" | "info" | "warn" | "error" | "fatal")) {
                    c.errors.push("\"log-level\" must be debug, info, warn, error or fatal".into());
                }
            }
            "log-opts" => match v.as_object() {
                Some(o) => {
                    for (ok, ov) in o {
                        // dockerd only accepts strings here ("max-file": 3 makes the daemon refuse to start)
                        let Some(s) = ov.as_str() else {
                            c.errors.push(format!("log option \"{ok}\" must be a string, e.g. \"{ov}\""));
                            continue;
                        };
                        if ok == "max-size" && !valid_size(s) {
                            c.errors.push(format!("log option max-size '{s}' must look like 10m, 512k or 1g"));
                        }
                        if ok == "max-file" && s.parse::<u32>().map(|n| n == 0).unwrap_or(true) {
                            c.errors.push(format!("log option max-file '{s}' must be a positive number"));
                        }
                    }
                }
                None => c.errors.push("\"log-opts\" must be an object of strings".into()),
            },
            "data-root" => match v.as_str() {
                Some(p) if p.starts_with('/') && !p.contains("..") && !p.chars().any(|c| c.is_control()) => c.warnings.push("changing data-root makes existing images, containers and volumes invisible until moved".into()),
                _ => c.errors.push("\"data-root\" must be an absolute path".into()),
            },
            "storage-driver" => match v.as_str() {
                Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => c.warnings.push("changing the storage driver hides existing images and containers".into()),
                _ => c.errors.push("\"storage-driver\" must be a driver name".into()),
            },
            "live-restore" | "debug" => {
                if !v.is_boolean() {
                    c.errors.push(format!("\"{k}\" must be true or false"));
                }
            }
            "max-concurrent-downloads" | "max-concurrent-uploads" => {
                if v.as_u64().map(|n| n == 0 || n > 64).unwrap_or(true) {
                    c.errors.push(format!("\"{k}\" must be a number between 1 and 64"));
                }
            }
            "proxies" => match v.as_object() {
                Some(o) => {
                    for (pk, pv) in o {
                        let Some(s) = pv.as_str() else {
                            c.errors.push(format!("proxies.{pk} must be a string"));
                            continue;
                        };
                        match pk.as_str() {
                            "http-proxy" | "https-proxy" => {
                                if let Err(e) = ProxyUrl::parse(s) {
                                    c.errors.push(format!("proxies.{pk}: {e}"));
                                }
                            }
                            "no-proxy" => {
                                if let Some(bad) = s.split(',').map(str::trim).find(|e| !e.is_empty() && !valid_bypass(e)) {
                                    c.errors.push(format!("proxies.no-proxy: invalid entry '{bad}'"));
                                }
                            }
                            _ => c.errors.push(format!("proxies.{pk} is not a daemon proxy setting (use http-proxy, https-proxy, no-proxy)")),
                        }
                    }
                }
                None => c.errors.push("\"proxies\" must be an object".into()),
            },
            "hosts" => {
                for h in strings(v, k, &mut c) {
                    if h.starts_with("tcp://") && m.get("tlsverify").and_then(Value::as_bool) != Some(true) {
                        c.errors.push(format!("refusing to expose the Docker API on {h} without \"tlsverify\": anyone who can reach it gets root on this host"));
                    }
                }
                c.warnings.push("\"hosts\" in daemon.json conflicts with a -H flag in the systemd unit; dockerd will not start if both are set".into());
            }
            _ => {}
        }
    }
    let unknown: Vec<&str> = m.keys().map(String::as_str).filter(|k| !KNOWN.contains(k)).collect();
    if !unknown.is_empty() {
        c.warnings.push(format!("not checked here (left to dockerd --validate): {}", unknown.join(", ")));
    }
    c
}

/// Parse and check document text: JSON object + `validate_daemon`.
pub fn check_text(text: &str) -> (Option<Map<String, Value>>, Check) {
    if text.len() as u64 > MAX_CONFIG {
        return (None, Check { errors: vec!["configuration is larger than 256 KiB".into()], ..Default::default() });
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => {
            let c = validate_daemon(&m);
            (Some(m), c)
        }
        Ok(_) => (None, Check { errors: vec!["the configuration must be a JSON object { ... }".into()], ..Default::default() }),
        Err(e) => (None, Check { errors: vec![format!("invalid JSON: {e}")], ..Default::default() }),
    }
}

/// `dockerd --validate` on a private staged copy. `None` when dockerd is not installed here.
pub fn dockerd_validate(text: &str) -> Option<Result<(), String>> {
    let dockerd = which("dockerd").or_else(|| Some(PathBuf::from("/usr/bin/dockerd")).filter(|p| p.exists()))?;
    let staged = app_dir().join(format!(".daemon-validate-{}.json", now_millis()));
    if let Err(e) = write_atomic(&staged, text, 0o600) {
        return Some(Err(e));
    }
    let out = std::process::Command::new(dockerd).arg("--validate").arg("--config-file").arg(&staged).stdin(std::process::Stdio::null()).output();
    let _ = fs::remove_file(&staged);
    Some(match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
            // the message names the staged path; show the real one instead
            Err(crate::docker::audit::scrub(&msg.replace(&staged.display().to_string(), DAEMON_JSON)))
        }
        Err(e) => Err(format!("dockerd: {e}")),
    })
}

// ---------------------------------------------------------------- backups (user side)

/// Copy of the current daemon.json into the app's backup directory (0600). Returns the backup name.
pub fn backup_daemon_json(label: &str) -> Result<String, String> {
    let (m, exists) = read_daemon_json()?;
    let doc = json!({"label": label, "ts": now_secs(), "existed": exists, "config": Value::Object(m)});
    let stem = format!("{}", now_millis());
    write_atomic(&backup_dir("docker").join(format!("{stem}.json")), &serde_json::to_string_pretty(&doc).unwrap_or_default(), 0o600)?;
    Ok(stem)
}

pub struct BackupInfo {
    pub path: PathBuf,
    pub label: String,
    pub ts: u64,
}

pub fn list_daemon_backups() -> Vec<BackupInfo> {
    crate::linux::list_backups("docker")
        .into_iter()
        .filter_map(|(p, _)| {
            let v: Value = serde_json::from_str(&fs::read_to_string(&p).ok()?).ok()?;
            Some(BackupInfo { label: v.get("label")?.as_str()?.to_string(), ts: v.get("ts")?.as_u64()?, path: p })
        })
        .collect()
}

pub fn read_backup(p: &Path) -> Result<Map<String, Value>, String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(p).map_err(|e| e.to_string())?).map_err(|e| format!("corrupt backup: {e}"))?;
    match v.get("config") {
        Some(Value::Object(m)) => Ok(m.clone()),
        _ => Err("backup has no configuration".into()),
    }
}

// ---------------------------------------------------------------- privileged apply / restart

fn run_helper(args: &[&str]) -> Result<String, String> {
    if which("pkexec").is_none() {
        return Err("PolicyKit (pkexec) is not installed: an administrator must edit /etc/docker/daemon.json by hand".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new("pkexec").arg(&exe).arg("--helper-docker").args(args).stdin(std::process::Stdio::null()).output().map_err(|e| format!("pkexec: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Some(126) => Err("Authentication was cancelled or not authorized".into()),
        Some(127) => Err("PolicyKit could not authenticate (no authentication agent running?)".into()),
        _ => Err(crate::docker::audit::scrub(String::from_utf8_lossy(&out.stderr).trim())),
    }
}

/// Validate, back up (user side), then write through the helper. The text is the complete new document.
pub fn apply_daemon_json(text: &str, label: &str) -> Result<String, String> {
    let (m, c) = check_text(text);
    let m = m.filter(|_| c.ok()).ok_or_else(|| c.errors.join("; "))?;
    if let Some(Err(e)) = dockerd_validate(text) {
        return Err(format!("dockerd rejected the configuration: {e}"));
    }
    let b = backup_daemon_json(&format!("Before: {label}"))?;
    let staged = app_dir().join("staging-daemon.json");
    write_atomic(&staged, &pretty(&m), 0o600)?;
    let r = run_helper(&["write", &staged.display().to_string()]);
    let _ = fs::remove_file(&staged);
    r.map(|root_bak| format!("daemon.json updated (your backup: {b}{}). Restart Docker to apply.", if root_bak.is_empty() { String::new() } else { format!("; root copy: {root_bak}") }))
}

/// Restart through the helper, then wait for the engine to answer again.
pub fn restart_daemon(socket: &Path) -> Result<String, String> {
    run_helper(&["restart"])?;
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(90) {
        if crate::docker::http::request(socket, std::time::Duration::from_secs(2), "GET", "/_ping", None).map(|r| r.status == 200).unwrap_or(false) {
            return Ok(format!("Docker restarted and is answering again ({} s)", t0.elapsed().as_secs()));
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
    Err("Docker was restarted but does not answer: check `systemctl status docker` and `journalctl -u docker` (the previous daemon.json can be restored from Backups)".into())
}

/// Root side: `SystemMonitor --helper-docker write <staged>` | `--helper-docker restart`.
pub fn helper_main(args: &[String]) -> i32 {
    use std::os::unix::fs::MetadataExt;
    let root = unsafe { libc::geteuid() } == 0;
    let test_dir = std::env::var_os("SYSMON_TEST_DOCKER_DIR").filter(|_| !root);
    if !root && test_dir.is_none() {
        eprintln!("helper must run through pkexec");
        return 2;
    }
    match args.first().map(String::as_str) {
        Some("restart") if args.len() == 1 => {
            if test_dir.is_some() {
                return 0;
            }
            let st = std::process::Command::new("systemctl").args(["restart", "docker.service"]).stdin(std::process::Stdio::null()).status();
            match st {
                Ok(s) if s.success() => 0,
                Ok(s) => {
                    eprintln!("systemctl restart docker.service failed ({s})");
                    1
                }
                Err(e) => {
                    eprintln!("systemctl: {e}");
                    1
                }
            }
        }
        Some("write") if args.len() == 2 => {
            let staged = &args[1];
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
            if md.len() > MAX_CONFIG {
                eprintln!("staged file too large");
                return 2;
            }
            let text = fs::read_to_string(staged).unwrap_or_default();
            let (m, c) = check_text(&text);
            let Some(m) = m.filter(|_| c.ok()) else {
                eprintln!("rejected: {}", c.errors.join("; "));
                return 2;
            };
            let target = daemon_json_path();
            let dir = target.parent().unwrap_or(Path::new("/etc/docker")).to_path_buf();
            if let Err(e) = fs::create_dir_all(&dir) {
                eprintln!("{}: {e}", dir.display());
                return 1;
            }
            let mut bak = String::new();
            if let Ok(cur) = fs::read_to_string(&target) {
                bak = format!("{}.systemmonitor-{}.bak", target.display(), now_secs());
                // may hold proxy credentials: root-only
                if let Err(e) = write_atomic(Path::new(&bak), &cur, 0o600) {
                    eprintln!("backup failed: {e}");
                    return 1;
                }
                prune_root_backups(&dir);
            }
            // daemon.json itself stays 0644 unless it holds credentials
            let mode = if m.get("proxies").map(|p| p.to_string().contains('@')).unwrap_or(false) { 0o600 } else { 0o644 };
            if let Err(e) = write_atomic(&target, &pretty(&m), mode) {
                eprintln!("write failed: {e}");
                return 1;
            }
            println!("{bak}");
            0
        }
        _ => {
            eprintln!("usage: --helper-docker write <staged> | restart");
            2
        }
    }
}

fn prune_root_backups(dir: &Path) {
    let mut v: Vec<PathBuf> = fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("daemon.json.systemmonitor-")).unwrap_or(false)).collect()).unwrap_or_default();
    v.sort();
    while v.len() > ROOT_BACKUPS_KEPT {
        let _ = fs::remove_file(v.remove(0));
    }
}

// ---------------------------------------------------------------- proxy

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProxyScheme {
    Http,
    Https,
    Socks5,
}

impl ProxyScheme {
    pub const ALL: [ProxyScheme; 3] = [ProxyScheme::Http, ProxyScheme::Https, ProxyScheme::Socks5];
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyScheme::Http => "http",
            ProxyScheme::Https => "https",
            ProxyScheme::Socks5 => "socks5",
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct ProxyUrl {
    pub scheme: ProxyScheme,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl ProxyUrl {
    pub fn parse(u: &str) -> Result<ProxyUrl, String> {
        let u = u.trim();
        let (scheme, rest) = u.split_once("://").ok_or("must look like http://host:port")?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "http" => ProxyScheme::Http,
            "https" => ProxyScheme::Https,
            "socks5" | "socks5h" => ProxyScheme::Socks5,
            s => return Err(format!("scheme '{s}' is not supported (http, https, socks5)")),
        };
        let rest = rest.trim_end_matches('/');
        let (auth, hp) = match rest.rfind('@') {
            Some(i) => (Some(&rest[..i]), &rest[i + 1..]),
            None => (None, rest),
        };
        let (h, p) = split_host_port(hp);
        if !valid_host(h) {
            return Err(format!("'{h}' is not a valid host"));
        }
        let port = match p {
            Some(p) => parse_port(p).ok_or("port must be 1-65535")?,
            None => match scheme {
                ProxyScheme::Http => 80,
                ProxyScheme::Https => 443,
                ProxyScheme::Socks5 => 1080,
            },
        };
        let (user, password) = match auth {
            Some(a) => {
                let (u, p) = a.split_once(':').unwrap_or((a, ""));
                (pct_decode(u), pct_decode(p))
            }
            None => (String::new(), String::new()),
        };
        if user.chars().chain(password.chars()).any(|c| c.is_control()) {
            return Err("credentials contain control characters".into());
        }
        Ok(ProxyUrl { scheme, host: h.to_string(), port, user, password })
    }

    pub fn to_url(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let auth = match (self.user.is_empty(), self.password.is_empty()) {
            (true, _) => String::new(),
            (false, true) => format!("{}@", crate::proxy::pct_encode(&self.user)),
            (false, false) => format!("{}:{}@", crate::proxy::pct_encode(&self.user), crate::proxy::pct_encode(&self.password)),
        };
        format!("{}://{auth}{host}:{}", self.scheme.as_str(), self.port)
    }

    /// For display: the password is never shown.
    pub fn masked(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let auth = if self.user.is_empty() { String::new() } else { format!("{}:***@", self.user) };
        format!("{}://{auth}{host}:{}", self.scheme.as_str(), self.port)
    }
}

/// The proxy values of one scope. Empty string = not set.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct ProxySet {
    pub http: String,
    pub https: String,
    pub all: String,
    pub no_proxy: String,
}

impl ProxySet {
    pub fn is_empty(&self) -> bool {
        self.http.is_empty() && self.https.is_empty() && self.all.is_empty() && self.no_proxy.is_empty()
    }
    pub fn validate(&self, daemon: bool) -> Result<(), String> {
        for (k, v) in [("HTTP_PROXY", &self.http), ("HTTPS_PROXY", &self.https), ("ALL_PROXY", &self.all)] {
            if !v.is_empty() {
                ProxyUrl::parse(v).map_err(|e| format!("{k}: {e}"))?;
            }
        }
        if daemon && !self.all.is_empty() {
            return Err("ALL_PROXY is not supported by the Docker daemon (only HTTP_PROXY, HTTPS_PROXY and NO_PROXY)".into());
        }
        if let Some(bad) = self.no_proxy.split(',').map(str::trim).find(|e| !e.is_empty() && !valid_bypass(e)) {
            return Err(format!("NO_PROXY: invalid entry '{bad}'"));
        }
        Ok(())
    }
    pub fn masked(&self) -> ProxySet {
        let m = |s: &str| if s.is_empty() { String::new() } else { ProxyUrl::parse(s).map(|u| u.masked()).unwrap_or_else(|_| crate::proxy::mask(s)) };
        ProxySet { http: m(&self.http), https: m(&self.https), all: m(&self.all), no_proxy: self.no_proxy.clone() }
    }
}

pub fn daemon_proxies(m: &Map<String, Value>) -> ProxySet {
    let g = |k: &str| m.get("proxies").and_then(|p| p.get(k)).and_then(Value::as_str).unwrap_or_default().to_string();
    ProxySet { http: g("http-proxy"), https: g("https-proxy"), all: String::new(), no_proxy: g("no-proxy") }
}

/// The `proxies` change for daemon.json (`None` removes the key = proxy disabled).
pub fn daemon_proxy_change(p: &ProxySet) -> Option<Value> {
    if p.is_empty() {
        return None;
    }
    let mut o = Map::new();
    for (k, v) in [("http-proxy", &p.http), ("https-proxy", &p.https), ("no-proxy", &p.no_proxy)] {
        if !v.is_empty() {
            o.insert(k.into(), json!(v));
        }
    }
    Some(Value::Object(o))
}

pub const SECRET_PLACEHOLDER: &str = "__hidden__";

/// Replace proxy passwords in a daemon.json document with a placeholder before it is shown in an editor.
/// Returns the originals keyed by proxy key, for `restore_proxy_secrets`.
pub fn mask_proxy_secrets(m: &mut Map<String, Value>) -> std::collections::HashMap<String, String> {
    let mut saved = std::collections::HashMap::new();
    if let Some(Value::Object(p)) = m.get_mut("proxies") {
        for (k, v) in p.iter_mut() {
            if let Some(mut u) = v.as_str().and_then(|s| ProxyUrl::parse(s).ok()).filter(|u| !u.password.is_empty()) {
                saved.insert(k.clone(), std::mem::replace(&mut u.password, SECRET_PLACEHOLDER.into()));
                *v = json!(u.to_url());
            }
        }
    }
    saved
}

/// Put the real passwords back where the placeholder is still present (an edited password is kept as typed).
pub fn restore_proxy_secrets(m: &mut Map<String, Value>, saved: &std::collections::HashMap<String, String>) {
    if let Some(Value::Object(p)) = m.get_mut("proxies") {
        for (k, v) in p.iter_mut() {
            if let (Some(mut u), Some(pw)) = (v.as_str().and_then(|s| ProxyUrl::parse(s).ok()), saved.get(k)) {
                if u.password == SECRET_PLACEHOLDER {
                    u.password = pw.clone();
                    *v = json!(u.to_url());
                }
            }
        }
    }
}

/// Proxy variables in the systemd unit environment (the pre-23 way); they conflict with daemon.json.
pub fn systemd_proxy_dropins() -> Vec<(String, String)> {
    let mut out = vec![];
    for dir in ["/etc/systemd/system/docker.service.d", "/run/systemd/system/docker.service.d"] {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            for l in fs::read_to_string(e.path()).unwrap_or_default().lines() {
                if l.to_ascii_lowercase().contains("_proxy=") {
                    out.push((e.path().display().to_string(), crate::docker::audit::scrub(l.trim())));
                }
            }
        }
    }
    out
}

// ---- client config (~/.docker/config.json)

pub fn client_config_path() -> PathBuf {
    std::env::var_os("DOCKER_CONFIG").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".docker")).join("config.json")
}

fn read_client_config() -> Result<Map<String, Value>, String> {
    let p = client_config_path();
    match fs::read_to_string(&p) {
        Ok(s) if s.trim().is_empty() => Ok(Map::new()),
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(format!("{} is not a JSON object; not touching it", p.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

pub fn client_proxies() -> Result<ProxySet, String> {
    let m = read_client_config()?;
    let g = |k: &str| m.get("proxies").and_then(|p| p.get("default")).and_then(|d| d.get(k)).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(ProxySet { http: g("httpProxy"), https: g("httpsProxy"), all: g("allProxy"), no_proxy: g("noProxy") })
}

/// Pure merge for `~/.docker/config.json`: only `proxies.default` changes; `auths`, `credsStore`,
/// other proxy entries and everything else are preserved.
pub fn merge_client_proxies(mut m: Map<String, Value>, p: &ProxySet) -> Map<String, Value> {
    let mut proxies = m.remove("proxies").and_then(|v| if let Value::Object(o) = v { Some(o) } else { None }).unwrap_or_default();
    let mut d = Map::new();
    for (k, v) in [("httpProxy", &p.http), ("httpsProxy", &p.https), ("allProxy", &p.all), ("noProxy", &p.no_proxy)] {
        if !v.is_empty() {
            d.insert(k.into(), json!(v));
        }
    }
    if d.is_empty() {
        proxies.remove("default");
    } else {
        proxies.insert("default".into(), Value::Object(d));
    }
    if !proxies.is_empty() {
        m.insert("proxies".into(), Value::Object(proxies));
    }
    m
}

pub fn apply_client_proxies(p: &ProxySet) -> Result<String, String> {
    p.validate(false)?;
    let path = client_config_path();
    let cur = read_client_config()?;
    let stem = format!("client-{}", now_millis());
    write_atomic(&backup_dir("docker-client").join(format!("{stem}.json")), &pretty(&cur), 0o600)?;
    let mode = fs::metadata(&path).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777).unwrap_or(0o600);
    write_atomic(&path, &pretty(&merge_client_proxies(cur, p)), mode)?;
    Ok(format!("{} updated (backup {stem}); new containers and builds started with the docker CLI get these variables", path.display()))
}

// ---- connectivity test

fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// Open a tunnel through the proxy to `target:port` (HTTP CONNECT or SOCKS5 with optional
/// username/password). Success proves the proxy is reachable, accepts the credentials and can reach the
/// target. For an https:// proxy only the TCP connection is checked (TLS to the proxy needs a TLS library).
pub fn test_proxy(p: &ProxyUrl, target: &str, port: u16) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    if !valid_host(target) {
        return Err(format!("'{target}' is not a valid test host"));
    }
    let t = std::time::Duration::from_secs(6);
    let addr = (p.host.as_str(), p.port).to_socket_addrs().map_err(|e| format!("cannot resolve proxy {}: {e}", p.host))?.next().ok_or("no proxy address")?;
    let t0 = std::time::Instant::now();
    let mut s = std::net::TcpStream::connect_timeout(&addr, t).map_err(|e| format!("cannot connect to proxy {addr}: {e}"))?;
    s.set_read_timeout(Some(t)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(t)).map_err(|e| e.to_string())?;
    let io = |e: std::io::Error| format!("proxy connection: {e}");
    match p.scheme {
        ProxyScheme::Https => Ok(format!("TCP connection to the HTTPS proxy works ({} ms); the TLS handshake and credentials are not verified by this test", t0.elapsed().as_millis())),
        ProxyScheme::Http => {
            let mut req = format!("CONNECT {target}:{port} HTTP/1.1\r\nHost: {target}:{port}\r\n");
            if !p.user.is_empty() {
                req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", b64(format!("{}:{}", p.user, p.password).as_bytes())));
            }
            req.push_str("\r\n");
            s.write_all(req.as_bytes()).map_err(io)?;
            let mut buf = [0u8; 512];
            let n = s.read(&mut buf).map_err(io)?;
            let line = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or_default().to_string();
            match line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()) {
                Some(200) => Ok(format!("tunnel to {target}:{port} through the proxy works ({} ms)", t0.elapsed().as_millis())),
                Some(407) => Err("the proxy requires authentication, or rejected the user name / password (HTTP 407)".into()),
                Some(c) => Err(format!("the proxy refused the tunnel to {target}:{port} (HTTP {c})")),
                None => Err(format!("not an HTTP proxy response: {:?}", line.chars().take(60).collect::<String>())),
            }
        }
        ProxyScheme::Socks5 => {
            let auth = !p.user.is_empty();
            s.write_all(if auth { &[5, 2, 0, 2] } else { &[5, 1, 0] }).map_err(io)?;
            let mut r = [0u8; 2];
            s.read_exact(&mut r).map_err(io)?;
            if r[0] != 5 {
                return Err("not a SOCKS5 proxy".into());
            }
            match r[1] {
                0 => {}
                2 => {
                    let (u, pw) = (p.user.as_bytes(), p.password.as_bytes());
                    if u.len() > 255 || pw.len() > 255 {
                        return Err("SOCKS5 user name / password longer than 255 bytes".into());
                    }
                    let mut m = vec![1, u.len() as u8];
                    m.extend(u);
                    m.push(pw.len() as u8);
                    m.extend(pw);
                    s.write_all(&m).map_err(io)?;
                    s.read_exact(&mut r).map_err(io)?;
                    if r[1] != 0 {
                        return Err("the SOCKS5 proxy rejected the user name / password".into());
                    }
                }
                0xff => return Err("the SOCKS5 proxy accepts none of the offered authentication methods (it may need credentials)".into()),
                m => return Err(format!("unsupported SOCKS5 method {m}")),
            }
            let mut req = vec![5, 1, 0, 3, target.len() as u8];
            req.extend(target.as_bytes());
            req.extend(port.to_be_bytes());
            s.write_all(&req).map_err(io)?;
            let mut rep = [0u8; 4];
            s.read_exact(&mut rep).map_err(io)?;
            const ERR: [&str; 9] = ["succeeded", "general failure", "connection not allowed by ruleset", "network unreachable", "host unreachable", "connection refused", "TTL expired", "command not supported", "address type not supported"];
            if rep[1] == 0 {
                Ok(format!("SOCKS5 tunnel to {target}:{port} works ({} ms)", t0.elapsed().as_millis()))
            } else {
                Err(format!("SOCKS5 proxy could not reach {target}:{port}: {}", ERR.get(rep[1] as usize).unwrap_or(&"unknown error")))
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn b64_for_test(s: &str) -> String {
    b64(s.as_bytes())
}
