//! Daemon configuration: daemon.json editor with structured fields, live validation, `dockerd --validate`,
//! backups/restore, privileged save and a guarded daemon restart.
use super::{Confirm, DockerUi};
use crate::app::Notices;
use crate::docker::config::{self, BackupInfo, Check};
use crate::linux::{fmt_ts, spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;

#[derive(Default, Clone)]
struct Fields {
    mirrors: String,
    insecure: String,
    dns: String,
    log_driver: String,
    max_size: String,
    max_file: String,
    live_restore: bool,
    data_root: String,
    storage_driver: String,
}

type LoadResult = Result<(Map<String, Value>, bool), String>;

#[derive(Default)]
pub(super) struct State {
    loaded: bool,
    load_rx: Option<Receiver<LoadResult>>,
    load_err: Option<String>,
    existed: bool,
    text: String,
    /// Proxy passwords replaced by a placeholder in the editor.
    secrets: HashMap<String, String>,
    check: Check,
    checked: String,
    dockerd_rx: Option<Receiver<Option<Result<(), String>>>>,
    dockerd: Option<Option<Result<(), String>>>,
    f: Fields,
    backups: Vec<BackupInfo>,
    apply_rx: Option<Receiver<Result<String, String>>>,
    pub(super) restart_needed: bool,
}

impl State {
    pub(super) fn poll(&mut self, n: &mut Notices) {
        if let Some(rx) = &self.load_rx {
            if let Ok(r) = rx.try_recv() {
                self.load_rx = None;
                self.loaded = true;
                match r {
                    Ok((mut m, existed)) => {
                        self.secrets = config::mask_proxy_secrets(&mut m);
                        self.existed = existed;
                        self.text = config::pretty(&m);
                        self.f = fields_from(&m);
                        self.load_err = None;
                    }
                    Err(e) => self.load_err = Some(e),
                }
                self.backups = config::list_daemon_backups();
            }
        }
        if let Some(rx) = &self.dockerd_rx {
            if let Ok(r) = rx.try_recv() {
                self.dockerd_rx = None;
                self.dockerd = Some(r);
            }
        }
        if let Some(rx) = &self.apply_rx {
            if let Ok(r) = rx.try_recv() {
                self.apply_rx = None;
                match r {
                    Ok(m) => {
                        n.ok(m);
                        self.restart_needed = true;
                        self.loaded = false;
                    }
                    Err(e) => n.err(format!("Saving daemon.json failed: {e}")),
                }
                self.backups = config::list_daemon_backups();
            }
        }
    }
}

fn strs(m: &Map<String, Value>, k: &str, sep: &str) -> String {
    m.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(sep)).unwrap_or_default()
}

fn fields_from(m: &Map<String, Value>) -> Fields {
    let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let lo = |k: &str| m.get("log-opts").and_then(|o| o.get(k)).and_then(Value::as_str).unwrap_or_default().to_string();
    Fields { mirrors: strs(m, "registry-mirrors", "\n"), insecure: strs(m, "insecure-registries", "\n"), dns: strs(m, "dns", ", "), log_driver: s("log-driver"), max_size: lo("max-size"), max_file: lo("max-file"), live_restore: m.get("live-restore").and_then(Value::as_bool).unwrap_or(false), data_root: s("data-root"), storage_driver: s("storage-driver") }
}

fn list(s: &str) -> Option<Value> {
    let v: Vec<&str> = s.split(['\n', ',']).map(str::trim).filter(|x| !x.is_empty()).collect();
    if v.is_empty() { None } else { Some(json!(v)) }
}

fn opt(s: &str) -> Option<Value> {
    let s = s.trim();
    if s.is_empty() { None } else { Some(json!(s)) }
}

/// Merge the form into the document; keys the form does not cover (and other log options) stay as they are.
fn apply_fields(m: &Map<String, Value>, f: &Fields) -> Map<String, Value> {
    let mut lo = m.get("log-opts").and_then(Value::as_object).cloned().unwrap_or_default();
    for (k, v) in [("max-size", &f.max_size), ("max-file", &f.max_file)] {
        match opt(v) {
            Some(v) => lo.insert(k.into(), v),
            None => lo.remove(k),
        };
    }
    config::merge(m, &[
        ("registry-mirrors", list(&f.mirrors)),
        ("insecure-registries", list(&f.insecure)),
        ("dns", list(&f.dns)),
        ("log-driver", opt(&f.log_driver)),
        ("log-opts", if lo.is_empty() { None } else { Some(Value::Object(lo)) }),
        ("live-restore", if f.live_restore { Some(json!(true)) } else if m.contains_key("live-restore") { Some(json!(false)) } else { None }),
        ("data-root", opt(&f.data_root)),
        ("storage-driver", opt(&f.storage_driver)),
    ])
}

/// Confirmation for restarting dockerd, shared by the proxy and daemon tabs.
pub(super) fn ask_restart(d: &mut DockerUi) {
    let (live_restore, running) = d.inv.as_ref().map(|i| (i.info.live_restore_enabled, i.count_state("running"))).unwrap_or((false, 0));
    d.ask(Confirm {
        title: "Restart the Docker daemon?".into(),
        lines: vec![
            "Runs `systemctl restart docker.service` after administrator authentication. The new daemon.json is read at start.".into(),
            if live_restore { format!("live-restore is enabled: the {running} running containers keep running during the restart.") } else { format!("{running} running containers are stopped; those with a restart policy (always / unless-stopped) start again, the others stay stopped.") },
            "If the daemon fails to start, restore the previous daemon.json from Backups and restart again.".into(),
        ],
        warning: if !live_restore && running > 0 { Some("Running services will be interrupted.".into()) } else { None },
        button: "Restart Docker".into(),
        danger: !live_restore && running > 0,
        typed: None,
        input: String::new(),
        then: Box::new(|d, ctx| {
            d.px.restart_needed = false;
            d.dm.restart_needed = false;
            d.run(ctx, "restart Docker", Some(("daemon restart".into(), "docker.service".into())), |e| config::restart_daemon(&e.socket));
        }),
    });
}

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    if !d.dm.loaded && d.dm.load_rx.is_none() {
        d.dm.load_rx = Some(spawn(&ctx, config::read_daemon_json));
    }
    if d.dm.restart_needed || d.px.restart_needed {
        theme::container(ui, "Restart required", |ui| {
            theme::status(ui, Kind::Warning, "daemon.json was changed. Docker applies it only when the daemon restarts.");
            if theme::primary_button(ui, "Restart Docker...").clicked() {
                ask_restart(d);
            }
        });
        ui.add_space(8.0);
    }
    if let Some(e) = &d.dm.load_err {
        theme::note(ui, Kind::Error, &format!("{e}\nFix the file by hand (it is not overwritten while it cannot be parsed)."));
        return;
    }
    if !d.dm.loaded {
        ui.spinner();
        return;
    }
    // live validation whenever the text changes
    if d.dm.checked != d.dm.text {
        d.dm.checked = d.dm.text.clone();
        d.dm.check = config::check_text(&d.dm.text).1;
        d.dm.dockerd = None;
    }
    let mut to_json = false;
    theme::container(ui, "Common settings", |ui| {
        let f = &mut d.dm.f;
        egui::Grid::new("dk_dmf").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Registry mirrors (one URL per line)");
            ui.add(egui::TextEdit::multiline(&mut f.mirrors).desired_rows(2).desired_width(380.0).hint_text("https://mirror.gcr.io"));
            ui.end_row();
            ui.label("Insecure registries (host:port or CIDR)");
            ui.add(egui::TextEdit::multiline(&mut f.insecure).desired_rows(2).desired_width(380.0).hint_text("registry.local:5000"));
            ui.end_row();
            ui.label("DNS servers for containers");
            ui.add(egui::TextEdit::singleline(&mut f.dns).desired_width(380.0).hint_text("1.1.1.1, 9.9.9.9"));
            ui.end_row();
            ui.label("Log driver");
            egui::ComboBox::from_id_salt("dk_logdrv").selected_text(if f.log_driver.is_empty() { "default (json-file)".to_string() } else { f.log_driver.clone() }).show_ui(ui, |ui| {
                for dr in ["", "json-file", "local", "journald", "syslog", "none"] {
                    ui.selectable_value(&mut f.log_driver, dr.to_string(), if dr.is_empty() { "default (json-file)" } else { dr });
                }
            });
            ui.end_row();
            ui.label("Log rotation");
            ui.horizontal(|ui| {
                ui.label("max size");
                ui.add(egui::TextEdit::singleline(&mut f.max_size).desired_width(70.0).hint_text("10m"));
                ui.label("files kept");
                ui.add(egui::TextEdit::singleline(&mut f.max_file).desired_width(50.0).hint_text("3"));
            });
            ui.end_row();
            ui.label("Live restore");
            ui.checkbox(&mut f.live_restore, "keep containers running while the daemon restarts");
            ui.end_row();
            ui.label("Data root");
            ui.add(egui::TextEdit::singleline(&mut f.data_root).desired_width(380.0).hint_text("/var/lib/docker"));
            ui.end_row();
            ui.label("Storage driver");
            ui.add(egui::TextEdit::singleline(&mut f.storage_driver).desired_width(160.0).hint_text("automatic"));
            ui.end_row();
        });
        ui.label(RichText::new("Empty fields remove the setting (Docker's default applies). Keys not shown here are kept unchanged. Proxy settings are on the Proxy tab.").color(p.text_dim).small());
        to_json = ui.button("Apply these fields to the JSON below").clicked();
    });
    if to_json {
        match serde_json::from_str::<Value>(&d.dm.text) {
            Ok(Value::Object(m)) => {
                let m = apply_fields(&m, &d.dm.f);
                d.dm.text = config::pretty(&m);
            }
            _ => d.toast(Kind::Error, "Fix the JSON first: the fields are merged into it"),
        }
    }
    ui.add_space(8.0);
    let mut save = false;
    let mut validate = false;
    theme::container(ui, &format!("{}{}", config::DAEMON_JSON, if d.dm.existed { "" } else { " (does not exist yet)" }), |ui| {
        egui::ScrollArea::vertical().id_salt("dk_dmtext").max_height(360.0).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(&mut d.dm.text).code_editor().desired_rows(12).desired_width(f32::INFINITY));
        });
        if !d.dm.secrets.is_empty() {
            ui.label(RichText::new(format!("Proxy passwords are shown as {}; they are kept unless you replace the placeholder.", config::SECRET_PLACEHOLDER)).color(p.text_dim).small());
        }
        let c = &d.dm.check;
        if c.ok() {
            theme::status(ui, Kind::Success, "Valid JSON; known settings pass validation");
        }
        for e in &c.errors {
            theme::status(ui, Kind::Error, e);
        }
        for w in &c.warnings {
            theme::status(ui, Kind::Warning, w);
        }
        match &d.dm.dockerd {
            Some(Some(Ok(()))) => theme::status(ui, Kind::Success, "dockerd --validate: configuration OK"),
            Some(Some(Err(e))) => theme::status(ui, Kind::Error, &format!("dockerd --validate: {e}")),
            Some(None) => theme::status(ui, Kind::Inactive, "dockerd is not installed here; only the built-in checks ran"),
            None => {}
        }
        ui.horizontal(|ui| {
            validate = ui.add_enabled(c.ok() && d.dm.dockerd_rx.is_none(), egui::Button::new("Validate with dockerd")).clicked();
            save = ui.add_enabled(c.ok() && d.dm.apply_rx.is_none(), egui::Button::new(RichText::new("Save...").strong())).clicked();
            if ui.button("Discard changes").clicked() {
                d.dm.loaded = false;
            }
            if d.dm.dockerd_rx.is_some() || d.dm.apply_rx.is_some() {
                ui.spinner();
            }
        });
    });
    let full_text = || -> Result<String, String> {
        match serde_json::from_str::<Value>(&d.dm.text) {
            Ok(Value::Object(mut m)) => {
                config::restore_proxy_secrets(&mut m, &d.dm.secrets);
                Ok(config::pretty(&m))
            }
            _ => Err("invalid JSON".into()),
        }
    };
    if validate {
        if let Ok(t) = full_text() {
            d.dm.dockerd_rx = Some(spawn(&ctx, move || config::dockerd_validate(&t)));
        }
    }
    if save {
        if let Ok(t) = full_text() {
            let warn = d.dm.check.warnings.clone();
            d.ask(Confirm {
                title: "Save daemon.json?".into(),
                lines: {
                    let mut l = vec![format!("Write the edited configuration to {} (administrator authentication required).", config::DAEMON_JSON), "The current file is backed up first (in your settings folder and as a root-only copy next to it), and the new one is checked with dockerd --validate.".into(), "The daemon uses the new settings only after a restart.".into()];
                    l.extend(warn.into_iter().map(|w| format!("Note: {w}")));
                    l
                },
                warning: None,
                button: "Save".into(),
                danger: false,
                typed: None,
                input: String::new(),
                then: Box::new(move |d, ctx| {
                    d.dm.apply_rx = Some(spawn(ctx, move || {
                        let r = config::apply_daemon_json(&t, "daemon configuration edit");
                        crate::docker::audit::record("daemon.json save", config::DAEMON_JSON, &r);
                        r
                    }));
                }),
            });
        }
    }
    ui.add_space(8.0);
    theme::container(ui, "Backups", |ui| {
        ui.horizontal(|ui| {
            if ui.button("Back up current file now").clicked() {
                match config::backup_daemon_json("Manual backup") {
                    Ok(b) => d.toast(Kind::Success, format!("Backup {b} saved")),
                    Err(e) => d.toast(Kind::Error, e),
                }
                d.dm.backups = config::list_daemon_backups();
            }
            ui.label(RichText::new(format!("stored in {} (mode 0600)", crate::linux::backup_dir("docker").display())).color(p.text_dim).small());
        });
        if d.dm.backups.is_empty() {
            ui.label(RichText::new("No backups yet. One is made automatically before every save.").color(p.text_dim));
        }
        let mut load = None;
        egui::Grid::new("dk_baks").striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
            for b in d.dm.backups.iter().take(15) {
                ui.label(format!("{} UTC", fmt_ts(b.ts)));
                ui.label(&b.label);
                if ui.small_button("Load into editor").on_hover_text("Review it, then Save to restore").clicked() {
                    load = Some(b.path.clone());
                }
                ui.end_row();
            }
        });
        if let Some(pth) = load {
            match config::read_backup(&pth) {
                Ok(mut m) => {
                    d.dm.secrets = config::mask_proxy_secrets(&mut m);
                    d.dm.f = fields_from(&m);
                    d.dm.text = config::pretty(&m);
                    d.toast(Kind::Info, "Backup loaded into the editor; press Save to restore it");
                }
                Err(e) => d.toast(Kind::Error, e),
            }
        }
    });
}
