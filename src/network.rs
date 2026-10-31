//! Network interface metrics: /proc/net/dev, /sys/class/net, getifaddrs, routes, resolv.conf.
#[cfg(feature = "gui")]
use crate::charts::{line_chart, Series};
use crate::linux::read_trim;
#[cfg(feature = "gui")]
use crate::linux::{human_bytes, human_rate};
#[cfg(feature = "gui")]
use crate::monitor::Snapshot;
#[cfg(feature = "gui")]
use crate::theme::{self, pal};
use crate::linux::Kind;
#[cfg(feature = "gui")]
use eframe::egui::{self, RichText, Ui};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Instant;

#[derive(Default, Clone)]
pub struct Iface {
    pub name: String,
    pub state: String,
    pub mac: String,
    pub mtu: u32,
    pub speed: Option<u32>,
    pub kind: &'static str,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub rx_err: u64,
    pub tx_err: u64,
    pub gateway4: Option<String>,
    pub gateway6: Option<String>,
    pub hist_rx: VecDeque<f32>,
    pub hist_tx: VecDeque<f32>,
}

#[derive(Default, Clone)]
pub struct Net {
    pub ifaces: Vec<Iface>,
    pub dns: Vec<String>,
    pub search: Vec<String>,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub hist_rx: VecDeque<f32>,
    pub hist_tx: VecDeque<f32>,
}

#[derive(Default)]
pub struct NetSampler {
    prev: HashMap<String, (u64, u64)>,
    last: Option<Instant>,
    hist: HashMap<String, (VecDeque<f32>, VecDeque<f32>)>,
    ticks: u64,
}

fn addresses() -> HashMap<String, (Vec<String>, Vec<String>)> {
    let mut map: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return map;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let a = &*cur;
            if !a.ifa_addr.is_null() {
                let name = std::ffi::CStr::from_ptr(a.ifa_name).to_string_lossy().into_owned();
                match (*a.ifa_addr).sa_family as i32 {
                    libc::AF_INET => {
                        let ip = Ipv4Addr::from(u32::from_be((*(a.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr));
                        let plen = if a.ifa_netmask.is_null() { 32 } else { u32::from_be((*(a.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr).count_ones() };
                        map.entry(name).or_default().0.push(format!("{ip}/{plen}"));
                    }
                    libc::AF_INET6 => {
                        let ip = Ipv6Addr::from((*(a.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr);
                        let plen = if a.ifa_netmask.is_null() { 128 } else { (*(a.ifa_netmask as *const libc::sockaddr_in6)).sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum() };
                        map.entry(name).or_default().1.push(format!("{ip}/{plen}"));
                    }
                    _ => {}
                }
            }
            cur = a.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    map
}

fn gateways() -> (HashMap<String, String>, HashMap<String, String>) {
    let (mut g4, mut g6) = (HashMap::new(), HashMap::new());
    if let Ok(s) = fs::read_to_string("/proc/net/route") {
        for l in s.lines().skip(1) {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() > 3 && f[1] == "00000000" {
                if let Ok(gw) = u32::from_str_radix(f[2], 16) {
                    g4.entry(f[0].to_string()).or_insert_with(|| Ipv4Addr::from(gw.swap_bytes()).to_string());
                }
            }
        }
    }
    if let Ok(s) = fs::read_to_string("/proc/net/ipv6_route") {
        for l in s.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            // dest, dest_plen, src, src_plen, next_hop, metric, refcnt, use, flags, iface
            if f.len() >= 10 && f[0] == "0".repeat(32) && f[1] == "00" && f[4] != "0".repeat(32) {
                if let Ok(v) = u128::from_str_radix(f[4], 16) {
                    g6.entry(f[9].to_string()).or_insert_with(|| Ipv6Addr::from(v).to_string());
                }
            }
        }
    }
    (g4, g6)
}

pub fn read_dns() -> (Vec<String>, Vec<String>) {
    let mut path = "/etc/resolv.conf";
    let first = fs::read_to_string(path).unwrap_or_default();
    // systemd-resolved stub: show the real upstream servers instead of 127.0.0.53.
    let mut txt = first.clone();
    if first.contains("127.0.0.53") && std::path::Path::new("/run/systemd/resolve/resolv.conf").exists() {
        path = "/run/systemd/resolve/resolv.conf";
        txt = fs::read_to_string(path).unwrap_or(first);
    }
    let mut dns = vec![];
    let mut search = vec![];
    for l in txt.lines() {
        let mut it = l.split_whitespace();
        match it.next() {
            Some("nameserver") => dns.extend(it.next().map(String::from)),
            Some("search") | Some("domain") => search.extend(it.map(String::from)),
            _ => {}
        }
    }
    (dns, search)
}

fn kind_of(name: &str) -> &'static str {
    let base = format!("/sys/class/net/{name}");
    if name == "lo" {
        "Loopback"
    } else if fs::metadata(format!("{base}/wireless")).is_ok() || fs::metadata(format!("{base}/phy80211")).is_ok() {
        "Wi-Fi"
    } else if fs::metadata(format!("{base}/bridge")).is_ok() {
        "Bridge"
    } else if fs::metadata(format!("{base}/tun_flags")).is_ok() || name.starts_with("tun") || name.starts_with("wg") {
        "Tunnel"
    } else if fs::read_link(&base).map(|p| p.to_string_lossy().contains("/virtual/")).unwrap_or(false) {
        "Virtual"
    } else {
        "Ethernet"
    }
}

impl NetSampler {
    pub fn sample(&mut self, n: &mut Net, cap: usize) {
        let now = Instant::now();
        let dt = self.last.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.last = Some(now);
        let addrs = addresses();
        let (g4, g6) = gateways();
        let mut ifaces = vec![];
        let (mut rx_sum, mut tx_sum) = (0.0, 0.0);
        if let Ok(s) = fs::read_to_string("/proc/net/dev") {
            for l in s.lines().skip(2) {
                let Some((name, rest)) = l.split_once(':') else { continue };
                let name = name.trim().to_string();
                let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if v.len() < 16 {
                    continue;
                }
                let (rx, tx) = (v[0], v[8]);
                let (rbps, tbps) = match self.prev.get(&name) {
                    Some(&(pr, pt)) if dt > 0.0 => (rx.saturating_sub(pr) as f64 / dt, tx.saturating_sub(pt) as f64 / dt),
                    _ => (0.0, 0.0),
                };
                self.prev.insert(name.clone(), (rx, tx));
                let base = format!("/sys/class/net/{name}");
                let (a4, a6) = addrs.get(&name).cloned().unwrap_or_default();
                let h = self.hist.entry(name.clone()).or_default();
                crate::linux::push(&mut h.0, rbps as f32, cap);
                crate::linux::push(&mut h.1, tbps as f32, cap);
                if name != "lo" {
                    rx_sum += rbps;
                    tx_sum += tbps;
                }
                ifaces.push(Iface {
                    state: read_trim(format!("{base}/operstate")).unwrap_or_else(|| "unknown".into()),
                    mac: read_trim(format!("{base}/address")).unwrap_or_default(),
                    mtu: read_trim(format!("{base}/mtu")).and_then(|s| s.parse().ok()).unwrap_or(0),
                    speed: read_trim(format!("{base}/speed")).and_then(|s| s.parse::<i32>().ok()).filter(|s| *s > 0).map(|s| s as u32),
                    kind: kind_of(&name),
                    ipv4: a4,
                    ipv6: a6,
                    rx_bps: rbps,
                    tx_bps: tbps,
                    rx_total: rx,
                    tx_total: tx,
                    rx_err: v[2],
                    tx_err: v[10],
                    gateway4: g4.get(&name).cloned(),
                    gateway6: g6.get(&name).cloned(),
                    hist_rx: h.0.clone(),
                    hist_tx: h.1.clone(),
                    name,
                });
            }
        }
        n.ifaces = ifaces;
        n.rx_bps = rx_sum;
        n.tx_bps = tx_sum;
        crate::linux::push(&mut n.hist_rx, rx_sum as f32, cap);
        crate::linux::push(&mut n.hist_tx, tx_sum as f32, cap);
        if self.ticks % 5 == 0 {
            let (d, s) = read_dns();
            n.dns = d;
            n.search = s;
        }
        self.ticks += 1;
    }
}

pub fn state_kind(state: &str) -> Kind {
    match state {
        "up" => Kind::Success,
        "down" | "lowerlayerdown" | "notpresent" => Kind::Error,
        "unknown" => Kind::Inactive, // e.g. loopback, tun
        _ => Kind::Warning,
    }
}

#[cfg(feature = "gui")]
pub fn page(ui: &mut Ui, s: &Snapshot, selected: &mut String) {
    let n = &s.net;
    let p = pal(ui);
    theme::page_title(ui, "Network", "Interfaces, throughput and resolver configuration");
    theme::card_grid(ui, 220.0, 4, |ui, i| {
        theme::container(ui, ["Download", "Upload", "Total received", "Total sent"][i], |ui| {
            let real = n.ifaces.iter().filter(|f| f.name != "lo");
            match i {
                0 => theme::big_value(ui, "All interfaces", &human_rate(n.rx_bps), Some(p.series[0])),
                1 => theme::big_value(ui, "All interfaces", &human_rate(n.tx_bps), Some(p.series[1])),
                2 => theme::big_value(ui, "Since boot", &human_bytes(real.map(|f| f.rx_total).sum()), None),
                _ => theme::big_value(ui, "Since boot", &human_bytes(real.map(|f| f.tx_total).sum()), None),
            }
        });
    });
    theme::container(ui, "Real-time throughput", |ui| {
        line_chart(
            ui,
            200.0,
            s.cap,
            &[Series { name: "Download", color: p.series[0], data: &n.hist_rx }, Series { name: "Upload", color: p.series[1], data: &n.hist_tx }],
            None,
            &|v| human_rate(v as f64),
        );
    });
    ui.add_space(8.0);
    theme::container(ui, "Interfaces", |ui| {
        egui::Grid::new("ifaces").striped(true).num_columns(7).spacing([18.0, 8.0]).show(ui, |ui| {
            for h in ["Name", "Status", "Type", "IPv4", "Down", "Up", "Total rx / tx"] {
                ui.label(RichText::new(h).strong().color(p.text_dim));
            }
            ui.end_row();
            for f in &n.ifaces {
                if ui.selectable_label(*selected == f.name, &f.name).clicked() {
                    *selected = f.name.clone();
                }
                theme::status(ui, state_kind(&f.state), &f.state.to_uppercase());
                ui.label(f.kind);
                ui.label(f.ipv4.first().cloned().unwrap_or_else(|| "—".into()));
                ui.label(human_rate(f.rx_bps));
                ui.label(human_rate(f.tx_bps));
                ui.label(format!("{} / {}", human_bytes(f.rx_total), human_bytes(f.tx_total)));
                ui.end_row();
            }
        });
    });
    if selected.is_empty() {
        if let Some(f) = n.ifaces.iter().find(|f| f.state == "up" && f.name != "lo").or(n.ifaces.first()) {
            *selected = f.name.clone();
        }
    }
    ui.add_space(8.0);
    theme::card_grid(ui, 380.0, 2, |ui, i| {
        if i == 0 {
            if let Some(f) = n.ifaces.iter().find(|f| f.name == *selected) {
                theme::container(ui, &format!("Interface {}", f.name), |ui| {
                    theme::kv(ui, "Status", f.state.to_uppercase());
                    theme::kv(ui, "Type", f.kind);
                    theme::kv(ui, "MAC address", if f.mac.is_empty() { "—".into() } else { f.mac.clone() });
                    theme::kv(ui, "MTU", f.mtu.to_string());
                    theme::kv(ui, "Link speed", f.speed.map(|s| format!("{s} Mb/s")).unwrap_or_else(|| "—".into()));
                    theme::kv(ui, "IPv4", if f.ipv4.is_empty() { "—".into() } else { f.ipv4.join(", ") });
                    theme::kv(ui, "IPv6", if f.ipv6.is_empty() { "—".into() } else { f.ipv6.join(", ") });
                    theme::kv(ui, "Gateway", [f.gateway4.clone(), f.gateway6.clone()].into_iter().flatten().collect::<Vec<_>>().join(", ").pipe_empty());
                    theme::kv(ui, "Errors rx/tx", format!("{} / {}", f.rx_err, f.tx_err));
                    ui.add_space(6.0);
                    line_chart(ui, 120.0, s.cap, &[Series { name: "Down", color: p.series[0], data: &f.hist_rx }, Series { name: "Up", color: p.series[1], data: &f.hist_tx }], None, &|v| human_rate(v as f64));
                });
            }
        } else {
            theme::container(ui, "Resolver", |ui| {
                theme::kv(ui, "DNS servers", if n.dns.is_empty() { "—".into() } else { n.dns.join(", ") });
                theme::kv(ui, "Search domains", if n.search.is_empty() { "—".into() } else { n.search.join(", ") });
                let gws: Vec<String> = n.ifaces.iter().filter_map(|f| f.gateway4.as_ref().map(|g| format!("{g} ({})", f.name))).collect();
                theme::kv(ui, "Default gateway", if gws.is_empty() { "—".into() } else { gws.join(", ") });
                ui.label(RichText::new("Change DNS servers in the DNS Manager.").color(p.text_dim).small());
            });
        }
    });
}

#[cfg(feature = "gui")]
trait PipeEmpty {
    fn pipe_empty(self) -> String;
}
#[cfg(feature = "gui")]
impl PipeEmpty for String {
    fn pipe_empty(self) -> String {
        if self.is_empty() { "—".into() } else { self }
    }
}
