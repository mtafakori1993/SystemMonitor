//! Docker page: tab strip, shared state, background jobs, confirmation dialog.
//! The UI thread never touches the socket: every call runs on a worker (`run`, `fetch`), results arrive
//! over channels and are picked up in `poll`.
mod cleanup;
mod containers;
mod daemon;
mod events;
mod images;
mod networks;
mod overview;
mod proxy;
mod storage;
mod volumes;

use super::api::{DiskUsage, Engine};
use super::live::Live;
use super::{audit, probe, Access, Inventory};
use crate::app::Notices;
use crate::linux::{spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Overview,
    Containers,
    Images,
    Storage,
    Cleanup,
    Networks,
    Volumes,
    Events,
    Proxy,
    Daemon,
}

const TABS: [(Tab, &str); 10] = [
    (Tab::Overview, "Overview"),
    (Tab::Containers, "Containers"),
    (Tab::Images, "Images"),
    (Tab::Storage, "Storage"),
    (Tab::Cleanup, "Cleanup"),
    (Tab::Networks, "Networks"),
    (Tab::Volumes, "Volumes"),
    (Tab::Events, "Events & monitoring"),
    (Tab::Proxy, "Proxy"),
    (Tab::Daemon, "Daemon configuration"),
];

const AUTO_REFRESH: Duration = Duration::from_secs(20);

struct OpDone {
    label: String,
    result: Result<String, String>,
}

/// A pending confirmation. `typed` makes the administrator type a word before the button unlocks.
pub(super) struct Confirm {
    pub title: String,
    pub lines: Vec<String>,
    pub warning: Option<String>,
    pub button: String,
    pub danger: bool,
    pub typed: Option<String>,
    pub input: String,
    pub then: Box<dyn FnOnce(&mut DockerUi, &egui::Context)>,
}

pub struct DockerUi {
    started: bool,
    tab: Tab,
    access: Option<Access>,
    probe_rx: Option<Receiver<Access>>,
    live: Option<Live>,
    inv: Option<Arc<Inventory>>,
    inv_err: Option<String>,
    inv_rx: Option<Receiver<Result<Inventory, String>>>,
    inv_at: Option<Instant>,
    seen_gen: u64,
    gen_changed_at: Option<Instant>,
    df: Option<Arc<DiskUsage>>,
    df_err: Option<String>,
    df_rx: Option<Receiver<Result<DiskUsage, String>>>,
    df_stale: bool,
    ops: Vec<Receiver<OpDone>>,
    busy: Vec<String>,
    confirm: Option<Confirm>,
    toasts: Vec<(Kind, String)>,
    state: audit::State,
    c: containers::State,
    im: images::State,
    st: storage::State,
    cl: cleanup::State,
    net: networks::State,
    vol: volumes::State,
    ev: events::State,
    px: proxy::State,
    dm: daemon::State,
}

impl Default for DockerUi {
    fn default() -> Self {
        // developer aid next to SYSMON_SHOT: open the Docker page on a given tab
        let tab = std::env::var("SYSMON_DOCKER_TAB").ok().and_then(|t| TABS.iter().find(|(_, l)| l.to_lowercase().starts_with(&t.to_lowercase())).map(|x| x.0)).unwrap_or(Tab::Overview);
        Self {
            started: false,
            tab,
            access: None,
            probe_rx: None,
            live: None,
            inv: None,
            inv_err: None,
            inv_rx: None,
            inv_at: None,
            seen_gen: 0,
            gen_changed_at: None,
            df: None,
            df_err: None,
            df_rx: None,
            df_stale: true,
            ops: vec![],
            busy: vec![],
            confirm: None,
            toasts: vec![],
            state: audit::State::default(),
            c: Default::default(),
            im: Default::default(),
            st: Default::default(),
            cl: Default::default(),
            net: Default::default(),
            vol: Default::default(),
            ev: Default::default(),
            px: Default::default(),
            dm: Default::default(),
        }
    }
}

impl DockerUi {
    pub fn engine(&self) -> Option<Engine> {
        match &self.access {
            Some(Access::Ready(e)) => Some(e.clone()),
            _ => None,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.started = true;
        self.state = audit::load_state();
        self.reprobe(ctx);
    }

    fn reprobe(&mut self, ctx: &egui::Context) {
        self.live = None;
        self.access = None;
        self.probe_rx = Some(spawn(ctx, probe));
    }

    pub fn refresh(&mut self, ctx: &egui::Context) {
        let Some(e) = self.engine() else { return };
        if self.inv_rx.is_none() {
            self.inv_rx = Some(spawn(ctx, move || Inventory::fetch(&e).map_err(|e| e.to_string())));
        }
    }

    pub fn refresh_df(&mut self, ctx: &egui::Context) {
        let Some(e) = self.engine() else { return };
        if self.df_rx.is_none() {
            self.df_stale = false;
            self.df_rx = Some(spawn(ctx, move || e.disk_usage().map_err(|e| e.to_string())));
        }
    }

    /// Run a Docker operation on a worker. With `audit = Some((action, target))` the outcome is audited.
    pub(super) fn run(&mut self, ctx: &egui::Context, label: impl Into<String>, audit: Option<(String, String)>, f: impl FnOnce(&Engine) -> Result<String, String> + Send + 'static) {
        let Some(e) = self.engine() else {
            self.toast(Kind::Error, "Docker is not connected");
            return;
        };
        let label = label.into();
        self.busy.push(label.clone());
        let l2 = label.clone();
        self.ops.push(spawn(ctx, move || {
            let result = f(&e);
            if let Some((a, t)) = audit {
                audit::record(&a, &t, &result);
            }
            OpDone { label: l2, result }
        }));
    }

    pub(super) fn toast(&mut self, k: Kind, s: impl Into<String>) {
        self.toasts.push((k, s.into()));
    }

    pub(super) fn ask(&mut self, c: Confirm) {
        self.confirm = Some(c);
    }

    /// Called every frame from `App::logic`; does nothing until the page was opened once.
    pub fn poll(&mut self, ctx: &egui::Context, n: &mut Notices) {
        if !self.started {
            return;
        }
        if let Some(rx) = &self.probe_rx {
            if let Ok(a) = rx.try_recv() {
                self.probe_rx = None;
                if let Access::Ready(e) = &a {
                    let c2 = ctx.clone();
                    self.live = Some(Live::start(e.clone(), move || c2.request_repaint()));
                }
                self.access = Some(a);
                self.refresh(ctx);
                self.df_stale = true;
            }
        }
        if let Some(rx) = &self.inv_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.inv_rx = None;
                    self.inv_at = Some(Instant::now());
                    match r {
                        Ok(i) => {
                            self.inv = Some(Arc::new(i));
                            self.inv_err = None;
                        }
                        Err(e) => self.inv_err = Some(e),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.inv_rx = None,
                Err(_) => {}
            }
        }
        if let Some(rx) = &self.df_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.df_rx = None;
                    match r {
                        Ok(d) => {
                            self.df = Some(Arc::new(d));
                            self.df_err = None;
                        }
                        Err(e) => self.df_err = Some(e),
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.df_rx = None,
                Err(_) => {}
            }
        }
        let mut changed = false;
        let mut done = vec![];
        self.ops.retain(|rx| match rx.try_recv() {
            Ok(d) => {
                done.push(d);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for d in done {
            self.busy.retain(|b| *b != d.label);
            changed = true;
            match d.result {
                Ok(m) => n.ok(if m.is_empty() { format!("{}: done", d.label) } else { m }),
                Err(e) => n.err(format!("{} failed: {e}", d.label)),
            }
        }
        if self.ops.is_empty() {
            self.busy.clear(); // a worker that panicked never reports
        }
        // engine events (start/stop/pull/remove ...) refresh the lists, debounced
        if let Some(l) = &self.live {
            let g = l.generation.load(std::sync::atomic::Ordering::Relaxed);
            if g != self.seen_gen {
                self.seen_gen = g;
                self.gen_changed_at = Some(Instant::now());
            }
        }
        if self.gen_changed_at.map(|t| t.elapsed() > Duration::from_millis(600)).unwrap_or(false) {
            self.gen_changed_at = None;
            changed = true;
        }
        if changed {
            self.refresh(ctx);
            self.df_stale = true;
        }
        for (k, t) in self.toasts.drain(..) {
            n.push(k, t);
        }
        self.c.poll(ctx);
        self.im.poll();
        self.cl.poll(&mut self.state, n);
        self.px.poll(n);
        self.dm.poll(n);
        self.net.poll();
        self.vol.poll();
    }

    pub fn page(&mut self, ui: &mut Ui, n: &mut Notices) {
        let ctx = ui.ctx().clone();
        if !self.started {
            self.start(&ctx);
        }
        if let Some(l) = &self.live {
            l.touch();
        }
        if self.engine().is_some() && self.inv_at.map(|t| t.elapsed() > AUTO_REFRESH).unwrap_or(false) {
            self.refresh(&ctx);
        }
        ctx.request_repaint_after(Duration::from_secs(2));
        let p = pal(ui);
        let sub = match (&self.inv, &self.access) {
            (Some(i), Some(Access::Ready(e))) => format!("Docker Engine {} · API {} (using {}) · {}", i.version.version, i.version.api_version, e.api, e.socket.display()),
            _ => "Containers, images, storage, networks, volumes, proxy and daemon settings".into(),
        };
        theme::page_title(ui, "Docker", &sub);
        ui.horizontal_wrapped(|ui| {
            for (t, l) in TABS {
                if ui.selectable_label(self.tab == t, l).clicked() {
                    self.tab = t;
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.small_button("Refresh").clicked() {
                self.refresh(&ctx);
                self.refresh_df(&ctx);
            }
            if self.inv_rx.is_some() || !self.busy.is_empty() {
                ui.spinner();
            }
            if !self.busy.is_empty() {
                ui.label(RichText::new(format!("Working: {}", self.busy.join(", "))).color(p.text_dim).small());
            }
        });
        // connection problems are explained instead of showing empty tables
        match &self.access {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting to the Docker daemon...");
                });
                return;
            }
            Some(Access::Ready(_)) => {}
            Some(a) => {
                let a = a.clone();
                self.unavailable(ui, &a);
                if !matches!(self.tab, Tab::Proxy | Tab::Daemon) {
                    return;
                }
            }
        }
        if let Some(e) = &self.inv_err {
            theme::note(ui, Kind::Error, &format!("Could not read from Docker: {e}"));
            ui.add_space(6.0);
        }
        match self.tab {
            Tab::Overview => overview::show(self, ui),
            Tab::Containers => containers::show(self, ui),
            Tab::Images => images::show(self, ui),
            Tab::Storage => storage::show(self, ui),
            Tab::Cleanup => cleanup::show(self, ui),
            Tab::Networks => networks::show(self, ui),
            Tab::Volumes => volumes::show(self, ui),
            Tab::Events => events::show(self, ui),
            Tab::Proxy => proxy::show(self, ui),
            Tab::Daemon => daemon::show(self, ui),
        }
        self.confirm_dialog(&ctx);
        for (k, t) in self.toasts.drain(..) {
            n.push(k, t);
        }
    }

    fn unavailable(&mut self, ui: &mut Ui, a: &Access) {
        let (k, title, body) = match a {
            Access::NotInstalled => (Kind::Inactive, "Docker is not installed", "Install Docker Engine from your distribution or docs.docker.com, then press Retry.".to_string()),
            Access::NotRunning(s) => (Kind::Warning, "The Docker daemon is not running", format!("{s}\nStart it with `sudo systemctl start docker` (or enable it at boot with `sudo systemctl enable --now docker`), then press Retry.")),
            Access::Denied(s) => (Kind::Error, "Not authorized to manage Docker", format!("The socket {s} is restricted to root and the 'docker' group. Membership in that group is equivalent to root access, so this program does not grant it: an administrator can run `sudo usermod -aG docker $USER` (then log out and in). Proxy and daemon settings below remain readable; changing them asks for administrator authentication.")),
            Access::Failed(s) => (Kind::Error, "Cannot use the Docker daemon", s.clone()),
            Access::Ready(_) => return,
        };
        theme::container(ui, title, |ui| {
            theme::status(ui, k, title);
            ui.add(egui::Label::new(body).wrap());
            if ui.button("Retry").clicked() {
                let ctx = ui.ctx().clone();
                self.reprobe(&ctx);
            }
        });
        ui.add_space(8.0);
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.confirm else { return };
        let mut go = false;
        let mut close = false;
        let m = egui::Modal::new(egui::Id::new("docker_confirm")).show(ctx, |ui| {
            let p = pal(ui);
            ui.set_width(460.0);
            ui.heading(&c.title);
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for l in &c.lines {
                    ui.add(egui::Label::new(l.as_str()).wrap());
                }
            });
            if let Some(w) = &c.warning {
                ui.add_space(4.0);
                theme::note(ui, Kind::Warning, w);
            }
            let mut unlocked = true;
            if let Some(word) = &c.typed {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("Type {word} to confirm:")).color(p.danger));
                ui.add(egui::TextEdit::singleline(&mut c.input).desired_width(260.0));
                unlocked = c.input.trim() == word;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(unlocked, |ui| {
                    let b = if c.danger { theme::danger_button(ui, &c.button) } else { theme::primary_button(ui, &c.button) };
                    if b.clicked() {
                        go = true;
                    }
                });
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if go {
            if let Some(c) = self.confirm.take() {
                (c.then)(self, ctx);
            }
        } else if close || m.should_close() {
            self.confirm = None;
        }
    }
}

// ---------------------------------------------------------------- shared widgets

pub(super) fn state_kind(state: &str, health: &str) -> Kind {
    match (state, health) {
        (_, "unhealthy") => Kind::Error,
        ("running", "starting") => Kind::Warning,
        ("running", _) => Kind::Success,
        ("paused" | "restarting" | "removing", _) => Kind::Warning,
        ("dead", _) => Kind::Error,
        _ => Kind::Inactive,
    }
}

pub(super) fn age(ts: i64) -> String {
    let now = crate::linux::now_secs() as i64;
    if ts <= 0 || ts > now + 60 {
        return "—".into();
    }
    format!("{} ago", crate::linux::human_duration((now - ts) as u64).split(' ').take(2).collect::<Vec<_>>().join(" "))
}

/// Sortable column header; returns nothing, flips `asc` when the active column is clicked again.
pub(super) fn sort_header<K: PartialEq + Copy>(ui: &mut Ui, label: &str, key: K, sort: &mut (K, bool)) {
    let p = pal(ui);
    let arrow = if sort.0 == key { if sort.1 { " (asc)" } else { " (desc)" } } else { "" };
    if ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong().color(p.text_dim)).sense(egui::Sense::click())).clicked() {
        if sort.0 == key {
            sort.1 = !sort.1;
        } else {
            *sort = (key, true);
        }
    }
}

pub(super) fn ord<T: PartialOrd>(a: T, b: T, asc: bool) -> std::cmp::Ordering {
    let o = a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    if asc { o } else { o.reverse() }
}

pub(super) fn search(ui: &mut Ui, s: &mut String, hint: &str) {
    ui.add(egui::TextEdit::singleline(s).hint_text(hint).desired_width(260.0));
    if !s.is_empty() && ui.small_button("Clear").clicked() {
        s.clear();
    }
}

pub(super) fn matches(q: &str, fields: &[&str]) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(&q))
}

/// Pretty JSON in a bounded, selectable monospace box.
pub(super) fn json_box(ui: &mut Ui, id: &str, v: &serde_json::Value) {
    let mut text = serde_json::to_string_pretty(v).unwrap_or_default();
    egui::ScrollArea::vertical().id_salt(id).max_height(380.0).show(ui, |ui| {
        ui.add(egui::TextEdit::multiline(&mut text).code_editor().interactive(true).desired_width(f32::INFINITY));
    });
}

/// Table in a horizontal scroller so narrow windows scroll instead of squeezing columns.
pub(super) fn hscroll(ui: &mut Ui, id: &str, f: impl FnOnce(&mut Ui)) {
    egui::ScrollArea::horizontal().id_salt(id).show(ui, f);
}

pub(super) fn kind_text(ui: &mut Ui, k: Kind, s: &str) {
    let p = pal(ui);
    ui.label(RichText::new(s).color(theme::kind_color(&p, k)));
}

/// Environment variables whose values must not be shown by default.
pub(super) fn sensitive_key(k: &str) -> bool {
    let k = k.to_ascii_uppercase();
    ["PASS", "SECRET", "TOKEN", "KEY", "CREDENTIAL", "AUTH", "PRIVATE", "COOKIE", "SESSION"].iter().any(|s| k.contains(s))
}

pub(super) fn mask_env(kv: &str, reveal: bool) -> String {
    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
    if reveal {
        return kv.to_string();
    }
    if sensitive_key(k) && !v.is_empty() {
        return format!("{k}=****** (hidden)");
    }
    format!("{k}={}", super::audit::scrub(v))
}
