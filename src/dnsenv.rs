//! Which component owns DNS on this machine, whether a change would persist, and what conflicts.
//!
//! Detection only reads: `/etc/resolv.conf` (never written by this program), NetworkManager's
//! configuration files, and `systemctl is-active`. Parsing is separated from I/O so it is unit-tested
//! with fixtures for each layout.
use crate::linux::{read_trim, run, which, Services};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvConf {
    /// symlink to systemd-resolved's stub (127.0.0.53): applications go through resolved.
    ResolvedStub,
    /// symlink to resolved's uplink list: applications bypass the stub and talk to upstream servers directly.
    ResolvedUplink,
    /// symlink into /run/NetworkManager
    NetworkManager,
    /// regular file (hand-edited, or written by dhclient/resolvconf/another tool)
    StaticFile,
    OtherSymlink,
    Missing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NmDns {
    Default,
    SystemdResolved,
    Dnsmasq,
    None,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Manager {
    NetworkManager,
    SystemdNetworkd,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Persistence {
    /// Stored in NetworkManager's connection profile and applied again at every connection.
    Persistent(&'static str),
    /// Applied now through the resolver's D-Bus API; lost when the link or the service restarts.
    RuntimeOnly(&'static str),
    Unsupported(String),
}

#[derive(Clone, Debug)]
pub struct DnsEnv {
    pub resolved_installed: bool,
    pub resolved_running: bool,
    pub nm_running: bool,
    pub networkd_running: bool,
    pub resolv_conf: ResolvConf,
    pub resolv_nameservers: Vec<String>,
    pub nm_dns: NmDns,
    pub manager: Manager,
    pub conflicts: Vec<String>,
    pub persistence: Persistence,
}

pub fn classify_resolv_conf(link_target: Option<&str>, exists: bool) -> ResolvConf {
    match link_target {
        Some(t) if t.ends_with("stub-resolv.conf") => ResolvConf::ResolvedStub,
        Some(t) if t.contains("systemd/resolve/") => ResolvConf::ResolvedUplink,
        Some(t) if t.contains("NetworkManager") => ResolvConf::NetworkManager,
        Some(_) => ResolvConf::OtherSymlink,
        None if exists => ResolvConf::StaticFile,
        None => ResolvConf::Missing,
    }
}

pub fn parse_nameservers(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.trim().strip_prefix("nameserver").map(|r| r.trim().to_string())).filter(|s| !s.is_empty()).collect()
}

/// Last `dns=` under `[main]` wins (conf.d files override NetworkManager.conf, in file-name order).
pub fn parse_nm_dns(files: &[String]) -> NmDns {
    let mut mode = NmDns::Default;
    for text in files {
        let mut in_main = false;
        for l in text.lines() {
            let l = l.trim();
            if l.starts_with('[') {
                in_main = l == "[main]";
            } else if in_main && !l.starts_with('#') && !l.starts_with(';') {
                if let Some(v) = l.strip_prefix("dns").and_then(|r| r.trim_start().strip_prefix('=')) {
                    mode = match v.trim() {
                        "default" => NmDns::Default,
                        "systemd-resolved" => NmDns::SystemdResolved,
                        "dnsmasq" => NmDns::Dnsmasq,
                        "none" => NmDns::None,
                        _ => NmDns::Unknown,
                    };
                }
            }
        }
    }
    mode
}

fn nm_conf_files() -> Vec<String> {
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let mut names: Vec<std::path::PathBuf> = vec![];
    for d in ["/usr/lib/NetworkManager/conf.d", "/etc/NetworkManager/conf.d"] {
        if let Ok(rd) = std::fs::read_dir(d) {
            names.extend(rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|e| e == "conf").unwrap_or(false)));
        }
    }
    names.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
    let mut all = vec![];
    if let Some(t) = read(Path::new("/etc/NetworkManager/NetworkManager.conf")) {
        all.push(t);
    }
    all.extend(names.iter().filter_map(|p| read(p)));
    all
}

pub fn evaluate(
    resolved_installed: bool,
    resolved_running: bool,
    nm_running: bool,
    networkd_running: bool,
    resolv_conf: ResolvConf,
    resolv_nameservers: Vec<String>,
    nm_dns: NmDns,
) -> DnsEnv {
    let manager = if nm_running { Manager::NetworkManager } else if networkd_running { Manager::SystemdNetworkd } else { Manager::None };
    let mut c: Vec<String> = vec![];
    if nm_running && networkd_running {
        c.push("NetworkManager and systemd-networkd are both running: only one of them should manage a given interface, otherwise DNS settings can be overwritten by the other. Disable one (usually `systemctl disable --now systemd-networkd`) or mark interfaces unmanaged in one of them.".into());
    }
    if resolved_running && resolv_conf == ResolvConf::StaticFile {
        c.push("systemd-resolved is running but /etc/resolv.conf is a plain file: applications ignore resolved, so per-link DNS set through resolved has no effect for them. The supported fix is a symlink to /run/systemd/resolve/stub-resolv.conf (done by your distribution or an administrator, not by this program).".into());
    }
    if resolved_running && resolv_conf == ResolvConf::NetworkManager {
        c.push("systemd-resolved is running but /etc/resolv.conf is managed by NetworkManager: applications bypass resolved.".into());
    }
    if resolved_running && resolv_conf == ResolvConf::ResolvedUplink {
        c.push("/etc/resolv.conf points at resolved's uplink list (resolv.conf), not the stub: applications query the upstream servers directly and skip resolved's routing and caching.".into());
    }
    if !resolved_running && (resolv_conf == ResolvConf::ResolvedStub || resolv_nameservers.iter().any(|n| n == "127.0.0.53")) {
        c.push(if resolved_installed {
            "/etc/resolv.conf points at the resolved stub (127.0.0.53) but systemd-resolved is not running: name resolution is broken. Start it with `systemctl enable --now systemd-resolved`.".into()
        } else {
            "/etc/resolv.conf points at 127.0.0.53 but systemd-resolved is not installed: name resolution is broken.".into()
        });
    }
    if nm_running && resolved_running && nm_dns == NmDns::Default && resolv_conf != ResolvConf::NetworkManager {
        c.push("NetworkManager has dns=default (writes its own resolv.conf) while systemd-resolved is running: they do not share server lists. Set `dns=systemd-resolved` in /etc/NetworkManager/conf.d/ so NetworkManager pushes its DNS to resolved.".into());
    }
    if nm_running && !resolved_running && nm_dns == NmDns::SystemdResolved {
        c.push("NetworkManager is configured with dns=systemd-resolved but systemd-resolved is not running: its DNS servers are not applied anywhere.".into());
    }
    if nm_running && nm_dns == NmDns::None {
        c.push("NetworkManager has dns=none: it does not publish DNS servers at all, so profile changes will not become the system resolver configuration.".into());
    }
    let persistence = match manager {
        Manager::NetworkManager if nm_dns == NmDns::None => Persistence::Unsupported("NetworkManager is configured with dns=none, so connection DNS settings are stored but not used".into()),
        Manager::NetworkManager => Persistence::Persistent("saved in the NetworkManager connection profile"),
        Manager::SystemdNetworkd | Manager::None if resolved_running => Persistence::RuntimeOnly("systemd-resolved's D-Bus API (SetLinkDNS) is runtime-only; for a permanent setting an administrator must add DNS= to the interface's /etc/systemd/network/*.network file (systemd-networkd) or a /etc/systemd/resolved.conf.d/*.conf drop-in (global), which this program does not write"),
        _ => Persistence::Unsupported("neither NetworkManager nor systemd-resolved is running on the system bus, so there is no supported interface to change DNS".into()),
    };
    DnsEnv { resolved_installed, resolved_running, nm_running, networkd_running, resolv_conf, resolv_nameservers, nm_dns, manager, conflicts: c, persistence }
}

fn unit_active(unit: &str) -> bool {
    which("systemctl").is_some() && run("systemctl", &["is-active", unit]).map(|s| s == "active").unwrap_or(false)
}

pub fn detect(s: &Services) -> DnsEnv {
    let resolved_installed = ["/usr/lib/systemd/systemd-resolved", "/lib/systemd/systemd-resolved", "/usr/libexec/systemd/systemd-resolved"].iter().any(|p| Path::new(p).exists()) || which("resolvectl").is_some();
    let link = std::fs::read_link("/etc/resolv.conf").ok().map(|p| p.to_string_lossy().into_owned());
    let rc = classify_resolv_conf(link.as_deref(), Path::new("/etc/resolv.conf").exists());
    let ns = parse_nameservers(&read_trim("/etc/resolv.conf").unwrap_or_default());
    let networkd = unit_active("systemd-networkd");
    evaluate(resolved_installed, s.resolved, s.network_manager, networkd, rc, ns, parse_nm_dns(&nm_conf_files()))
}

impl DnsEnv {
    pub fn describe(&self) -> Vec<String> {
        let yn = |b| if b { "running" } else { "not running" };
        let mut o = vec![
            format!("systemd-resolved: {}{}", if self.resolved_installed { "installed, " } else { "not installed, " }, yn(self.resolved_running)),
            format!("NetworkManager: {}   (dns= {:?})", yn(self.nm_running), self.nm_dns),
            format!("systemd-networkd: {}", yn(self.networkd_running)),
            format!("/etc/resolv.conf: {:?} {}", self.resolv_conf, if self.resolv_nameservers.is_empty() { String::new() } else { format!("(nameserver {})", self.resolv_nameservers.join(", ")) }),
            format!("Active network manager: {:?}", self.manager),
        ];
        o.push(match &self.persistence {
            Persistence::Persistent(w) => format!("Changes are PERSISTENT: {w}"),
            Persistence::RuntimeOnly(w) => format!("Changes are RUNTIME-ONLY: {w}"),
            Persistence::Unsupported(w) => format!("Changing DNS is NOT SUPPORTED here: {w}"),
        });
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf_layouts() {
        assert_eq!(classify_resolv_conf(Some("../run/systemd/resolve/stub-resolv.conf"), true), ResolvConf::ResolvedStub);
        assert_eq!(classify_resolv_conf(Some("/run/systemd/resolve/resolv.conf"), true), ResolvConf::ResolvedUplink);
        assert_eq!(classify_resolv_conf(Some("/run/NetworkManager/resolv.conf"), true), ResolvConf::NetworkManager);
        assert_eq!(classify_resolv_conf(None, true), ResolvConf::StaticFile);
        assert_eq!(classify_resolv_conf(None, false), ResolvConf::Missing);
        assert_eq!(parse_nameservers("# c\nsearch lan\nnameserver 127.0.0.53\nnameserver  1.1.1.1 \n"), vec!["127.0.0.53", "1.1.1.1"]);
    }

    #[test]
    fn nm_dns_mode_last_definition_wins() {
        assert_eq!(parse_nm_dns(&[]), NmDns::Default);
        assert_eq!(parse_nm_dns(&["[main]\nplugins=keyfile\ndns=dnsmasq\n".into()]), NmDns::Dnsmasq);
        assert_eq!(parse_nm_dns(&["[main]\ndns=dnsmasq".into(), "[main]\ndns=systemd-resolved".into()]), NmDns::SystemdResolved);
        assert_eq!(parse_nm_dns(&["[keyfile]\ndns=none\n".into()]), NmDns::Default, "other sections ignored");
        assert_eq!(parse_nm_dns(&["[main]\n#dns=none\n".into()]), NmDns::Default);
    }

    #[test]
    fn no_resolved_no_networkmanager() {
        let e = evaluate(false, false, false, false, ResolvConf::StaticFile, vec!["192.168.1.1".into()], NmDns::Default);
        assert!(matches!(e.persistence, Persistence::Unsupported(_)));
        assert!(e.conflicts.is_empty());
        assert_eq!(e.manager, Manager::None);
    }

    #[test]
    fn networkmanager_is_persistent() {
        let e = evaluate(true, true, true, false, ResolvConf::ResolvedStub, vec!["127.0.0.53".into()], NmDns::SystemdResolved);
        assert_eq!(e.persistence, Persistence::Persistent("saved in the NetworkManager connection profile"));
        assert!(e.conflicts.is_empty(), "{:?}", e.conflicts);
        let none = evaluate(false, false, true, false, ResolvConf::NetworkManager, vec![], NmDns::None);
        assert!(matches!(none.persistence, Persistence::Unsupported(_)) && !none.conflicts.is_empty());
    }

    #[test]
    fn resolved_only_is_runtime() {
        let e = evaluate(true, true, false, true, ResolvConf::ResolvedStub, vec!["127.0.0.53".into()], NmDns::Default);
        assert!(matches!(e.persistence, Persistence::RuntimeOnly(w) if w.contains("resolved.conf.d")));
        assert_eq!(e.manager, Manager::SystemdNetworkd);
    }

    #[test]
    fn conflicts_are_reported() {
        let both = evaluate(true, true, true, true, ResolvConf::ResolvedStub, vec![], NmDns::SystemdResolved);
        assert!(both.conflicts.iter().any(|c| c.contains("both running")));
        let plain = evaluate(true, true, false, false, ResolvConf::StaticFile, vec![], NmDns::Default);
        assert!(plain.conflicts.iter().any(|c| c.contains("plain file")));
        let dead = evaluate(true, false, false, false, ResolvConf::ResolvedStub, vec!["127.0.0.53".into()], NmDns::Default);
        assert!(dead.conflicts.iter().any(|c| c.contains("enable --now systemd-resolved")));
        let nm_default = evaluate(true, true, true, false, ResolvConf::ResolvedStub, vec![], NmDns::Default);
        assert!(nm_default.conflicts.iter().any(|c| c.contains("dns=systemd-resolved")));
    }

    /// The program must never write /etc/resolv.conf itself.
    #[test]
    fn resolv_conf_is_never_written() {
        for (name, src) in [("dns.rs", include_str!("dns.rs")), ("dnsenv.rs", include_str!("dnsenv.rs")), ("proxy.rs", include_str!("proxy.rs"))] {
            for l in src.lines().filter(|l| l.contains("/etc/resolv.conf") && !l.trim_start().starts_with("//") && !l.contains("assert") && !l.contains("include_str")) {
                assert!(!(l.contains("write") || l.contains("remove_file") || l.contains("rename") || l.contains("set_permissions")), "{name}: {l}");
            }
        }
    }
}
