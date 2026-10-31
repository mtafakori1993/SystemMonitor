//! Application shell: top navigation, sidebar, page routing, notifications.
use crate::linux::{self, Desktop, Services};
use crate::monitor::{self, Config, Sampler};
use crate::settings::{self, Settings, ThemeMode};
use crate::theme::{self, pal, Kind};
use crate::{cpu, dashboard, memory, network, processes, storage};
use eframe::egui::{self, Color32, RichText};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    Cpu,
    Memory,
    Network,
    Storage,
    Processes,
    Proxy,
    Dns,
    Docker,
    Settings,
}

const PAGES: [(Page, &str, &str); 10] = [
    (Page::Dashboard, "Dashboard", ""),
    (Page::Cpu, "CPU", ""),
    (Page::Memory, "Memory", ""),
    (Page::Network, "Network", ""),
    (Page::Storage, "Storage", ""),
    (Page::Processes, "Processes", ""),
    (Page::Proxy, "Proxy Manager", ""),
    (Page::Dns, "DNS Manager", ""),
    (Page::Docker, "Docker", ""),
    (Page::Settings, "Settings", ""),
];

pub struct Notice {
    pub kind: Kind,
    pub text: String,
    pub until: Option<Instant>,
}

#[derive(Default)]
pub struct Notices(pub Vec<Notice>);

impl Notices {
    pub fn push(&mut self, kind: Kind, text: impl Into<String>) {
        let until = if kind == Kind::Error { None } else { Some(Instant::now() + Duration::from_secs(6)) };
        self.0.push(Notice { kind, text: text.into(), until });
    }
    pub fn ok(&mut self, t: impl Into<String>) {
        self.push(Kind::Success, t)
    }
    pub fn err(&mut self, t: impl Into<String>) {
        self.push(Kind::Error, t)
    }
}

pub struct App {
    sampler: Sampler,
    cfg: Arc<Config>,
    settings: Settings,
    page: Page,
    notices: Notices,
    services: Services,
    services_rx: Option<Receiver<Services>>,
    desktop: Desktop,
    net_selected: String,
    storage_ui: storage::StorageUi,
    proc_ui: processes::ProcUi,
    proxy: crate::proxy::ProxyUi,
    dns: crate::dns::DnsUi,
    docker: crate::docker::ui::DockerUi,
    nav_open: bool,
    shot: Option<(String, Instant)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
        let cfg = Arc::new(Config { interval_ms: settings.interval_ms.into(), history: settings.history.into() });
        theme::apply(&cc.egui_ctx, true);
        theme::apply(&cc.egui_ctx, false);
        cc.egui_ctx.set_theme(match settings.theme {
            ThemeMode::Dark => egui::ThemePreference::Dark,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::System => egui::ThemePreference::System,
        });
        let ctx = cc.egui_ctx.clone();
        let sampler = monitor::start(move || ctx.request_repaint(), cfg.clone());
        let services_rx = Some(linux::spawn(&cc.egui_ctx, linux::detect_services));
        let desktop = linux::desktop();
        let mut notices = Notices::default();
        {
            use eframe::glow::HasContext;
            if let Some(gl) = &cc.gl {
                let r = unsafe { gl.get_parameter_string(eframe::glow::RENDERER) };
                crate::gfx::set_renderer(&r);
                if crate::gfx::is_software_renderer(&r) {
                    notices.push(Kind::Warning, format!("Software rendering ({r}): the UI works but is slower. Raise the refresh interval in Settings if it feels sluggish."));
                }
            }
        }
        let proxy = crate::proxy::ProxyUi::new(&settings, &desktop, &cc.egui_ctx);
        let dns = crate::dns::DnsUi::new(&settings, &cc.egui_ctx);
        let _ = &mut notices;
        Self { sampler, cfg, settings, page: start_page(), notices, services: Services::default(), services_rx, desktop, net_selected: String::new(), storage_ui: Default::default(), proc_ui: Default::default(), proxy, dns, docker: Default::default(), nav_open: true, shot: std::env::var("SYSMON_SHOT").ok().map(|p| (p, Instant::now())) }
    }

    fn sync_settings(&mut self, ctx: &egui::Context) {
        self.cfg.interval_ms.store(self.settings.interval_ms, Ordering::Relaxed);
        self.cfg.history.store(self.settings.history, Ordering::Relaxed);
        ctx.set_theme(match self.settings.theme {
            ThemeMode::Dark => egui::ThemePreference::Dark,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::System => egui::ThemePreference::System,
        });
        self.settings.save();
    }

    fn top_nav(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        let ctx = ui.ctx().clone();
        egui::Panel::top("topnav")
            .exact_size(48.0)
            .frame(egui::Frame::new().fill(p.topnav).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Menu").color(Color32::WHITE)).frame(false)).on_hover_text("Toggle navigation").clicked() {
                        self.nav_open = !self.nav_open;
                    }
                    ui.label(RichText::new("System Monitor").size(17.0).strong().color(Color32::WHITE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let dark = ui.visuals().dark_mode;
                        if ui.add(egui::Button::new(RichText::new(if dark { "Light theme" } else { "Dark theme" }).color(Color32::WHITE)).frame(false)).clicked() {
                            self.settings.theme = if dark { ThemeMode::Light } else { ThemeMode::Dark };
                            self.sync_settings(&ctx);
                        }
                        ui.separator();
                        ui.scope(|ui| {
                            self.dns.quick_switch(ui, &mut self.settings, &self.services, &mut self.notices);
                        });
                        ui.scope(|ui| {
                            self.proxy.quick_switch(ui, &mut self.settings, &self.desktop, &self.services, &mut self.notices);
                        });
                    });
                });
            });
    }

    fn side_nav(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui);
        if !self.nav_open {
            return;
        }
        egui::Panel::left("sidenav")
            .exact_size(210.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.nav).stroke(egui::Stroke::new(1.0, p.border)).inner_margin(egui::Margin::same(10)))
            .show(ui, |ui| {
                ui.add_space(6.0);
                for (pg, label, _icon) in PAGES {
                    let sel = self.page == pg;
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
                    if sel || resp.hovered() {
                        ui.painter().rect_filled(rect, 4, if sel { p.accent.gamma_multiply(0.14) } else { p.container_alt });
                    }
                    if sel {
                        ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height())), 2, p.accent);
                    }
                    ui.painter().text(rect.left_center() + egui::vec2(14.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(14.5), if sel { p.accent } else { p.text });
                    if resp.clicked() {
                        self.page = pg;
                    }
                    if pg == Page::Processes || pg == Page::Network {
                        ui.add_space(0.0);
                    }
                }
            });
    }

    /// Developer aid: `SYSMON_SHOT=/path.ppm` saves this window's own framebuffer after 4 s
    /// (`SYSMON_SHOT_DELAY=<seconds>` to wait longer) and exits.
    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, t0)) = self.shot.clone() else { return };
        ctx.request_repaint_after(Duration::from_millis(300));
        let delay = std::env::var("SYSMON_SHOT_DELAY").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(4).clamp(1, 120);
        if t0.elapsed() > Duration::from_secs(delay) {
            ctx.request_screenshot(move |img| {
                let mut out = format!("P6\n{} {}\n255\n", img.width(), img.height()).into_bytes();
                for p in &img.pixels {
                    out.extend([p.r(), p.g(), p.b()]);
                }
                let _ = std::fs::write(&path, out);
                std::process::exit(0);
            });
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.notices.0.retain(|n| n.until.map(|u| u > now).unwrap_or(true));
        if self.notices.0.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        let p = palette_for(ctx);
        let mut dismiss = None;
        egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::RIGHT_TOP, [-16.0, 60.0]).order(egui::Order::Tooltip).show(ctx, |ui| {
            ui.set_max_width(420.0);
            for (i, n) in self.notices.0.iter().enumerate() {
                let c = theme::kind_color(&p, n.kind);
                egui::Frame::new().fill(p.container).stroke(egui::Stroke::new(1.5, c)).corner_radius(6).inner_margin(egui::Margin::symmetric(12, 9)).shadow(egui::Shadow { offset: [0, 3], blur: 12, spread: 0, color: Color32::from_black_alpha(80) }).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        theme::dot(ui, c);
                        ui.add(egui::Label::new(&n.text).wrap());
                        if ui.add(egui::Button::new("x").frame(false)).clicked() {
                            dismiss = Some(i);
                        }
                    });
                });
            }
        });
        if let Some(i) = dismiss {
            self.notices.0.remove(i);
        }
    }
}

/// `SystemMonitor --page proxy` opens directly on a page (dashboard, cpu, memory, network, storage, processes, proxy, dns, docker, settings).
fn start_page() -> Page {
    let a: Vec<String> = std::env::args().collect();
    let want = a.iter().position(|x| x == "--page").and_then(|i| a.get(i + 1)).map(|s| s.to_lowercase()).unwrap_or_default();
    PAGES.iter().find(|(_, l, _)| l.to_lowercase().starts_with(&want) && !want.is_empty()).map(|p| p.0).unwrap_or(Page::Dashboard)
}

fn palette_for(ctx: &egui::Context) -> theme::Palette {
    theme::palette(ctx.theme() == egui::Theme::Dark)
}

impl Drop for App {
    /// Window closed: cancel background work (the sampler is stopped and joined when `sampler` drops).
    fn drop(&mut self) {
        linux::shutdown().trigger();
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        if let Some(rx) = &self.services_rx {
            if let Ok(s) = rx.try_recv() {
                self.services = s;
                self.services_rx = None;
                self.proxy.on_services(&self.services);
                self.dns.on_services(&self.services, ctx);
            }
        }
        self.proxy.poll(&mut self.notices, &mut self.settings);
        self.dns.poll(&mut self.notices, &mut self.settings);
        self.docker.poll(ctx, &mut self.notices);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = pal(ui);
        self.top_nav(ui);
        self.side_nav(ui);
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(20, 12))).show(ui, |ui| {
            let snap = self.sampler.snapshot();
            if !snap.ready {
                ui.centered_and_justified(|ui| ui.spinner());
                return;
            }
            let scroll = |ui: &mut egui::Ui, f: &mut dyn FnMut(&mut egui::Ui)| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    f(ui);
                    ui.add_space(24.0);
                });
            };
            match self.page {
                Page::Dashboard => scroll(ui, &mut |ui| dashboard::page(ui, &snap, &self.services, &self.settings.active_proxy, &self.settings.active_dns)),
                Page::Cpu => scroll(ui, &mut |ui| cpu::page(ui, &snap)),
                Page::Memory => scroll(ui, &mut |ui| memory::page(ui, &snap)),
                Page::Network => scroll(ui, &mut |ui| network::page(ui, &snap, &mut self.net_selected)),
                Page::Storage => {
                    let can = self.services.pkexec;
                    scroll(ui, &mut |ui| storage::page(ui, &snap, &mut self.storage_ui, can))
                }
                Page::Processes => {
                    if let Some((what, r)) = processes::page(ui, &snap, &mut self.proc_ui) {
                        match r {
                            Ok(()) => self.notices.ok(format!("Signal sent: {what}")),
                            Err(e) => self.notices.err(format!("Could not signal {what}: {e}")),
                        }
                    }
                }
                Page::Proxy => {
                    drop(snap);
                    scroll(ui, &mut |ui| self.proxy.page(ui, &mut self.settings, &self.desktop, &self.services, &mut self.notices));
                }
                Page::Dns => {
                    drop(snap);
                    scroll(ui, &mut |ui| self.dns.page(ui, &mut self.settings, &self.services, &mut self.notices));
                }
                Page::Docker => {
                    drop(snap);
                    scroll(ui, &mut |ui| self.docker.page(ui, &mut self.notices));
                }
                Page::Settings => {
                    drop(snap);
                    let mut changed = false;
                    let mut msg = None;
                    scroll(ui, &mut |ui| changed |= settings::page(ui, &mut self.settings, &self.services, &self.desktop, &mut msg));
                    if let Some((ok, m)) = msg {
                        if ok { self.notices.ok(m) } else { self.notices.err(m) }
                    }
                    if changed {
                        self.sync_settings(&ui.ctx().clone());
                    }
                }
            }
        });
        let ctx = ui.ctx().clone();
        self.toasts(&ctx);
        self.debug_screenshot(&ctx);
    }
}
