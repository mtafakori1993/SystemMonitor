//! DNS manager: NetworkManager / systemd-resolved over D-Bus, UDP resolver tests, backup & rollback.
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
use std::collections::HashMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
#[cfg(feature = "gui")]
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

type Settings_ = HashMap<String, HashMap<String, OwnedVariant>>;
const NM: &str = "org.freedesktop.NetworkManager";

// ---------------------------------------------------------------- profiles

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct DnsProfile {
    pub name: String,
    pub builtin: bool,
    pub auto: bool,
    pub v4: Vec<String>,
    pub v6: Vec<String>,
}

fn presets() -> Vec<DnsProfile> {
    let p = |n: &str, a: &[&str], b: &[&str]| DnsProfile { name: n.into(), builtin: true, auto: false, v4: a.iter().map(|s| s.to_string()).collect(), v6: b.iter().map(|s| s.to_string()).collect() };
    vec![
        DnsProfile { name: "Automatic (DHCP)".into(), builtin: true, auto: true, v4: vec![], v6: vec![] },
        p("Cloudflare", &["1.1.1.1", "1.0.0.1"], &["2606:4700:4700::1111", "2606:4700:4700::1001"]),
        p("Google", &["8.8.8.8", "8.8.4.4"], &["2001:4860:4860::8888", "2001:4860:4860::8844"]),
        p("Quad9", &["9.9.9.9", "149.112.112.112"], &["2620:fe::fe", "2620:fe::9"]),
    ]
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct CustomStore {
    #[serde(deserialize_with = "crate::store::lenient_vec")]
    profiles: Vec<DnsProfile>,
}

/// v0 was a bare JSON array of profiles; v1 wraps it so the document can carry a schema version.
fn migrate_custom(v: serde_json::Value, from: u32) -> Result<serde_json::Value, String> {
    match (from, v) {
        (0, serde_json::Value::Array(a)) => Ok(serde_json::json!({ "profiles": a })),
        (_, v) => Ok(v),
    }
}

fn validate_custom(s: &mut CustomStore) -> Vec<String> {
    let mut notes = vec![];
    s.profiles.retain(|p| match validate(p) {
        Ok(()) if !p.builtin => true,
        Ok(()) => false,
        Err(e) => {
            notes.push(format!("DNS profile '{}' ignored: {e}", p.name));
            false
        }
    });
    notes
}

const CUSTOM_SPEC: crate::store::Spec<CustomStore> = crate::store::Spec { file: "dns-profiles.json", version: 1, migrate: migrate_custom, validate: validate_custom };

fn load_custom() -> Vec<DnsProfile> {
    let l = crate::store::load(&app_dir(), &CUSTOM_SPEC);
    for n in &l.notes {
        eprintln!("dns profiles: {n}");
    }
    l.value.profiles
}

fn save_custom(p: &[DnsProfile]) {
    let s = CustomStore { profiles: p.iter().filter(|p| !p.builtin).cloned().collect() };
    if let Err(e) = crate::store::save(&app_dir(), &CUSTOM_SPEC, &s) {
        eprintln!("dns profiles: {e}");
    }
}

pub fn validate(p: &DnsProfile) -> Result<(), String> {
    if !valid_profile_name(&p.name) {
        return Err("Profile name must be 1-48 printable characters".into());
    }
    if p.auto {
        return Ok(());
    }
    if p.v4.is_empty() && p.v6.is_empty() {
        return Err("Enter at least one DNS server".into());
    }
    for s in &p.v4 {
        match parse_dns_ip(s)? {
            IpAddr::V4(_) => {}
            _ => return Err(format!("'{s}' is not an IPv4 address")),
        }
    }
    for s in &p.v6 {
        match parse_dns_ip(s)? {
            IpAddr::V6(_) => {}
            _ => return Err(format!("'{s}' is not an IPv6 address")),
        }
    }
    if p.v4.len() > 3 || p.v6.len() > 3 {
        return Err("At most 3 servers per address family are used by the resolver".into());
    }
    Ok(())
}

fn split_list(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|x| !x.is_empty()).map(String::from).collect()
}

// ---------------------------------------------------------------- resolver tests (raw UDP)

fn build_query(id: u16, name: &str, aaaa: bool) -> Option<Vec<u8>> {
    let mut q = vec![(id >> 8) as u8, id as u8, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend(label.bytes());
    }
    q.extend([0, 0, if aaaa { 28 } else { 1 }, 0, 1]);
    Some(q)
}

fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *b.get(i)? as usize;
        if l == 0 {
            return Some(i + 1);
        }
        if l & 0xC0 == 0xC0 {
            return Some(i + 2);
        }
        i += 1 + l;
    }
}

pub struct DnsAnswer {
    pub rtt: Duration,
    pub addrs: Vec<IpAddr>,
    pub rcode: u8,
}

pub fn query(server: IpAddr, name: &str, aaaa: bool, timeout: Duration) -> Result<DnsAnswer, String> {
    let id = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(7) & 0xFFFF) as u16;
    let q = build_query(id, name, aaaa).ok_or("invalid domain name")?;
    let sock = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(timeout)).ok();
    sock.connect((server, 53)).map_err(|e| e.to_string())?;
    let t = Instant::now();
    sock.send(&q).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 1500];
    let n = loop {
        let n = sock.recv(&mut buf).map_err(|e| if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut { "timeout".to_string() } else { e.to_string() })?;
        if n >= 12 && buf[0] == q[0] && buf[1] == q[1] {
            break n;
        }
    };
    let rtt = t.elapsed();
    let b = &buf[..n];
    let rcode = b[3] & 0x0F;
    let (qd, an) = (u16::from_be_bytes([b[4], b[5]]) as usize, u16::from_be_bytes([b[6], b[7]]) as usize);
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i).ok_or("malformed response")? + 4;
    }
    let mut addrs = vec![];
    for _ in 0..an {
        i = skip_name(b, i).ok_or("malformed response")?;
        if i + 10 > b.len() {
            break;
        }
        let (ty, rl) = (u16::from_be_bytes([b[i], b[i + 1]]), u16::from_be_bytes([b[i + 8], b[i + 9]]) as usize);
        i += 10;
        if i + rl > b.len() {
            break;
        }
        match (ty, rl) {
            (1, 4) => addrs.push(IpAddr::V4(Ipv4Addr::new(b[i], b[i + 1], b[i + 2], b[i + 3]))),
            (28, 16) => addrs.push(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&b[i..i + 16]).unwrap()))),
            _ => {}
        }
        i += rl;
    }
    Ok(DnsAnswer { rtt, addrs, rcode })
}

#[derive(Clone)]
pub struct BenchRow {
    pub label: String,
    pub server: String,
    pub avg_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub ok: usize,
    pub total: usize,
    pub sample: String,
}

fn bench(label: &str, server: &str, domain: &str, rounds: usize) -> BenchRow {
    let mut ms = vec![];
    let mut sample = String::new();
    let mut err = String::new();
    if let Ok(ip) = server.parse::<IpAddr>() {
        for _ in 0..rounds {
            if crate::linux::shutdown().is_set() {
                break; // application is closing: stop probing
            }
            match query(ip, domain, false, Duration::from_millis(1500)) {
                Ok(a) if a.rcode == 0 && !a.addrs.is_empty() => {
                    ms.push(a.rtt.as_secs_f64() * 1000.0);
                    sample = a.addrs.iter().take(2).map(|x| x.to_string()).collect::<Vec<_>>().join(", ");
                }
                Ok(a) => err = if a.rcode == 3 { "NXDOMAIN".into() } else { format!("rcode {}", a.rcode) },
                Err(e) => err = e,
            }
        }
    }
    BenchRow { label: label.into(), server: server.into(), avg_ms: (!ms.is_empty()).then(|| ms.iter().sum::<f64>() / ms.len() as f64), min_ms: ms.iter().cloned().reduce(f64::min), ok: ms.len(), total: rounds, sample: if sample.is_empty() { err } else { sample } }
}

fn system_resolve(domain: &str) -> Result<(Duration, Vec<IpAddr>), String> {
    use std::net::ToSocketAddrs;
    let t = Instant::now();
    let a: Vec<IpAddr> = (domain, 0u16).to_socket_addrs().map_err(|e| e.to_string())?.map(|s| s.ip()).collect();
    Ok((t.elapsed(), a))
}

// ---------------------------------------------------------------- NetworkManager

#[derive(Clone, Default, Debug)]
pub struct DevInfo {
    pub iface: String,
    pub ifindex: i32,
    pub dtype: u32,
    pub dev_path: String,
    pub conn_path: String,
    pub conn_id: String,
    pub v4_method: String,
    pub v6_method: String,
    pub cfg_v4: Vec<String>,
    pub cfg_v6: Vec<String>,
    pub ignore_v4: bool,
    pub ignore_v6: bool,
    pub effective: Vec<String>,
}

fn e(x: impl std::fmt::Display) -> String {
    x.to_string()
}

async fn nm_proxy<'a>(c: &'a Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>, String> {
    Proxy::new(c, NM, path, iface).await.map_err(e)
}

fn v4_from_u32(v: &[u32]) -> Vec<String> {
    v.iter().map(|x| Ipv4Addr::from(x.to_ne_bytes()).to_string()).collect()
}

fn read_family(s: &Settings_, fam: &str) -> (String, Vec<String>, bool) {
    let sec = s.get(fam);
    let method = sec.and_then(|m| m.get("method")).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
    let ignore = sec.and_then(|m| m.get("ignore-auto-dns")).and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
    let dns: Vec<String> = match (fam, sec.and_then(|m| m.get("dns"))) {
        ("ipv4", Some(v)) => v.try_clone().ok().and_then(|v| Vec::<u32>::try_from(v).ok()).map(|x| v4_from_u32(&x)).unwrap_or_default(),
        (_, Some(v)) => v.try_clone().ok().and_then(|v| Vec::<Vec<u8>>::try_from(v).ok()).map(|x| x.iter().filter_map(|b| <[u8; 16]>::try_from(b.as_slice()).ok()).map(|b| Ipv6Addr::from(b).to_string()).collect()).unwrap_or_default(),
        _ => vec![],
    };
    (method, dns, ignore)
}

pub async fn nm_list() -> Result<Vec<DevInfo>, String> {
    let c = Connection::system().await.map_err(e)?;
    let nm = nm_proxy(&c, "/org/freedesktop/NetworkManager", "org.freedesktop.NetworkManager").await?;
    let devs: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).await.map_err(e)?;
    let mut out = vec![];
    for d in devs {
        let dp = nm_proxy(&c, d.as_str(), "org.freedesktop.NetworkManager.Device").await?;
        let ac: OwnedObjectPath = match dp.get_property("ActiveConnection").await {
            Ok(a) => a,
            Err(_) => continue,
        };
        if ac.as_str() == "/" {
            continue;
        }
        let iface: String = dp.get_property("Interface").await.unwrap_or_default();
        let dtype: u32 = dp.get_property("DeviceType").await.unwrap_or(0);
        if iface == "lo" || dtype == 14 && iface.starts_with("lo") {
            continue;
        }
        let acp = nm_proxy(&c, ac.as_str(), "org.freedesktop.NetworkManager.Connection.Active").await?;
        let conn: OwnedObjectPath = acp.get_property("Connection").await.map_err(e)?;
        let id: String = acp.get_property("Id").await.unwrap_or_default();
        let cp = nm_proxy(&c, conn.as_str(), "org.freedesktop.NetworkManager.Settings.Connection").await?;
        let settings: Settings_ = match cp.call("GetSettings", &()).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let (m4, d4, i4) = read_family(&settings, "ipv4");
        let (m6, d6, i6) = read_family(&settings, "ipv6");
        let mut effective = vec![];
        for (prop, iface_name) in [("Ip4Config", "org.freedesktop.NetworkManager.IP4Config"), ("Ip6Config", "org.freedesktop.NetworkManager.IP6Config")] {
            if let Ok(path) = dp.get_property::<OwnedObjectPath>(prop).await {
                if path.as_str() != "/" {
                    let ip = nm_proxy(&c, path.as_str(), iface_name).await?;
                    if let Ok(data) = ip.get_property::<Vec<HashMap<String, OwnedVariant>>>("NameserverData").await {
                        effective.extend(data.iter().filter_map(|m| m.get("address").and_then(|v| String::try_from(v.try_clone().ok()?).ok())));
                    }
                }
            }
        }
        out.push(DevInfo {
            ifindex: read_trim(format!("/sys/class/net/{iface}/ifindex")).and_then(|s| s.parse().ok()).unwrap_or(0),
            dtype,
            iface,
            dev_path: d.to_string(),
            conn_path: conn.to_string(),
            conn_id: id,
            v4_method: m4,
            v6_method: m6,
            cfg_v4: d4,
            cfg_v6: d6,
            ignore_v4: i4,
            ignore_v6: i6,
            effective,
        });
    }
    Ok(out)
}

fn variant_owned(v: Variant<'_>) -> Result<OwnedVariant, String> {
    v.try_into_owned().map_err(e)
}

/// Write DNS fields for one address family; `None` = leave untouched, `Some(empty)` = clear (automatic).
fn set_family(s: &mut Settings_, fam: &str, servers: Option<(&[String], bool)>) -> Result<(), String> {
    let Some((list, ignore)) = servers else { return Ok(()) };
    let sec = s.entry(fam.to_string()).or_default();
    let method = sec.get("method").and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
    if matches!(method.as_str(), "disabled" | "ignore" | "link-local") || method.is_empty() {
        return Ok(()); // nothing to configure for this family
    }
    // Newer NetworkManager mirrors `dns` into `dns-data`; a stale copy would win, so drop it.
    sec.remove("dns-data");
    if list.is_empty() {
        sec.remove("dns");
    } else if fam == "ipv4" {
        let v: Vec<u32> = list.iter().filter_map(|s| s.parse::<Ipv4Addr>().ok()).map(|ip| u32::from_ne_bytes(ip.octets())).collect();
        sec.insert("dns".into(), variant_owned(Variant::new(v))?);
    } else {
        let v: Vec<Vec<u8>> = list.iter().filter_map(|s| s.parse::<Ipv6Addr>().ok()).map(|ip| ip.octets().to_vec()).collect();
        sec.insert("dns".into(), variant_owned(Variant::new(v))?);
    }
    sec.insert("ignore-auto-dns".into(), variant_owned(Variant::Bool(ignore))?);
    Ok(())
}

/// `expect`: servers that must show up as in use afterwards; if they do not, the connection is re-activated.
/// The result says whether the profile was saved to disk (persistent) or only exists in memory.
async fn nm_write(d: &DevInfo, v4: Option<(&[String], bool)>, v6: Option<(&[String], bool)>, expect: &[String]) -> Result<String, String> {
    let how = nm_write_inner(d, v4, v6, expect).await?;
    let saved = match Connection::system().await {
        Ok(c) => match nm_proxy(&c, &d.conn_path, "org.freedesktop.NetworkManager.Settings.Connection").await {
            Ok(cp) => match cp.get_property::<bool>("Unsaved").await {
                Ok(true) => "runtime only: the connection profile is not saved to disk",
                Ok(false) => "persistent: saved in the connection profile",
                Err(_) => "persistence unknown",
            },
            Err(_) => "persistence unknown",
        },
        Err(_) => "persistence unknown",
    };
    Ok(format!("{how}, {saved}"))
}

async fn nm_write_inner(d: &DevInfo, v4: Option<(&[String], bool)>, v6: Option<(&[String], bool)>, expect: &[String]) -> Result<String, String> {
    let c = Connection::system().await.map_err(e)?;
    let cp = nm_proxy(&c, &d.conn_path, "org.freedesktop.NetworkManager.Settings.Connection").await?;
    let mut s: Settings_ = cp.call("GetSettings", &()).await.map_err(e)?;
    set_family(&mut s, "ipv4", v4)?;
    set_family(&mut s, "ipv6", v6)?;
    cp.call::<_, _, ()>("Update", &(s,)).await.map_err(|er| format!("NetworkManager refused the update: {er}"))?;
    // Apply to the live connection without re-activation when possible.
    let dp = nm_proxy(&c, &d.dev_path, "org.freedesktop.NetworkManager.Device").await?;
    let empty: Settings_ = HashMap::new();
    let reapplied = dp.call::<_, _, ()>("Reapply", &(empty, 0u64, 0u32)).await.is_ok();
    if reapplied {
        if expect.is_empty() {
            return Ok("applied live".into());
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
        let mut inuse: Vec<String> = vec![];
        for (prop, iface_name) in [("Ip4Config", "org.freedesktop.NetworkManager.IP4Config"), ("Ip6Config", "org.freedesktop.NetworkManager.IP6Config")] {
            if let Ok(path) = dp.get_property::<OwnedObjectPath>(prop).await {
                if path.as_str() != "/" {
                    let ip = nm_proxy(&c, path.as_str(), iface_name).await?;
                    if let Ok(data) = ip.get_property::<Vec<HashMap<String, OwnedVariant>>>("NameserverData").await {
                        inuse.extend(data.iter().filter_map(|m| m.get("address").and_then(|v| String::try_from(v.try_clone().ok()?).ok())));
                    }
                }
            }
        }
        if expect.iter().any(|x| inuse.contains(x)) {
            return Ok("applied live".into());
        }
    }
    {
        {
            let nm = nm_proxy(&c, "/org/freedesktop/NetworkManager", "org.freedesktop.NetworkManager").await?;
            let conn = OwnedObjectPath::try_from(d.conn_path.as_str()).map_err(e)?;
            let dev = OwnedObjectPath::try_from(d.dev_path.as_str()).map_err(e)?;
            let none = OwnedObjectPath::try_from("/").map_err(e)?;
            nm.call::<_, _, OwnedObjectPath>("ActivateConnection", &(conn, dev, none)).await.map_err(|er| format!("re-activation failed: {er}"))?;
            Ok("connection re-activated".into())
        }
    }
}

/// Drop cached answers so the new servers are used immediately.
async fn flush_caches() -> String {
    let mut done = vec![];
    if let Ok(c) = Connection::system().await {
        if let Ok(p) = Proxy::new(&c, "org.freedesktop.resolve1", "/org/freedesktop/resolve1", "org.freedesktop.resolve1.Manager").await {
            if p.call::<_, _, ()>("FlushCaches", &()).await.is_ok() {
                done.push("systemd-resolved cache flushed");
            }
        }
    }
    if which("nscd").is_some() && run("nscd", &["-i", "hosts"]).is_ok() {
        done.push("nscd hosts cache flushed");
    }
    done.join(", ")
}

// ---------------------------------------------------------------- systemd-resolved

/// DNS servers systemd-resolved currently holds for one link (read through its D-Bus API, `Link.DNS`).
pub async fn resolved_link_dns(ifindex: i32) -> Result<Vec<String>, String> {
    let c = Connection::system().await.map_err(e)?;
    let p = Proxy::new(&c, "org.freedesktop.resolve1", "/org/freedesktop/resolve1", "org.freedesktop.resolve1.Manager").await.map_err(e)?;
    let path: OwnedObjectPath = p.call("GetLink", &(ifindex,)).await.map_err(e)?;
    let l = Proxy::new(&c, "org.freedesktop.resolve1", path.as_str(), "org.freedesktop.resolve1.Link").await.map_err(e)?;
    let dns: Vec<(i32, Vec<u8>)> = l.get_property("DNS").await.map_err(e)?;
    Ok(dns
        .into_iter()
        .filter_map(|(fam, b)| match (fam, b.len()) {
            (2, 4) => Some(IpAddr::from(<[u8; 4]>::try_from(b.as_slice()).ok()?).to_string()),
            (10, 16) => Some(IpAddr::from(<[u8; 16]>::try_from(b.as_slice()).ok()?).to_string()),
            _ => None,
        })
        .collect())
}

/// Same set of addresses regardless of order or textual form (`2001:db8::1` vs `2001:0db8::1`).
pub fn same_servers(a: &[String], b: &[String]) -> bool {
    let norm = |v: &[String]| -> Vec<IpAddr> {
        let mut x: Vec<IpAddr> = v.iter().filter_map(|s| s.parse().ok()).collect();
        x.sort();
        x
    };
    norm(a) == norm(b)
}

async fn resolved_set(ifindex: i32, servers: &[String]) -> Result<(), String> {
    let c = Connection::system().await.map_err(e)?;
    let p = Proxy::new(&c, "org.freedesktop.resolve1", "/org/freedesktop/resolve1", "org.freedesktop.resolve1.Manager").await.map_err(e)?;
    if servers.is_empty() {
        return p.call::<_, _, ()>("RevertLink", &(ifindex,)).await.map_err(e);
    }
    let addrs: Vec<(i32, Vec<u8>)> = servers
        .iter()
        .filter_map(|s| s.parse::<IpAddr>().ok())
        .map(|ip| match ip {
            IpAddr::V4(a) => (2, a.octets().to_vec()),
            IpAddr::V6(a) => (10, a.octets().to_vec()),
        })
        .collect();
    p.call::<_, _, ()>("SetLinkDNS", &(ifindex, addrs)).await.map_err(|er| format!("systemd-resolved refused SetLinkDNS (polkit may require authentication): {er}"))?;
    let _ = p.call::<_, _, ()>("SetLinkDefaultRoute", &(ifindex, true)).await;
    // Verify that resolved really holds what was asked; undo the link if not.
    let got = resolved_link_dns(ifindex).await.unwrap_or_default();
    if !same_servers(&got, servers) {
        let _ = p.call::<_, _, ()>("RevertLink", &(ifindex,)).await;
        return Err(format!("systemd-resolved reports {got:?} for the link instead of {servers:?}; the link was reverted"));
    }
    Ok(())
}

// ---------------------------------------------------------------- backup + apply + rollback

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Debug)]
pub enum Backend {
    NetworkManager,
    Resolved,
}

#[derive(Serialize, Deserialize)]
struct DnsBackup {
    label: String,
    ts: u64,
    backend: Backend,
    iface: String,
    ifindex: i32,
    dev_path: String,
    conn_path: String,
    conn_id: String,
    v4: Vec<String>,
    v6: Vec<String>,
    ignore_v4: bool,
    ignore_v6: bool,
    resolv_conf: String,
    #[serde(default)]
    group: u128,
}

fn backup(backend: Backend, d: &DevInfo, label: &str, group: u128) -> Result<(), String> {
    let b = DnsBackup {
        label: label.into(),
        ts: now_secs(),
        backend,
        iface: d.iface.clone(),
        ifindex: d.ifindex,
        dev_path: d.dev_path.clone(),
        conn_path: d.conn_path.clone(),
        conn_id: d.conn_id.clone(),
        v4: d.cfg_v4.clone(),
        v6: d.cfg_v6.clone(),
        ignore_v4: d.ignore_v4,
        ignore_v6: d.ignore_v6,
        resolv_conf: fs::read_to_string("/etc/resolv.conf").unwrap_or_default(),
        group,
    };
    let name = format!("{}-{}", now_millis(), d.iface);
    write_atomic(&backup_dir("dns").join(format!("{name}.json")), &serde_json::to_string_pretty(&b).map_err(e)?, 0o600)
}

fn restore_one(b: &DnsBackup) -> Result<String, String> {
    let d = DevInfo { iface: b.iface.clone(), ifindex: b.ifindex, dev_path: b.dev_path.clone(), conn_path: b.conn_path.clone(), conn_id: b.conn_id.clone(), ..Default::default() };
    match b.backend {
        Backend::NetworkManager => {
            let r = block_on(nm_write(&d, Some((&b.v4, b.ignore_v4)), Some((&b.v6, b.ignore_v6)), &[]))?;
            Ok(format!("'{}' ({}): {r}", b.conn_id, b.iface))
        }
        Backend::Resolved => {
            let all: Vec<String> = b.v4.iter().chain(b.v6.iter()).cloned().collect();
            block_on(resolved_set(b.ifindex, &all))?;
            Ok(format!("{}: resolver settings restored", b.iface))
        }
    }
}

/// Restores a backup; a backup that belongs to a group (one switch over several connections) restores the whole group.
fn restore(path: &std::path::Path) -> Result<String, String> {
    let b: DnsBackup = serde_json::from_str(&fs::read_to_string(path).map_err(e)?).map_err(|x| format!("corrupt backup: {x}"))?;
    let mut all = vec![b];
    if all[0].group != 0 {
        let g = all[0].group;
        all = list_backups("dns").iter().filter_map(|(p, _)| serde_json::from_str::<DnsBackup>(&fs::read_to_string(p).ok()?).ok()).filter(|x| x.group == g).collect();
    }
    let mut out = vec![];
    for b in &all {
        out.push(restore_one(b)?);
    }
    let fl = block_on(flush_caches());
    Ok(format!("Restored DNS settings: {}{}", out.join("; "), if fl.is_empty() { String::new() } else { format!(" ({fl})") }))
}

fn restore_group(group: u128) {
    for (p, _) in list_backups("dns") {
        if let Some(b) = fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<DnsBackup>(&s).ok()) {
            if b.group == group {
                let _ = restore_one(&b);
            }
        }
    }
}

fn apply(backend: Backend, devs: &[DevInfo], p: &DnsProfile) -> Result<String, String> {
    validate(p)?;
    if devs.is_empty() {
        return Err("No network connection to configure".into());
    }
    let group = now_millis();
    for d in devs {
        backup(backend, d, &format!("Before switching {} to '{}'", d.iface, p.name), group)?;
    }
    let expect: Vec<String> = if p.auto { vec![] } else { p.v4.iter().chain(p.v6.iter()).cloned().collect() };
    let mut how = vec![];
    for d in devs {
        let res = match backend {
            Backend::NetworkManager => {
                let (v4, v6): (Option<(&[String], bool)>, Option<(&[String], bool)>) = if p.auto { (Some((&[], false)), Some((&[], false))) } else { ((!p.v4.is_empty()).then_some((&p.v4[..], true)), (!p.v6.is_empty()).then_some((&p.v6[..], true))) };
                block_on(nm_write(d, v4, v6, &expect))
            }
            Backend::Resolved => block_on(resolved_set(d.ifindex, &expect)).map(|_| "verified in systemd-resolved, runtime only (not persistent)".to_string()),
        };
        match res {
            Ok(h) => how.push(format!("{}: {h}", d.iface)),
            Err(err) => {
                restore_group(group);
                return Err(format!("{}: {err} (previous configuration restored)", d.iface));
            }
        }
    }
    // Verify: if custom servers cannot answer at all, roll back automatically.
    if !p.auto {
        std::thread::sleep(Duration::from_millis(800));
        let ok = expect.iter().filter_map(|s| s.parse::<IpAddr>().ok()).any(|ip| query(ip, "example.com", false, Duration::from_millis(2000)).map(|a| a.rcode == 0).unwrap_or(false));
        if !ok {
            restore_group(group);
            return Err(format!("None of the DNS servers of '{}' answered a test query; the previous configuration was restored automatically", p.name));
        }
    }
    let fl = block_on(flush_caches());
    Ok(format!("DNS profile '{}' active now, no reconnect needed [{}]{}", p.name, how.join("; "), if fl.is_empty() { String::new() } else { format!(" {fl}.") }))
}

// ---------------------------------------------------------------- UI

#[cfg(feature = "gui")]
enum Msg {
    Devices(Result<Vec<DevInfo>, String>),
    Applied(String, Result<String, String>),
    Restored(Result<String, String>),
    Bench(Vec<BenchRow>),
    Sys(String, Result<(Duration, Vec<IpAddr>), String>),
}

#[cfg(feature = "gui")]
pub struct DnsUi {
    env: Option<crate::dnsenv::DnsEnv>,
    services: Services,
    backend: Backend,
    devices: Vec<DevInfo>,
    dev_err: Option<String>,
    sel_dev: usize,
    all_conns: bool,
    profiles: Vec<DnsProfile>,
    selected: usize,
    edit_name: String,
    edit_v4: String,
    edit_v6: String,
    rx: Vec<Receiver<Msg>>,
    busy: bool,
    bench: Vec<BenchRow>,
    bench_busy: bool,
    domain: String,
    sys_result: Option<String>,
    backups: Vec<(std::path::PathBuf, String)>,
    confirm_restore: Option<std::path::PathBuf>,
    error: Option<String>,
}

#[cfg(feature = "gui")]
impl DnsUi {
    pub fn new(settings: &Settings, _ctx: &egui::Context) -> Self {
        let mut profiles = presets();
        profiles.extend(load_custom());
        let selected = profiles.iter().position(|p| p.name == settings.active_dns).unwrap_or(0);
        let mut s = Self { env: None, services: Services::default(), backend: Backend::NetworkManager, devices: vec![], dev_err: None, sel_dev: 0, all_conns: true, profiles, selected, edit_name: String::new(), edit_v4: String::new(), edit_v6: String::new(), rx: vec![], busy: false, bench: vec![], bench_busy: false, domain: "example.com".into(), sys_result: None, backups: list_backups("dns"), confirm_restore: None, error: None };
        s.load_editor();
        s
    }

    fn load_editor(&mut self) {
        let p = &self.profiles[self.selected];
        self.edit_name = p.name.clone();
        self.edit_v4 = p.v4.join(", ");
        self.edit_v6 = p.v6.join(", ");
    }

    pub fn on_services(&mut self, s: &Services, ctx: &egui::Context) {
        self.services = s.clone();
        self.backend = if s.network_manager { Backend::NetworkManager } else { Backend::Resolved };
        self.refresh(ctx);
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        self.backups = list_backups("dns");
        self.env = Some(crate::dnsenv::detect(&self.services));
        let (nm, resolved) = (self.services.network_manager, self.services.resolved);
        self.rx.push(spawn(ctx, move || {
            if nm {
                Msg::Devices(block_on(nm_list()))
            } else if resolved {
                // no NetworkManager: offer links that are up
                let mut v = vec![];
                for ent in fs::read_dir("/sys/class/net").into_iter().flatten().flatten() {
                    let n = ent.file_name().to_string_lossy().into_owned();
                    if n != "lo" && read_trim(format!("/sys/class/net/{n}/operstate")).as_deref() == Some("up") {
                        v.push(DevInfo { ifindex: read_trim(format!("/sys/class/net/{n}/ifindex")).and_then(|s| s.parse().ok()).unwrap_or(0), iface: n, ..Default::default() });
                    }
                }
                Msg::Devices(Ok(v))
            } else {
                Msg::Devices(Err("Neither NetworkManager nor systemd-resolved is running on the system bus.".into()))
            }
        }));
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
            self.bench_busy = false;
            self.error = Some("A background task ended unexpectedly; please try again.".into());
        }
        for m in msgs {
            match m {
                Msg::Devices(Ok(d)) => {
                    self.devices = d;
                    self.dev_err = None;
                    self.sel_dev = self.sel_dev.min(self.devices.len().saturating_sub(1));
                }
                Msg::Devices(Err(er)) => {
                    self.devices.clear();
                    self.dev_err = Some(er);
                }
                Msg::Applied(name, r) => {
                    self.busy = false;
                    match r {
                        Ok(s) => {
                            settings.active_dns = name;
                            settings.save();
                            n.ok(s);
                        }
                        Err(er) => n.err(format!("DNS change failed: {er}")),
                    }
                    self.backups = list_backups("dns");
                }
                Msg::Restored(r) => {
                    self.busy = false;
                    match r {
                        Ok(s) => {
                            settings.active_dns = "Automatic (DHCP)".into();
                            n.ok(s)
                        }
                        Err(er) => n.err(format!("Restore failed: {er}")),
                    }
                }
                Msg::Bench(b) => {
                    self.bench = b;
                    self.bench_busy = false;
                }
                Msg::Sys(d, r) => {
                    self.sys_result = Some(match r {
                        Ok((t, a)) => format!("{d}: {} in {:.1} ms", a.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "), t.as_secs_f64() * 1000.0),
                        Err(er) => format!("{d}: {er}"),
                    });
                }
            }
        }
    }

    fn start_apply(&mut self, ctx: &egui::Context, profile_idx: usize) {
        let devs: Vec<DevInfo> = if self.all_conns {
            self.devices.iter().filter(|d| d.dtype == 0 || [1, 2, 8, 11, 16, 29].contains(&d.dtype)).cloned().collect()
        } else {
            self.devices.get(self.sel_dev).cloned().into_iter().collect()
        };
        if devs.is_empty() {
            self.error = Some("No active network connection to configure".into());
            return;
        }
        let p = self.profiles[profile_idx].clone();
        if let Err(er) = validate(&p) {
            self.error = Some(er);
            return;
        }
        self.error = None;
        self.busy = true;
        let backend = self.backend;
        self.rx.push(spawn(ctx, move || {
            let name = p.name.clone();
            Msg::Applied(name, apply(backend, &devs, &p))
        }));
    }

    pub fn quick_switch(&mut self, ui: &mut Ui, settings: &mut Settings, _s: &Services, _n: &mut Notices) {
        let mut pick = None;
        ui.add_enabled_ui(!self.busy && !self.devices.is_empty(), |ui| {
            egui::ComboBox::from_id_salt("quick_dns").selected_text(format!("DNS: {}", settings.active_dns)).show_ui(ui, |ui| {
                for (i, p) in self.profiles.iter().enumerate() {
                    if ui.selectable_label(p.name == settings.active_dns, &p.name).clicked() {
                        pick = Some(i);
                    }
                }
            });
        });
        if let Some(i) = pick {
            self.selected = i;
            self.load_editor();
            let ctx = ui.ctx().clone();
            self.start_apply(&ctx, i);
        }
    }

    pub fn page(&mut self, ui: &mut Ui, settings: &mut Settings, services: &Services, n: &mut Notices) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        theme::page_title(ui, "DNS Manager", "Switch resolvers, test response times and roll back safely");
        if let Some(er) = &self.error {
            theme::note(ui, Kind::Error, er);
            ui.add_space(8.0);
        }
        if !services.network_manager && !services.resolved {
            theme::note(ui, Kind::Warning, "Neither NetworkManager nor systemd-resolved is available on the system bus, so DNS settings cannot be changed here. Testing and monitoring still work. Editing /etc/resolv.conf directly is intentionally not supported.");
            ui.add_space(8.0);
        }
        if let Some(env) = &self.env {
            theme::container(ui, "Resolver stack", |ui| {
                for l in env.describe() {
                    ui.label(l);
                }
                for c in &env.conflicts {
                    theme::note(ui, Kind::Warning, c);
                }
            });
            ui.add_space(8.0);
        }
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Applying DNS configuration and verifying...");
            });
        }
        // ---- current state
        let mut refresh = false;
        theme::container_with(ui, "Current configuration", |ui| {
            refresh = ui.button("Refresh").clicked();
        }, |ui| {
            ui.horizontal(|ui| {
                theme::status(ui, if services.network_manager { Kind::Success } else { Kind::Inactive }, "NetworkManager");
                theme::status(ui, if services.resolved { Kind::Success } else { Kind::Inactive }, "systemd-resolved");
                if services.network_manager && services.resolved {
                    ui.separator();
                    ui.label("Backend:");
                    ui.selectable_value(&mut self.backend, Backend::NetworkManager, "NetworkManager (persistent)");
                    ui.selectable_value(&mut self.backend, Backend::Resolved, "systemd-resolved (runtime)");
                }
            });
            if let Some(er) = &self.dev_err {
                ui.label(RichText::new(er).color(p.warning));
            }
            if !self.devices.is_empty() {
                egui::ComboBox::from_id_salt("dnsdev").selected_text(format!("Interface: {}", self.devices[self.sel_dev.min(self.devices.len() - 1)].iface)).show_ui(ui, |ui| {
                    for (i, d) in self.devices.iter().enumerate() {
                        ui.selectable_value(&mut self.sel_dev, i, format!("{}{}", d.iface, if d.conn_id.is_empty() { String::new() } else { format!(" ({})", d.conn_id) }));
                    }
                });
                let d = &self.devices[self.sel_dev.min(self.devices.len() - 1)];
                if !d.conn_id.is_empty() {
                    theme::kv(ui, "Connection profile", &d.conn_id);
                    theme::kv(ui, "IPv4 method / DNS", format!("{} / {}{}", d.v4_method, if d.cfg_v4.is_empty() { "automatic".into() } else { d.cfg_v4.join(", ") }, if d.ignore_v4 { "  (DHCP DNS ignored)" } else { "" }));
                    theme::kv(ui, "IPv6 method / DNS", format!("{} / {}{}", d.v6_method, if d.cfg_v6.is_empty() { "automatic".into() } else { d.cfg_v6.join(", ") }, if d.ignore_v6 { "  (RA DNS ignored)" } else { "" }));
                }
                theme::kv(ui, "In use by the system", if d.effective.is_empty() { crate::network::read_dns().0.join(", ") } else { d.effective.join(", ") });
            }
        });
        if refresh {
            self.refresh(&ctx);
        }
        ui.add_space(8.0);

        // ---- profiles
        let mut do_apply = false;
        let mut save_custom_clicked = false;
        let mut new_custom = false;
        let mut delete_custom = false;
        theme::container(ui, "DNS profiles", |ui| {
            ui.horizontal_wrapped(|ui| {
                for (i, pr) in self.profiles.iter().enumerate() {
                    let active = pr.name == settings.active_dns;
                    if ui.selectable_label(self.selected == i, if active { format!("{} (active)", pr.name) } else { pr.name.clone() }).clicked() {
                        self.selected = i;
                    }
                }
                if ui.button("+ Custom").clicked() {
                    new_custom = true;
                }
            });
            ui.separator();
            let pr = &self.profiles[self.selected.min(self.profiles.len() - 1)];
            if pr.auto {
                ui.label("Use the DNS servers supplied by the network (DHCP / router advertisements).");
            } else if pr.builtin {
                theme::kv(ui, "IPv4", pr.v4.join(", "));
                theme::kv(ui, "IPv6", pr.v6.join(", "));
            } else {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut self.edit_name).desired_width(220.0));
                });
                ui.horizontal(|ui| {
                    ui.label("IPv4 servers");
                    ui.add(egui::TextEdit::singleline(&mut self.edit_v4).desired_width(320.0).hint_text("192.168.1.1, 9.9.9.9"));
                });
                ui.horizontal(|ui| {
                    ui.label("IPv6 servers");
                    ui.add(egui::TextEdit::singleline(&mut self.edit_v6).desired_width(320.0).hint_text("2620:fe::fe"));
                });
                ui.horizontal(|ui| {
                    if ui.button("Save profile").clicked() {
                        save_custom_clicked = true;
                    }
                    if ui.button("Delete").clicked() {
                        delete_custom = true;
                    }
                });
            }
            ui.add_space(6.0);
            ui.checkbox(&mut self.all_conns, "Apply to all active connections (Ethernet, Wi-Fi, mobile, WireGuard); otherwise only the interface selected above");
            ui.add_enabled_ui(!self.busy && !self.devices.is_empty(), |ui| {
                if theme::primary_button(ui, &format!("Apply '{}'", self.profiles[self.selected.min(self.profiles.len() - 1)].name)).clicked() {
                    do_apply = true;
                }
            });
            ui.label(RichText::new("A backup is taken first. If the new servers do not answer a test query, the previous configuration is restored automatically.").small().color(p.text_dim));
        });
        if new_custom {
            let mut name = "Custom DNS".to_string();
            let mut k = 2;
            while self.profiles.iter().any(|p| p.name == name) {
                name = format!("Custom DNS {k}");
                k += 1;
            }
            self.profiles.push(DnsProfile { name, builtin: false, auto: false, v4: vec![], v6: vec![] });
            self.selected = self.profiles.len() - 1;
            self.load_editor();
        }
        if save_custom_clicked {
            let cand = DnsProfile { name: self.edit_name.trim().to_string(), builtin: false, auto: false, v4: split_list(&self.edit_v4), v6: split_list(&self.edit_v6) };
            let dup = self.profiles.iter().enumerate().any(|(i, o)| i != self.selected && o.name == cand.name);
            match validate(&cand) {
                Err(er) => self.error = Some(er),
                Ok(()) if dup => self.error = Some("A profile with this name already exists".into()),
                Ok(()) => {
                    let old = self.profiles[self.selected].name.clone();
                    if settings.active_dns == old {
                        settings.active_dns = cand.name.clone();
                        settings.save();
                    }
                    self.profiles[self.selected] = cand;
                    save_custom(&self.profiles);
                    self.error = None;
                    n.ok("DNS profile saved");
                }
            }
        }
        if delete_custom {
            self.profiles.remove(self.selected);
            self.selected = 0;
            self.load_editor();
            save_custom(&self.profiles);
        }
        if do_apply {
            if !self.profiles[self.selected].builtin {
                // apply what is typed in the editor, not the stale saved copy
                let cand = DnsProfile { name: self.edit_name.trim().to_string(), builtin: false, auto: false, v4: split_list(&self.edit_v4), v6: split_list(&self.edit_v6) };
                if validate(&cand).is_ok() {
                    self.profiles[self.selected] = cand;
                    save_custom(&self.profiles);
                }
            }
            self.start_apply(&ctx, self.selected);
        }
        ui.add_space(8.0);

        // ---- testing
        let mut run_bench = false;
        let mut run_sys = false;
        theme::container(ui, "Resolution and response-time tests", |ui| {
            ui.horizontal(|ui| {
                ui.label("Domain");
                ui.add(egui::TextEdit::singleline(&mut self.domain).desired_width(220.0));
                ui.add_enabled_ui(!self.bench_busy, |ui| {
                    if theme::primary_button(ui, "Benchmark servers").on_hover_text("3 UDP queries to every provider and to the servers currently in use").clicked() {
                        run_bench = true;
                    }
                });
                if ui.button("Resolve via system").clicked() {
                    run_sys = true;
                }
                if self.bench_busy {
                    ui.spinner();
                }
            });
            if let Some(r) = &self.sys_result {
                ui.label(RichText::new(r).monospace());
            }
            if !self.bench.is_empty() {
                egui::Grid::new("bench").striped(true).spacing([20.0, 6.0]).show(ui, |ui| {
                    for h in ["Provider", "Server", "Average", "Best", "Success", "Result"] {
                        ui.label(RichText::new(h).strong().color(p.text_dim));
                    }
                    ui.end_row();
                    let best = self.bench.iter().filter_map(|b| b.avg_ms).fold(f64::MAX, f64::min);
                    for b in &self.bench {
                        ui.label(&b.label);
                        ui.label(&b.server);
                        match b.avg_ms {
                            Some(a) => ui.label(RichText::new(format!("{a:.1} ms")).color(if a <= best + 0.001 { p.success } else { p.text })),
                            None => ui.label(RichText::new("failed").color(p.danger)),
                        };
                        ui.label(b.min_ms.map(|m| format!("{m:.1} ms")).unwrap_or_else(|| "—".into()));
                        ui.label(format!("{}/{}", b.ok, b.total));
                        ui.label(&b.sample);
                        ui.end_row();
                    }
                });
            }
        });
        if run_bench {
            let domain = self.domain.trim().to_string();
            if !valid_host(&domain) {
                self.error = Some("Enter a valid domain name".into());
            } else {
                self.bench_busy = true;
                let mut targets: Vec<(String, String)> = vec![];
                for pr in presets().iter().filter(|p| !p.auto) {
                    if let Some(s) = pr.v4.first() {
                        targets.push((pr.name.clone(), s.clone()));
                    }
                }
                for pr in self.profiles.iter().filter(|p| !p.builtin) {
                    if let Some(s) = pr.v4.first() {
                        targets.push((pr.name.clone(), s.clone()));
                    }
                }
                for s in crate::network::read_dns().0 {
                    targets.push(("In use".into(), s));
                }
                self.rx.push(spawn(&ctx, move || {
                    let handles: Vec<_> = targets.into_iter().map(|(l, s)| { let d = domain.clone(); std::thread::spawn(move || bench(&l, &s, &d, 3)) }).collect();
                    Msg::Bench(handles.into_iter().filter_map(|h| h.join().ok()).collect())
                }));
            }
        }
        if run_sys {
            let d = self.domain.trim().to_string();
            if valid_host(&d) {
                self.rx.push(spawn(&ctx, move || Msg::Sys(d.clone(), system_resolve(&d))));
            }
        }
        ui.add_space(8.0);

        // ---- backups
        theme::container(ui, "Restore previous configuration", |ui| {
            if self.backups.is_empty() {
                ui.label("No DNS backups yet; one is created automatically before each change.");
            }
            for (path, _) in self.backups.clone().iter().take(8) {
                let b: Option<DnsBackup> = fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok());
                let Some(b) = b else { continue };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(fmt_ts(b.ts)).monospace());
                    ui.label(&b.label);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!self.busy, egui::Button::new("Restore")).clicked() {
                            self.confirm_restore = Some(path.clone());
                        }
                    });
                });
            }
        });
        if let Some(path) = self.confirm_restore.clone() {
            let m = egui::Modal::new(egui::Id::new("restore_dns")).show(&ctx, |ui| {
                ui.heading("Restore this DNS configuration?");
                ui.label("The DNS settings stored in the backup replace the current ones for that connection.");
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
                    self.rx.push(spawn(&ctx, move || Msg::Restored(restore(&path))));
                }
                self.confirm_restore = None;
            } else if m.should_close() {
                self.confirm_restore = None;
            }
        }
    }
}

#[cfg(test)]
mod live {
    use super::*;

    fn nmcli(f: &str) -> String {
        run("nmcli", &["-g", f, "con", "show", "Wired connection 1"]).unwrap_or_default()
    }

    #[test]
    #[ignore = "modifies live NetworkManager DNS, then restores it"]
    fn nm_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("sysmon-live-{}", std::process::id()));
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &tmp) };
        let devs = block_on(nm_list()).expect("nm_list");
        println!("devices: {devs:#?}");
        let d = devs.iter().find(|d| d.conn_id == "Wired connection 1").expect("wired").clone();
        let before = (nmcli("ipv4.dns"), nmcli("ipv4.ignore-auto-dns"), nmcli("ipv6.dns"), nmcli("ipv6.method"));
        println!("before: {before:?}");
        let cf = presets().into_iter().find(|p| p.name == "Cloudflare").unwrap();
        let r = apply(Backend::NetworkManager, std::slice::from_ref(&d), &cf);
        println!("apply -> {r:?}");
        let mid = (nmcli("ipv4.dns"), nmcli("ipv4.ignore-auto-dns"), nmcli("ipv6.dns"));
        println!("during: {mid:?}");
        let eff = block_on(nm_list()).unwrap().into_iter().find(|x| x.iface == d.iface).unwrap().effective;
        println!("effective: {eff:?}; resolv.conf: {:?}", crate::network::read_dns().0);
        // restore from the backup taken before the change
        let (path, _) = list_backups("dns").into_iter().last().expect("backup");
        let rr = restore(&path);
        println!("restore -> {rr:?}");
        let after = (nmcli("ipv4.dns"), nmcli("ipv4.ignore-auto-dns"), nmcli("ipv6.dns"), nmcli("ipv6.method"));
        println!("after: {after:?}");
        assert!(r.is_ok() && mid.0.contains("1.1.1.1"));
        assert_eq!(before, after, "configuration must be restored exactly");
        let _ = fs::remove_dir_all(tmp);
    }
}
