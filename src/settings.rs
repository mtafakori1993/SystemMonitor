//! Persistent user settings (JSON in ~/.config/SystemMonitor) and the Settings page.
use crate::linux::{app_dir, Services};
#[cfg(feature = "gui")]
use crate::linux::Desktop;
use crate::store;
#[cfg(feature = "gui")]
use crate::linux::Kind;
#[cfg(feature = "gui")]
use crate::theme;
#[cfg(feature = "gui")]
use eframe::egui::{self, Ui};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub enum ThemeMode {
    Dark,
    Light,
    System,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    pub interval_ms: u64,
    pub history: usize,
    pub active_proxy: String,
    pub active_dns: String,
    /// Graphics backend policy: auto, hardware, software or headless (see `gfx`).
    pub graphics: String,
}

const SPEC: store::Spec<Settings> = store::Spec { file: "settings.json", version: 1, migrate: store::no_migration, validate: validate };

fn validate(s: &mut Settings) -> Vec<String> {
    let mut n = vec![];
    let (iv, h) = (s.interval_ms.clamp(250, 10_000), s.history.clamp(30, 900));
    if (iv, h) != (s.interval_ms, s.history) {
        n.push("refresh interval / history out of range, clamped".into());
    }
    s.interval_ms = iv;
    s.history = h;
    if crate::gfx::Mode::parse(&s.graphics).is_none() {
        n.push(format!("unknown graphics mode '{}', using auto", s.graphics));
        s.graphics = "auto".into();
    }
    n
}

impl Default for Settings {
    fn default() -> Self {
        Self { theme: ThemeMode::Dark, interval_ms: 1000, history: 120, active_proxy: "System Default".into(), active_dns: "Automatic (DHCP)".into(), graphics: "auto".into() }
    }
}

impl Settings {
    pub fn load() -> Self {
        let l = store::load(&app_dir(), &SPEC);
        for n in &l.notes {
            eprintln!("settings: {n}");
        }
        l.value
    }
    pub fn save(&self) {
        if let Err(e) = store::save(&app_dir(), &SPEC, self) {
            eprintln!("settings: {e}");
        }
    }
}

#[cfg(feature = "gui")]
pub fn page(ui: &mut Ui, s: &mut Settings, services: &Services, desktop: &Desktop, msg: &mut Option<(bool, String)>) -> bool {
    let mut changed = false;
    theme::page_title(ui, "Settings", "Appearance, sampling and environment");
    theme::container(ui, "Appearance", |ui| {
        ui.horizontal(|ui| {
            for (m, l) in [(ThemeMode::Dark, "Dark"), (ThemeMode::Light, "Light"), (ThemeMode::System, "Follow system")] {
                changed |= ui.selectable_value(&mut s.theme, m, l).changed();
            }
        });
    });
    ui.add_space(8.0);
    theme::container(ui, "Monitoring", |ui| {
        ui.horizontal(|ui| {
            ui.label("Refresh interval");
            changed |= ui.add(egui::Slider::new(&mut s.interval_ms, 250..=5000).suffix(" ms")).changed();
        });
        ui.horizontal(|ui| {
            ui.label("History length");
            changed |= ui.add(egui::Slider::new(&mut s.history, 30..=600).suffix(" samples")).changed();
        });
        ui.label("Longer intervals lower CPU usage; the sampler thread sleeps between reads.");
    });
    ui.add_space(8.0);
    theme::container(ui, "Graphics", |ui| {
        let gi = crate::gfx::runtime_info();
        theme::kv(ui, "Renderer", if gi.renderer.is_empty() { "unknown".into() } else { gi.renderer.clone() });
        let rep = crate::gfx::detect();
        theme::kv(ui, "Virtual machine", rep.hypervisor.clone().unwrap_or_else(|| "not detected".into()));
        for g in &rep.gpus {
            theme::kv(ui, &format!("GPU {}", g.card), format!("{} (driver {}){}", g.vendor, if g.driver.is_empty() { "?" } else { &g.driver }, g.virtual_kind.map(|k| format!(" — {k}")).unwrap_or_default()));
        }
        theme::kv(ui, "Initialization stage", if gi.stage.is_empty() { "unknown".into() } else { gi.stage.clone() });
        if crate::gfx::is_software_renderer(&gi.renderer) {
            theme::status(ui, Kind::Warning, "Software rendering (no GPU acceleration)");
        }
        ui.horizontal(|ui| {
            ui.label("Backend at next start");
            egui::ComboBox::from_id_salt("gfxmode").selected_text(s.graphics.clone()).show_ui(ui, |ui| {
                for m in ["auto", "hardware", "software", "headless"] {
                    changed |= ui.selectable_value(&mut s.graphics, m.to_string(), m).changed();
                }
            });
        });
        ui.label("auto: GPU first, then Mesa llvmpipe. Overridden by --gfx <mode> or SYSMON_GFX. Diagnostics: SystemMonitor --cli gfx");
    });
    ui.add_space(8.0);
    theme::container(ui, "Launcher", |ui| {
        let installed = crate::install::is_installed();
        theme::status(ui, if installed { Kind::Success } else { Kind::Inactive }, if installed { "Installed in the application menu" } else { "Not installed" });
        ui.label("Adds \"System Monitor\" to your application menu (with Proxy / DNS quick actions) and keeps a copy of the program in ~/.local/bin. Then right-click it and choose Pin to Task Manager / Add to Favorites / Add to Dock.");
        ui.horizontal(|ui| {
            if theme::primary_button(ui, if installed { "Reinstall / update" } else { "Add to application menu" }).clicked() {
                *msg = Some(match crate::install::install() { Ok(m) => (true, m), Err(e) => (false, e) });
            }
            if installed && ui.button("Remove").clicked() {
                *msg = Some(match crate::install::uninstall() { Ok(m) => (true, m), Err(e) => (false, e) });
            }
        });
    });
    ui.add_space(8.0);
    theme::container(ui, "Detected environment", |ui| {
        theme::kv(ui, "Desktop", if desktop.xdg.is_empty() { "unknown".into() } else { desktop.xdg.clone() });
        theme::kv(ui, "Session type", if desktop.session.is_empty() { "unknown".into() } else { desktop.session.clone() });
        for (name, ok, why) in service_rows(services) {
            ui.horizontal(|ui| {
                ui.label(name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::status(ui, if ok { Kind::Success } else { Kind::Inactive }, if ok { "available" } else { why });
                });
            });
        }
    });
    ui.add_space(8.0);
    theme::container(ui, "Storage locations", |ui| {
        theme::kv(ui, "Settings, profiles and backups", app_dir().display().to_string());
        ui.label("Passwords are never written there; they go to the desktop Secret Service when available.");
    });
    ui.add_space(8.0);
    theme::container(ui, "About", |ui| {
        theme::kv(ui, "Version", env!("CARGO_PKG_VERSION"));
        ui.label("Single-file Linux system monitor and network manager. Runs as your user; administrative actions go through PolicyKit.");
    });
    changed
}

pub fn service_rows(s: &Services) -> Vec<(&'static str, bool, &'static str)> {
    vec![
        ("NetworkManager (D-Bus)", s.network_manager, "not running"),
        ("systemd-resolved (D-Bus)", s.resolved, "not running"),
        ("Secret Service (keyring)", s.secret_service, "not running"),
        ("PolicyKit", s.polkit && s.pkexec, "pkexec/polkit missing"),
        ("gsettings (GNOME proxy)", s.gsettings, "not installed"),
        ("smartctl (disk health)", s.smartctl, "not installed"),
    ]
}
