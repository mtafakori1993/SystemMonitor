//! Storage metrics: mounts + statvfs, /proc/diskstats, /sys/block, optional SMART via smartctl.
#[cfg(feature = "gui")]
use crate::charts::{line_chart, Series};
use crate::linux::read_trim;
#[cfg(feature = "gui")]
use crate::linux::{human_bytes, human_rate};
#[cfg(feature = "gui")]
use crate::monitor::Snapshot;
#[cfg(feature = "gui")]
use crate::theme::{self, pal};
#[cfg(feature = "gui")]
use crate::linux::Kind;
#[cfg(feature = "gui")]
use eframe::egui::{self, RichText, Ui};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs;
use std::time::Instant;

#[derive(Default, Clone)]
pub struct Filesystem {
    pub device: String,
    pub mount: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub read_bps: f64,
    pub write_bps: f64,
}

#[derive(Default, Clone)]
pub struct Physical {
    pub name: String,
    pub model: String,
    pub size: u64,
    pub kind: &'static str,
    pub removable: bool,
    pub temp: Option<f32>,
    pub read_bps: f64,
    pub write_bps: f64,
}

#[derive(Default, Clone)]
pub struct Storage {
    pub fs: Vec<Filesystem>,
    pub devices: Vec<Physical>,
    pub hist_r: VecDeque<f32>,
    pub hist_w: VecDeque<f32>,
}

#[derive(Default)]
pub struct StorageSampler {
    prev: HashMap<String, (u64, u64)>,
    last: Option<Instant>,
    ticks: u64,
}

fn unescape(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t").replace("\\134", "\\")
}

fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    let c = CString::new(path).ok()?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let fr = st.f_frsize as u64;
        Some((st.f_blocks as u64 * fr, (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * fr, st.f_bavail as u64 * fr))
    }
}

fn diskstats() -> HashMap<String, (u64, u64)> {
    let mut m = HashMap::new();
    if let Ok(s) = fs::read_to_string("/proc/diskstats") {
        for l in s.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() > 9 {
                if let (Ok(r), Ok(w)) = (f[5].parse::<u64>(), f[9].parse::<u64>()) {
                    m.insert(f[2].to_string(), (r * 512, w * 512));
                }
            }
        }
    }
    m
}

fn block_name(dev: &str) -> String {
    fs::canonicalize(dev).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default()
}

fn physical_devices() -> Vec<Physical> {
    let mut v = vec![];
    let Ok(rd) = fs::read_dir("/sys/block") else { return v };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if ["loop", "ram", "zram", "dm-", "md", "sr", "nbd", "fd"].iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let b = format!("/sys/block/{name}");
        let size = read_trim(format!("{b}/size")).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 512;
        if size == 0 {
            continue;
        }
        let rot = read_trim(format!("{b}/queue/rotational")).as_deref() == Some("1");
        let kind = if name.starts_with("nvme") { "NVMe SSD" } else if name.starts_with("mmcblk") { "Flash card" } else if rot { "HDD" } else { "SSD" };
        let model = read_trim(format!("{b}/device/model")).or_else(|| read_trim(format!("{b}/device/name"))).unwrap_or_default();
        // temperature via hwmon exposed below the device (NVMe, SATA drivetemp)
        let mut temp = None;
        if let Ok(rd) = fs::read_dir(format!("{b}/device")) {
            for d in rd.flatten() {
                if d.file_name().to_string_lossy().starts_with("hwmon") {
                    if let Ok(rd2) = fs::read_dir(d.path()) {
                        for h in rd2.flatten() {
                            if let Some(t) = read_trim(h.path().join("temp1_input")).and_then(|s| s.parse::<f32>().ok()) {
                                temp = Some(t / 1000.0);
                            }
                        }
                    }
                    if let Some(t) = read_trim(d.path().join("temp1_input")).and_then(|s| s.parse::<f32>().ok()) {
                        temp = Some(t / 1000.0);
                    }
                }
            }
        }
        v.push(Physical { removable: read_trim(format!("{b}/removable")).as_deref() == Some("1"), name, model, size, kind, temp, ..Default::default() });
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

impl StorageSampler {
    pub fn sample(&mut self, st: &mut Storage, cap: usize) {
        let now = Instant::now();
        let dt = self.last.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.last = Some(now);
        let stats = diskstats();
        let rate = |name: &str| -> (f64, f64) {
            let cur = stats.get(name).copied().unwrap_or((0, 0));
            let r = match self.prev.get(name) {
                Some(&(pr, pw)) if dt > 0.0 => (cur.0.saturating_sub(pr) as f64 / dt, cur.1.saturating_sub(pw) as f64 / dt),
                _ => (0.0, 0.0),
            };
            r
        };
        if self.ticks % 3 == 0 || st.fs.is_empty() {
            let mut seen = HashSet::new();
            let mut list = vec![];
            for l in fs::read_to_string("/proc/self/mounts").unwrap_or_default().lines() {
                let f: Vec<&str> = l.split_whitespace().collect();
                if f.len() < 3 {
                    continue;
                }
                let (dev, mount, ty) = (unescape(f[0]), unescape(f[1]), f[2]);
                let real = (dev.starts_with("/dev/") && !dev.starts_with("/dev/loop") && ty != "squashfs")
                    || ["nfs", "nfs4", "cifs", "smb3", "zfs", "fuse.sshfs", "ceph"].contains(&ty);
                if !real || !seen.insert(dev.clone()) {
                    continue;
                }
                if let Some((total, used, avail)) = statvfs(&mount) {
                    list.push(Filesystem { device: dev, mount, fstype: ty.to_string(), total, used, avail, ..Default::default() });
                }
            }
            st.fs = list;
        }
        for f in st.fs.iter_mut() {
            let (r, w) = rate(&block_name(&f.device));
            f.read_bps = r;
            f.write_bps = w;
        }
        if self.ticks % 10 == 0 || st.devices.is_empty() {
            st.devices = physical_devices();
        }
        let (mut tr, mut tw) = (0.0, 0.0);
        for d in st.devices.iter_mut() {
            let (r, w) = rate(&d.name);
            d.read_bps = r;
            d.write_bps = w;
            tr += r;
            tw += w;
        }
        self.prev = stats;
        crate::linux::push(&mut st.hist_r, tr as f32, cap);
        crate::linux::push(&mut st.hist_w, tw as f32, cap);
        self.ticks += 1;
    }
}

#[cfg(feature = "gui")]
#[derive(Default)]
pub struct StorageUi {
    pub health: HashMap<String, crate::smart::Report>,
    pub pending: Vec<(String, std::sync::mpsc::Receiver<crate::smart::Report>)>,
}

#[cfg(feature = "gui")]
pub fn page(ui: &mut Ui, s: &Snapshot, ui_state: &mut StorageUi, can_pkexec: bool) {
    let st = &s.storage;
    let p = pal(ui);
    ui_state.pending.retain(|(d, rx)| match rx.try_recv() {
        Ok(r) => {
            ui_state.health.insert(d.clone(), r);
            false
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => true,
        Err(_) => false,
    });
    theme::page_title(ui, "Storage", "Filesystems, physical devices and I/O");
    theme::container(ui, "I/O throughput (all physical devices)", |ui| {
        line_chart(ui, 160.0, s.cap, &[Series { name: "Read", color: p.series[0], data: &st.hist_r }, Series { name: "Write", color: p.series[1], data: &st.hist_w }], None, &|v| human_rate(v as f64));
    });
    ui.add_space(8.0);
    theme::container(ui, "Filesystems", |ui| {
        for f in &st.fs {
            let pct = if f.total == 0 { 0.0 } else { 100.0 * f.used as f32 / f.total as f32 };
            ui.horizontal(|ui| {
                ui.label(RichText::new(&f.mount).strong());
                ui.label(RichText::new(format!("{} · {}", f.device, f.fstype)).color(p.text_dim));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("{} free of {}  ({pct:.0}%)   read {} · write {}", human_bytes(f.avail), human_bytes(f.total), human_rate(f.read_bps), human_rate(f.write_bps)));
                });
            });
            theme::meter(ui, pct / 100.0, theme::usage_color(&p, pct), 6.0);
            ui.add_space(6.0);
        }
        if st.fs.is_empty() {
            ui.label("No block-device filesystems found.");
        }
    });
    ui.add_space(8.0);
    theme::container(ui, "Physical devices", |ui| {
        egui::Grid::new("devs").striped(true).spacing([18.0, 8.0]).show(ui, |ui| {
            for h in ["Device", "Model", "Type", "Size", "Read", "Write", "Temp", "Health"] {
                ui.label(RichText::new(h).strong().color(p.text_dim));
            }
            ui.end_row();
            for d in &st.devices {
                ui.label(format!("/dev/{}", d.name));
                ui.label(if d.model.is_empty() { "—" } else { &d.model });
                ui.label(format!("{}{}", d.kind, if d.removable { " (removable)" } else { "" }));
                ui.label(human_bytes(d.size));
                ui.label(human_rate(d.read_bps));
                ui.label(human_rate(d.write_bps));
                ui.label(d.temp.map(|t| format!("{t:.0} °C")).unwrap_or_else(|| "—".into()));
                ui.horizontal(|ui| {
                    if let Some(r) = ui_state.health.get(&d.name) {
                        theme::status(ui, r.kind(), &r.headline());
                    }
                    let busy = ui_state.pending.iter().any(|(n, _)| *n == d.name);
                    if busy {
                        ui.spinner();
                    } else {
                        if ui.small_button("Check").on_hover_text("Run smartctl -H as the current user").clicked() {
                            let n = d.name.clone();
                            ui_state.pending.push((d.name.clone(), crate::linux::spawn(ui.ctx(), move || crate::smart::check(&n, false))));
                        }
                        if can_pkexec && ui.small_button("Check (admin)").on_hover_text("Run smartctl through PolicyKit (asks for authentication)").clicked() {
                            let n = d.name.clone();
                            ui_state.pending.push((d.name.clone(), crate::linux::spawn(ui.ctx(), move || crate::smart::check(&n, true))));
                        }
                    }
                });
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Refresh all").on_hover_text("Re-read SMART data for every device as the current user (read-only)").clicked() {
                for d in &st.devices {
                    if !ui_state.pending.iter().any(|(n, _)| *n == d.name) {
                        let n = d.name.clone();
                        ui_state.pending.push((d.name.clone(), crate::linux::spawn(ui.ctx(), move || crate::smart::check(&n, false))));
                    }
                }
            }
            ui.label(RichText::new("Read-only. \"No SMART data\" or \"needs permission\" is not a disk failure; only the drive's own FAILED verdict is shown as failing.").color(p.text_dim).small());
        });
    });
    let mut names: Vec<&String> = ui_state.health.keys().collect();
    names.sort();
    if !names.is_empty() {
        ui.add_space(8.0);
        theme::container(ui, "Disk health details", |ui| {
            for n in names {
                let r = &ui_state.health[n];
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("/dev/{n}")).strong());
                    theme::status(ui, r.kind(), &r.headline());
                    ui.label(RichText::new(format!("{} {}", r.model, r.firmware)).color(p.text_dim));
                });
                let mut facts: Vec<String> = vec![];
                if let Some(t) = r.temperature_c { facts.push(format!("{t} °C")); }
                if let Some(h) = r.power_on_hours { facts.push(format!("{h} h powered on")); }
                if let Some(w) = r.wear_used_pct { facts.push(format!("{w}% endurance used")); }
                if let Some(sp) = r.spare_pct { facts.push(format!("{sp}% spare")); }
                if let Some(x) = r.reallocated { facts.push(format!("{x} reallocated")); }
                if let Some(x) = r.pending { facts.push(format!("{x} pending")); }
                if let Some(x) = r.media_errors { facts.push(format!("{x} media errors")); }
                if r.device_type != "auto" { facts.push(format!("smartctl -d {}", r.device_type)); }
                if !facts.is_empty() {
                    ui.label(facts.join("  ·  "));
                }
                for w in &r.warnings {
                    theme::status(ui, Kind::Warning, w);
                }
                ui.add_space(6.0);
            }
        });
    }
}
