//! Process list from /proc and process control.
use crate::linux::uid_names;
#[cfg(feature = "gui")]
use crate::linux::human_bytes;
#[cfg(feature = "gui")]
use crate::monitor::Snapshot;
#[cfg(feature = "gui")]
use crate::theme::{self, pal};
#[cfg(feature = "gui")]
use eframe::egui::{self, RichText, Ui};
#[cfg(feature = "gui")]
use egui_extras::{Column, TableBuilder};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::Instant;

#[derive(Default, Clone)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub user: String,
    pub state: char,
    pub cpu: f32,
    pub mem: u64,
    pub threads: u32,
}

#[derive(Default, Clone)]
pub struct Procs {
    pub list: Vec<ProcInfo>,
    pub running: usize,
}

pub struct ProcSampler {
    prev: HashMap<i32, u64>,
    last: Option<Instant>,
    users: HashMap<u32, String>,
    users_at: Option<Instant>,
    hz: f64,
    page: u64,
}

impl Default for ProcSampler {
    fn default() -> Self {
        unsafe {
            Self { prev: HashMap::new(), last: None, users: HashMap::new(), users_at: None, hz: libc::sysconf(libc::_SC_CLK_TCK).max(1) as f64, page: libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64 }
        }
    }
}

impl ProcSampler {
    pub fn sample(&mut self, out: &mut Procs) {
        let now = Instant::now();
        if self.users_at.map(|t| now.duration_since(t).as_secs() > 60).unwrap_or(true) {
            self.users = uid_names();
            self.users_at = Some(now);
        }
        let dt = self.last.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.last = Some(now);
        let mut list = Vec::with_capacity(self.prev.len().max(256));
        let mut next = HashMap::with_capacity(self.prev.len());
        let Ok(rd) = fs::read_dir("/proc") else { return };
        for e in rd.flatten() {
            let fname = e.file_name();
            let Some(pid) = fname.to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            let Ok(stat) = fs::read_to_string(e.path().join("stat")) else { continue };
            // pid (comm) state ppid ... — comm may contain spaces/parens, so split on the last ')'
            let (Some(lp), Some(rp)) = (stat.find('('), stat.rfind(')')) else { continue };
            let name = stat[lp + 1..rp].to_string();
            let f: Vec<&str> = stat[rp + 2..].split_whitespace().collect();
            if f.len() < 22 {
                continue;
            }
            let g = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            let ticks = g(11) + g(12);
            let cpu = match self.prev.get(&pid) {
                Some(&p) if dt > 0.0 => (ticks.saturating_sub(p) as f64 / self.hz / dt * 100.0) as f32,
                _ => 0.0,
            };
            next.insert(pid, ticks);
            let uid = fs::metadata(e.path()).map(|m| m.uid()).unwrap_or(u32::MAX);
            list.push(ProcInfo {
                pid,
                ppid: g(1) as i32,
                name,
                user: self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string()),
                state: f[0].chars().next().unwrap_or('?'),
                cpu,
                mem: g(21) * self.page,
                threads: g(17) as u32,
            });
        }
        self.prev = next;
        out.running = list.iter().filter(|p| p.state == 'R').count();
        out.list = list;
    }
}

pub fn state_name(c: char) -> &'static str {
    match c {
        'R' => "Running",
        'S' => "Sleeping",
        'D' => "Disk wait",
        'Z' => "Zombie",
        'T' | 't' => "Stopped",
        'I' => "Idle",
        'X' | 'x' => "Dead",
        _ => "Other",
    }
}

#[cfg(feature = "gui")]
#[derive(Clone, Copy, PartialEq)]
pub enum Col {
    Pid,
    Name,
    Cpu,
    Mem,
    User,
    State,
}

#[cfg(feature = "gui")]
pub struct ProcUi {
    pub filter: String,
    pub sort: Col,
    pub asc: bool,
    pub selected: Option<i32>,
    pub confirm: Option<(i32, String, i32)>, // pid, name, signal
}

#[cfg(feature = "gui")]
impl Default for ProcUi {
    fn default() -> Self {
        Self { filter: String::new(), sort: Col::Cpu, asc: false, selected: None, confirm: None }
    }
}

#[cfg(feature = "gui")]
/// Returns `Some((pid, signal, result))` when a signal was sent.
pub fn page(ui: &mut Ui, s: &Snapshot, st: &mut ProcUi) -> Option<(String, Result<(), String>)> {
    let p = pal(ui);
    let mut result = None;
    theme::page_title(ui, "Processes", &format!("{} processes · {} running", s.procs.list.len(), s.procs.running));
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("Search by name, PID or user").desired_width(300.0));
        if !st.filter.is_empty() && ui.button("Clear").clicked() {
            st.filter.clear();
        }
        ui.add_space(12.0);
        let sel = st.selected.and_then(|pid| s.procs.list.iter().find(|p| p.pid == pid));
        ui.add_enabled_ui(sel.is_some(), |ui| {
            if ui.button("Terminate (SIGTERM)").clicked() {
                if let Some(p) = sel {
                    st.confirm = Some((p.pid, p.name.clone(), libc::SIGTERM));
                }
            }
            if theme::danger_button(ui, "Force kill (SIGKILL)").clicked() {
                if let Some(p) = sel {
                    st.confirm = Some((p.pid, p.name.clone(), libc::SIGKILL));
                }
            }
        });
    });
    let q = st.filter.to_lowercase();
    let mut rows: Vec<&ProcInfo> = s.procs.list.iter().filter(|p| q.is_empty() || p.name.to_lowercase().contains(&q) || p.user.to_lowercase().contains(&q) || p.pid.to_string() == q).collect();
    rows.sort_by(|a, b| {
        let o = match st.sort {
            Col::Pid => a.pid.cmp(&b.pid),
            Col::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            Col::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
            Col::Mem => a.mem.cmp(&b.mem),
            Col::User => a.user.cmp(&b.user),
            Col::State => a.state.cmp(&b.state),
        };
        if st.asc { o } else { o.reverse() }
    });
    let hdr = |ui: &mut Ui, st: &mut ProcUi, label: &str, c: Col| {
        let arrow = if st.sort == c { if st.asc { " (asc)" } else { " (desc)" } } else { "" };
        if ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong().color(p.text_dim)).sense(egui::Sense::click())).clicked() {
            if st.sort == c { st.asc = !st.asc } else { st.sort = c; st.asc = matches!(c, Col::Name | Col::User | Col::Pid) }
        }
    };
    let h = ui.available_height() - 6.0;
    TableBuilder::new(ui)
        .striped(true)
        .sense(egui::Sense::click())
        .min_scrolled_height(0.0)
        .max_scroll_height(h)
        .column(Column::exact(70.0))
        .column(Column::remainder().at_least(160.0))
        .column(Column::exact(80.0))
        .column(Column::exact(100.0))
        .column(Column::exact(130.0))
        .column(Column::exact(100.0))
        .header(26.0, |mut r| {
            for (l, c) in [("PID", Col::Pid), ("Name", Col::Name), ("CPU", Col::Cpu), ("Memory", Col::Mem), ("Owner", Col::User), ("State", Col::State)] {
                r.col(|ui| hdr(ui, st, l, c));
            }
        })
        .body(|body| {
            body.rows(24.0, rows.len(), |mut row| {
                let pr = rows[row.index()];
                row.set_selected(st.selected == Some(pr.pid));
                row.col(|ui| {
                    ui.label(pr.pid.to_string());
                });
                row.col(|ui| {
                    ui.add(egui::Label::new(&pr.name).truncate());
                });
                row.col(|ui| {
                    ui.label(RichText::new(format!("{:.1}%", pr.cpu)).color(theme::usage_color(&p, pr.cpu)));
                });
                row.col(|ui| {
                    ui.label(human_bytes(pr.mem));
                });
                row.col(|ui| {
                    ui.add(egui::Label::new(&pr.user).truncate());
                });
                row.col(|ui| {
                    ui.label(state_name(pr.state));
                });
                if row.response().clicked() {
                    st.selected = Some(pr.pid);
                }
            });
        });

    if let Some((pid, name, sig)) = st.confirm.clone() {
        let sig_name = if sig == libc::SIGKILL { "SIGKILL" } else { "SIGTERM" };
        let m = egui::Modal::new(egui::Id::new("kill_confirm")).show(ui.ctx(), |ui| {
            ui.set_width(380.0);
            ui.heading("Terminate process?");
            ui.add_space(6.0);
            ui.label(format!("Send {sig_name} to \"{name}\" (PID {pid})?"));
            if sig == libc::SIGKILL {
                ui.label(RichText::new("SIGKILL cannot be handled; the process loses unsaved data.").color(p.warning));
            }
            if pid <= 1 {
                ui.label(RichText::new("PID 1 cannot be terminated from here.").color(p.danger));
            }
            ui.add_space(10.0);
            let mut done = false;
            ui.horizontal(|ui| {
                ui.add_enabled_ui(pid > 1, |ui| {
                    if theme::danger_button(ui, "Terminate").clicked() {
                        let r = if unsafe { libc::kill(pid, sig) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) };
                        result = Some((format!("{sig_name} to {name} ({pid})"), r));
                        done = true;
                    }
                });
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
            done
        });
        if m.inner || m.should_close() {
            st.confirm = None;
        }
    }
    result
}
