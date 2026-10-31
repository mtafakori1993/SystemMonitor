//! CPU metrics from /proc/stat, /proc/cpuinfo, /proc/loadavg and /sys.
#[cfg(feature = "gui")]
use crate::charts::{line_chart, Series};
use crate::linux::read_trim;
#[cfg(feature = "gui")]
use crate::monitor::Snapshot;
#[cfg(feature = "gui")]
use crate::theme::{self, pal};
#[cfg(feature = "gui")]
use eframe::egui::{self, RichText, Ui};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;

#[derive(Default, Clone)]
pub struct Cpu {
    pub model: String,
    pub vendor: String,
    pub physical: usize,
    pub threads: usize,
    pub usage: f32,
    pub per_core: Vec<f32>,
    pub freq_mhz: Vec<u32>,
    pub max_mhz: u32,
    pub temp: Option<f32>,
    pub temp_label: String,
    pub load: [f32; 3],
    pub governor: String,
    pub procs_running: u32,
    pub hist: VecDeque<f32>,
    pub hist_freq: VecDeque<f32>,
    pub hist_temp: VecDeque<f32>,
}

#[derive(Default)]
pub struct CpuSampler {
    prev: HashMap<usize, (u64, u64)>, // index (usize::MAX = total) -> (total, idle)
    static_done: bool,
    temp_path: Option<(String, String)>,
}

fn parse_stat() -> Vec<(usize, u64, u64)> {
    let mut out = vec![];
    if let Ok(s) = fs::read_to_string("/proc/stat") {
        for l in s.lines() {
            if !l.starts_with("cpu") {
                continue;
            }
            let mut it = l.split_whitespace();
            let name = it.next().unwrap();
            let v: Vec<u64> = it.take(8).filter_map(|x| x.parse().ok()).collect();
            if v.len() < 5 {
                continue;
            }
            let idx = if name == "cpu" { usize::MAX } else { name[3..].parse().unwrap_or(usize::MAX - 1) };
            out.push((idx, v.iter().sum(), v[3] + v[4]));
        }
    }
    out
}

fn find_temp_sensor() -> Option<(String, String)> {
    let mut best: Option<(u8, String, String)> = None;
    for e in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let dir = e.path();
        let name = read_trim(dir.join("name")).unwrap_or_default();
        let rank = match name.as_str() {
            "coretemp" | "k10temp" | "zenpower" => 1,
            "cpu_thermal" | "cpu-thermal" | "soc_thermal" => 2,
            "acpitz" => 3,
            _ => continue,
        };
        // Prefer a package/die sensor if labelled.
        let mut pick = dir.join("temp1_input");
        for i in 1..=16 {
            if let Some(l) = read_trim(dir.join(format!("temp{i}_label"))) {
                if l.starts_with("Package") || l == "Tctl" || l == "Tdie" {
                    pick = dir.join(format!("temp{i}_input"));
                    break;
                }
            }
        }
        if pick.exists() && best.as_ref().map(|b| rank < b.0).unwrap_or(true) {
            best = Some((rank, pick.to_string_lossy().into(), name));
        }
    }
    best.map(|b| (b.1, b.2))
}

impl CpuSampler {
    pub fn sample(&mut self, c: &mut Cpu, cap: usize) {
        if !self.static_done {
            self.static_done = true;
            let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
            let mut cores = HashSet::new();
            let (mut pid, mut threads) = (String::new(), 0);
            for l in info.lines() {
                let (k, v) = match l.split_once(':') {
                    Some((k, v)) => (k.trim(), v.trim()),
                    None => continue,
                };
                match k {
                    "model name" | "Model" | "Hardware" if c.model.is_empty() => c.model = v.to_string(),
                    "vendor_id" if c.vendor.is_empty() => c.vendor = v.to_string(),
                    "processor" => threads += 1,
                    "physical id" => pid = v.to_string(),
                    "core id" => {
                        cores.insert((pid.clone(), v.to_string()));
                    }
                    _ => {}
                }
            }
            c.threads = threads.max(1);
            c.physical = if cores.is_empty() { c.threads } else { cores.len() };
            if c.model.is_empty() {
                c.model = "Unknown CPU".into();
            }
            self.temp_path = find_temp_sensor();
        }
        for (idx, total, idle) in parse_stat() {
            let pct = match self.prev.get(&idx) {
                Some(&(pt, pi)) if total > pt => 100.0 * (1.0 - (idle - pi) as f32 / (total - pt) as f32),
                _ => 0.0,
            };
            self.prev.insert(idx, (total, idle));
            if idx == usize::MAX {
                c.usage = pct.clamp(0.0, 100.0);
            } else if idx < 4096 {
                if c.per_core.len() <= idx {
                    c.per_core.resize(idx + 1, 0.0);
                }
                c.per_core[idx] = pct.clamp(0.0, 100.0);
            }
        }
        c.threads = c.per_core.len().max(c.threads);
        // frequencies
        c.freq_mhz.clear();
        let mut max = 0;
        for i in 0..c.threads {
            let base = format!("/sys/devices/system/cpu/cpu{i}/cpufreq");
            let f = read_trim(format!("{base}/scaling_cur_freq")).and_then(|s| s.parse::<u32>().ok()).map(|k| k / 1000);
            if let Some(f) = f {
                c.freq_mhz.push(f);
            }
            if let Some(m) = read_trim(format!("{base}/cpuinfo_max_freq")).and_then(|s| s.parse::<u32>().ok()) {
                max = max.max(m / 1000);
            }
        }
        if c.freq_mhz.is_empty() {
            // Fallback: /proc/cpuinfo "cpu MHz"
            if let Ok(s) = fs::read_to_string("/proc/cpuinfo") {
                c.freq_mhz = s.lines().filter(|l| l.starts_with("cpu MHz")).filter_map(|l| l.split(':').nth(1)?.trim().parse::<f32>().ok()).map(|f| f as u32).collect();
            }
        }
        c.max_mhz = max;
        c.governor = read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").unwrap_or_default();
        c.temp = None;
        if let Some((p, name)) = &self.temp_path {
            c.temp = read_trim(p).and_then(|s| s.parse::<f32>().ok()).map(|m| m / 1000.0);
            c.temp_label = name.clone();
        }
        if let Some(l) = read_trim("/proc/loadavg") {
            let f: Vec<&str> = l.split_whitespace().collect();
            for i in 0..3 {
                c.load[i] = f.get(i).and_then(|x| x.parse().ok()).unwrap_or(0.0);
            }
            c.procs_running = f.get(3).and_then(|x| x.split('/').next()).and_then(|x| x.parse().ok()).unwrap_or(0);
        }
        crate::linux::push(&mut c.hist, c.usage, cap);
        let af = c.avg_freq();
        crate::linux::push(&mut c.hist_freq, af, cap);
        if let Some(t) = c.temp {
            crate::linux::push(&mut c.hist_temp, t, cap);
        }
    }
}

impl Cpu {
    pub fn avg_freq(&self) -> f32 {
        if self.freq_mhz.is_empty() { 0.0 } else { self.freq_mhz.iter().sum::<u32>() as f32 / self.freq_mhz.len() as f32 }
    }
}

#[cfg(feature = "gui")]
pub fn temp_color(p: &theme::Palette, t: f32) -> egui::Color32 {
    if t >= 85.0 { p.danger } else if t >= 70.0 { p.warning } else { p.success }
}

#[cfg(feature = "gui")]
pub fn page(ui: &mut Ui, s: &Snapshot) {
    let c = &s.cpu;
    let p = pal(ui);
    theme::page_title(ui, "CPU", &c.model);
    theme::card_grid(ui, 200.0, 4, |ui, i| {
        theme::container(ui, ["Utilization", "Frequency", "Temperature", "Load average"][i], |ui| match i {
            0 => {
                theme::big_value(ui, "Total", &format!("{:.1}%", c.usage), Some(theme::usage_color(&p, c.usage)));
                theme::meter(ui, c.usage / 100.0, theme::usage_color(&p, c.usage), 6.0);
            }
            1 => {
                theme::big_value(ui, "Average", &format!("{:.0} MHz", c.avg_freq()), None);
                if c.max_mhz > 0 {
                    ui.label(RichText::new(format!("max {} MHz · {}", c.max_mhz, c.governor)).color(p.text_dim));
                }
            }
            2 => match c.temp {
                Some(t) => {
                    theme::big_value(ui, &c.temp_label, &format!("{t:.0} °C"), Some(temp_color(&p, t)));
                }
                None => {
                    theme::big_value(ui, "Sensor", "n/a", None);
                    ui.label(RichText::new("No hwmon CPU sensor exposed").color(p.text_dim));
                }
            },
            _ => {
                theme::big_value(ui, "1 min", &format!("{:.2}", c.load[0]), None);
                ui.label(RichText::new(format!("5 min {:.2} · 15 min {:.2}", c.load[1], c.load[2])).color(p.text_dim));
            }
        });
    });
    theme::container(ui, "Utilization history", |ui| {
        line_chart(ui, 200.0, s.cap, &[Series { name: "CPU %", color: p.series[0], data: &c.hist }], Some(100.0), &|v| format!("{v:.0}%"));
    });
    ui.add_space(8.0);
    theme::container(ui, &format!("Per-core utilization ({} threads)", c.threads), |ui| {
        theme::card_grid(ui, 220.0, c.per_core.len(), |ui, i| {
            let u = c.per_core[i];
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("CPU {i}")).color(p.text_dim));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let f = c.freq_mhz.get(i).map(|f| format!("  {f} MHz")).unwrap_or_default();
                    ui.label(RichText::new(format!("{u:>5.1}%{f}")).monospace());
                });
            });
            theme::meter(ui, u / 100.0, theme::usage_color(&p, u), 6.0);
        });
    });
    ui.add_space(8.0);
    theme::card_grid(ui, 360.0, 2, |ui, i| {
        if i == 0 {
            theme::container(ui, "Details", |ui| {
                theme::kv(ui, "Model", &c.model);
                theme::kv(ui, "Vendor", &c.vendor);
                theme::kv(ui, "Physical cores", c.physical.to_string());
                theme::kv(ui, "Logical threads", c.threads.to_string());
                theme::kv(ui, "Running tasks", c.procs_running.to_string());
            });
        } else {
            theme::container(ui, "Frequency and temperature history", |ui| {
                line_chart(ui, 110.0, s.cap, &[Series { name: "MHz", color: p.series[1], data: &c.hist_freq }], None, &|v| format!("{v:.0}"));
                if !c.hist_temp.is_empty() {
                    line_chart(ui, 110.0, s.cap, &[Series { name: "°C", color: p.series[4], data: &c.hist_temp }], None, &|v| format!("{v:.0}°"));
                }
            });
        }
    });
}
