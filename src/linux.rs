//! Linux helpers: file access, safe process execution, validation, service detection.
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Status severity shared by the core (health checks, link state) and the GUI badges.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Success,
    Warning,
    Error,
    Info,
    Inactive,
}

/// Append to a bounded history.
pub fn push(h: &mut std::collections::VecDeque<f32>, v: f32, cap: usize) {
    h.push_back(v);
    while h.len() > cap {
        h.pop_front();
    }
}

pub fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
}

/// Application data directory (profiles, backups, settings).
pub fn app_dir() -> PathBuf {
    let d = config_home().join("SystemMonitor");
    let _ = fs::create_dir_all(&d);
    let _ = fs::set_permissions(&d, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    d
}

/// Atomic write: unique temp file in the same directory, `fsync`, `rename`, `fsync` of the directory,
/// with explicit permission bits. A crash leaves either the old or the new content, never a mix.
pub fn write_atomic(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = parent.join(format!(".{name}.tmp.{}.{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let res = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(content.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.sync_all().map_err(|e| format!("{}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    } else if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all(); // make the rename itself durable
    }
    res
}

/// Run a program with an argument vector (never through a shell).
pub fn run(prog: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(prog);
    cmd.args(args).env("LC_ALL", "C").stdin(std::process::Stdio::null());
    crate::shellenv::apply_to(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("{prog} exited with {}", out.status) } else { err })
    }
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file() && fs::metadata(p).map(|m| m.mode() & 0o111 != 0).unwrap_or(false))
}

pub fn uid_names() -> HashMap<u32, String> {
    let mut m = HashMap::new();
    if let Ok(s) = fs::read_to_string("/etc/passwd") {
        for l in s.lines() {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() > 2 {
                if let Ok(u) = f[2].parse() {
                    m.entry(u).or_insert_with(|| f[0].to_string());
                }
            }
        }
    }
    m
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn human_rate(bps: f64) -> String {
    format!("{}/s", human_bytes(bps.max(0.0) as u64))
}

pub fn human_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m {}s", secs % 60) }
}

// ---------- validation ----------

pub fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    if h.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    h.split('.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

pub fn parse_port(p: &str) -> Option<u16> {
    p.trim().parse::<u16>().ok().filter(|p| *p != 0)
}

/// Parse a DNS server address; rejects unspecified/multicast/broadcast addresses.
pub fn parse_dns_ip(s: &str) -> Result<std::net::IpAddr, String> {
    let ip: std::net::IpAddr = s.trim().parse().map_err(|_| format!("'{}' is not a valid IP address", s.trim()))?;
    if ip.is_unspecified() || ip.is_multicast() {
        return Err(format!("{ip} cannot be used as a DNS server"));
    }
    if let std::net::IpAddr::V4(v4) = ip {
        if v4.is_broadcast() {
            return Err(format!("{ip} cannot be used as a DNS server"));
        }
    }
    Ok(ip)
}

/// Domain/bypass entry such as `localhost`, `.example.com`, `*.corp`, `10.0.0.0/8`, `::1`.
pub fn valid_bypass(e: &str) -> bool {
    let e = e.trim();
    !e.is_empty()
        && e.len() <= 255
        && e.chars().all(|c| c.is_ascii_alphanumeric() || ".-_*:/[]".contains(c))
}

pub fn valid_profile_name(n: &str) -> bool {
    let n = n.trim();
    !n.is_empty() && n.chars().count() <= 48 && !n.chars().any(|c| c.is_control())
}

// ---------- environment / services ----------

#[derive(Clone, Default, Debug)]
pub struct Desktop {
    pub xdg: String,
    pub gnome_like: bool,
    pub kde: bool,
    pub session: String,
}

pub fn desktop() -> Desktop {
    let xdg = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let up = xdg.to_uppercase();
    Desktop {
        gnome_like: ["GNOME", "UNITY", "CINNAMON", "MATE", "BUDGIE", "PANTHEON", "X-CINNAMON", "UBUNTU"]
            .iter()
            .any(|d| up.contains(d)),
        kde: up.contains("KDE"),
        session: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        xdg,
    }
}

#[derive(Clone, Default, Debug)]
pub struct Services {
    pub network_manager: bool,
    pub resolved: bool,
    pub secret_service: bool,
    pub polkit: bool,
    pub pkexec: bool,
    pub gsettings: bool,
    pub smartctl: bool,
}

/// Cancellation flag with an interruptible sleep (used by long-lived and long-running background work).
#[derive(Default)]
pub struct Stop {
    flag: std::sync::atomic::AtomicBool,
    lock: std::sync::Mutex<()>,
    cv: std::sync::Condvar,
}

impl Stop {
    pub fn trigger(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }
    pub fn is_set(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Sleep up to `d`; returns true as soon as the stop flag is set.
    pub fn sleep(&self, d: std::time::Duration) -> bool {
        let g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_set() {
            return true;
        }
        let _ = self.cv.wait_timeout_while(g, d, |_| !self.is_set());
        self.is_set()
    }
}

/// Process-wide shutdown token: set when the application exits so background workers can stop early.
pub fn shutdown() -> &'static Stop {
    static S: std::sync::OnceLock<Stop> = std::sync::OnceLock::new();
    S.get_or_init(Stop::default)
}

/// Shared Tokio runtime used to drive zbus (D-Bus) calls from worker threads.
pub fn rt() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).thread_name("dbus-rt").enable_all().build().expect("tokio runtime")
    })
}

/// Drive a future to completion from a plain (non-async) thread. Every D-Bus call goes through here:
/// worker threads block on the single shared runtime, and UI code never calls it. If it is ever reached
/// from inside the runtime (a programming error) the worker is moved off the executor first instead of panicking.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| rt().block_on(f))
    } else {
        rt().block_on(f)
    }
}

pub async fn name_has_owner(c: &zbus::Connection, name: &str) -> bool {
    match zbus::fdo::DBusProxy::new(c).await {
        Ok(p) => match zbus::names::BusName::try_from(name) {
            Ok(n) => p.name_has_owner(n).await.unwrap_or(false),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

pub fn detect_services() -> Services {
    let mut s = Services {
        pkexec: which("pkexec").is_some(),
        gsettings: which("gsettings").is_some(),
        smartctl: which("smartctl").is_some() || std::path::Path::new("/usr/sbin/smartctl").exists(),
        ..Default::default()
    };
    block_on(async {
        if let Ok(c) = zbus::Connection::system().await {
            s.network_manager = name_has_owner(&c, "org.freedesktop.NetworkManager").await;
            s.resolved = name_has_owner(&c, "org.freedesktop.resolve1").await;
            s.polkit = name_has_owner(&c, "org.freedesktop.PolicyKit1").await;
        }
        if let Ok(c) = zbus::Connection::session().await {
            s.secret_service = name_has_owner(&c, "org.freedesktop.secrets").await;
        }
    });
    s
}

#[cfg(feature = "gui")]
/// Run `f` on a worker thread and wake the UI when it finishes.
pub fn spawn<T: Send + 'static>(
    ctx: &eframe::egui::Context,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::sync::mpsc::Receiver<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("worker".into())
        .spawn(move || {
            // A panic ends only this worker; the receiver sees a disconnect (callers clear their busy state).
            if let Ok(v) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                let _ = tx.send(v);
            }
            ctx.request_repaint();
        })
        .expect("spawn worker");
    rx
}

// ---------- time ----------

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_millis() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// UTC "YYYY-MM-DD HH:MM:SS" (civil-from-days algorithm, no external crate).
pub fn fmt_ts(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Directory for configuration backups (mode 0700).
pub fn backup_dir(kind: &str) -> PathBuf {
    let d = app_dir().join("backups").join(kind);
    let _ = fs::create_dir_all(&d);
    d
}

/// List `*.json` backups, newest first, as (path, display name).
pub fn list_backups(kind: &str) -> Vec<(PathBuf, String)> {
    let mut v: Vec<PathBuf> = fs::read_dir(backup_dir(kind)).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|e| e == "json").unwrap_or(false)).collect()).unwrap_or_default();
    v.sort();
    v.reverse();
    v.into_iter().map(|p| { let n = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(); (p, n) }).collect()
}
