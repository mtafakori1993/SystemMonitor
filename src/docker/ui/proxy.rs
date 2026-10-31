//! Docker proxy: HTTP_PROXY / HTTPS_PROXY / ALL_PROXY / NO_PROXY for the daemon (daemon.json) and for
//! containers and builds (~/.docker/config.json). Existing passwords are kept in memory and never shown.
use super::{Confirm, DockerUi};
use crate::app::Notices;
use crate::docker::config::{self, ProxyScheme, ProxySet, ProxyUrl};
use crate::linux::{parse_port, spawn, valid_host, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use std::sync::mpsc::Receiver;

#[derive(Clone)]
struct Entry {
    on: bool,
    scheme: ProxyScheme,
    host: String,
    port: String,
}

impl Entry {
    fn new(scheme: ProxyScheme) -> Self {
        Self { on: false, scheme, host: String::new(), port: String::new() }
    }
    fn set(&mut self, url: &str) -> Option<ProxyUrl> {
        let u = ProxyUrl::parse(url).ok()?;
        *self = Entry { on: true, scheme: u.scheme, host: u.host.clone(), port: u.port.to_string() };
        Some(u)
    }
}

struct Loaded {
    daemon: Result<ProxySet, String>,
    client: Result<ProxySet, String>,
    dropins: Vec<(String, String)>,
}

pub(super) struct State {
    loaded: Option<Loaded>,
    load_rx: Option<Receiver<Loaded>>,
    to_daemon: bool,
    to_client: bool,
    enabled: bool,
    e: [Entry; 3],
    no_proxy: String,
    user: String,
    password: String,
    /// Password found in the current configuration; reused when the field is left empty.
    kept_pw: String,
    test_host: String,
    test_rx: Option<Receiver<Vec<(String, Result<String, String>)>>>,
    tests: Vec<(String, Result<String, String>)>,
    apply_rx: Vec<Receiver<(bool, Result<String, String>)>>,
    pub(super) restart_needed: bool,
    error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            loaded: None,
            load_rx: None,
            to_daemon: true,
            to_client: false,
            enabled: true,
            e: [Entry::new(ProxyScheme::Http), Entry::new(ProxyScheme::Http), Entry::new(ProxyScheme::Socks5)],
            no_proxy: "localhost,127.0.0.1,::1".into(),
            user: String::new(),
            password: String::new(),
            kept_pw: String::new(),
            test_host: "registry-1.docker.io".into(),
            test_rx: None,
            tests: vec![],
            apply_rx: vec![],
            restart_needed: false,
            error: None,
        }
    }
}

const NAMES: [&str; 3] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];

impl State {
    pub(super) fn poll(&mut self, n: &mut Notices) {
        if let Some(rx) = &self.load_rx {
            if let Ok(l) = rx.try_recv() {
                self.load_rx = None;
                // fill the form from the daemon settings, or from the client ones when the daemon has none
                let src = match (&l.daemon, &l.client) {
                    (Ok(d), _) if !d.is_empty() => Some(d.clone()),
                    (_, Ok(c)) if !c.is_empty() => Some(c.clone()),
                    _ => None,
                };
                if let Some(s) = src {
                    self.enabled = true;
                    for (i, v) in [&s.http, &s.https, &s.all].into_iter().enumerate() {
                        self.e[i].on = false;
                        if let Some(u) = self.e[i].set(v) {
                            if !u.user.is_empty() {
                                self.user = u.user;
                                self.kept_pw = u.password;
                            }
                        }
                    }
                    self.no_proxy = s.no_proxy.clone();
                }
                self.loaded = Some(l);
            }
        }
        if let Some(rx) = &self.test_rx {
            if let Ok(r) = rx.try_recv() {
                self.test_rx = None;
                self.tests = r;
            }
        }
        let mut reload = false;
        self.apply_rx.retain(|rx| match rx.try_recv() {
            Ok((daemon, r)) => {
                match r {
                    Ok(m) => {
                        n.ok(m);
                        if daemon {
                            self.restart_needed = true;
                        }
                    }
                    Err(e) => n.err(format!("Proxy change failed: {e}")),
                }
                reload = true;
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        if reload {
            self.loaded = None;
        }
    }

    fn load(&mut self, ctx: &egui::Context) {
        if self.load_rx.is_none() {
            self.load_rx = Some(spawn(ctx, || Loaded { daemon: config::read_daemon_json().map(|(m, _)| config::daemon_proxies(&m)), client: config::client_proxies(), dropins: config::systemd_proxy_dropins() }));
        }
    }

    fn urls(&self) -> Result<Vec<(usize, ProxyUrl)>, String> {
        let mut v = vec![];
        for (i, e) in self.e.iter().enumerate() {
            if !e.on {
                continue;
            }
            if !valid_host(e.host.trim()) {
                return Err(format!("{}: '{}' is not a valid host", NAMES[i], e.host.trim()));
            }
            let port = parse_port(&e.port).ok_or(format!("{}: port must be 1-65535", NAMES[i]))?;
            let pw = if self.password.is_empty() { self.kept_pw.clone() } else { self.password.clone() };
            v.push((i, ProxyUrl { scheme: e.scheme, host: e.host.trim().to_string(), port, user: self.user.trim().to_string(), password: if self.user.trim().is_empty() { String::new() } else { pw } }));
        }
        Ok(v)
    }

    fn proxy_set(&self) -> Result<ProxySet, String> {
        if !self.enabled {
            return Ok(ProxySet::default());
        }
        let mut s = ProxySet { no_proxy: self.no_proxy.split(',').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(","), ..Default::default() };
        for (i, u) in self.urls()? {
            *[&mut s.http, &mut s.https, &mut s.all][i] = u.to_url();
        }
        if s.http.is_empty() && s.https.is_empty() && s.all.is_empty() {
            return Err("Enable at least one proxy entry, or turn the proxy off".into());
        }
        Ok(s)
    }
}

fn show_set(ui: &mut Ui, title: &str, s: &Result<ProxySet, String>) {
    let p = pal(ui);
    ui.label(RichText::new(title).strong());
    match s {
        Ok(s) if s.is_empty() => {
            ui.label(RichText::new("no proxy configured").color(p.text_dim));
        }
        Ok(s) => {
            let m = s.masked();
            for (k, v) in [("HTTP_PROXY", m.http), ("HTTPS_PROXY", m.https), ("ALL_PROXY", m.all), ("NO_PROXY", m.no_proxy)] {
                if !v.is_empty() {
                    theme::kv(ui, k, v);
                }
            }
        }
        Err(e) => theme::status(ui, Kind::Error, e),
    }
}

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    if d.px.loaded.is_none() {
        d.px.load(&ctx);
    }
    if d.px.restart_needed {
        theme::container(ui, "Restart required", |ui| {
            theme::status(ui, Kind::Warning, "The daemon reads daemon.json only at start. The new proxy is used after Docker restarts.");
            if theme::primary_button(ui, "Restart Docker...").clicked() {
                super::daemon::ask_restart(d);
            }
        });
        ui.add_space(8.0);
    }
    theme::container(ui, "Current configuration", |ui| {
        let Some(l) = &d.px.loaded else {
            ui.spinner();
            return;
        };
        show_set(ui, "Daemon (/etc/docker/daemon.json) — pulls, pushes, registry logins", &l.daemon);
        if let Some(i) = &d.inv {
            if !i.info.http_proxy.is_empty() || !i.info.https_proxy.is_empty() {
                ui.label(RichText::new(format!("In effect now (reported by the daemon): http={} https={} no_proxy={}", i.info.http_proxy, i.info.https_proxy, i.info.no_proxy)).color(p.text_dim).small());
            }
        }
        ui.add_space(4.0);
        show_set(ui, &format!("Containers and builds ({})", config::client_config_path().display()), &l.client);
        if !l.dropins.is_empty() {
            ui.add_space(4.0);
            theme::note(ui, Kind::Warning, &format!("The Docker service also gets proxy variables from systemd drop-ins:\n{}\nConfigure the proxy in one place only; remove the drop-in (then `systemctl daemon-reload`) if you manage it here.", l.dropins.iter().map(|(f, x)| format!("{f}: {x}")).collect::<Vec<_>>().join("\n")));
        }
        if ui.small_button("Reload").clicked() {
            d.px.loaded = None;
        }
    });
    ui.add_space(8.0);
    let mut apply = false;
    let mut test = false;
    theme::container(ui, "Edit proxy", |ui| {
        let s = &mut d.px;
        ui.checkbox(&mut s.enabled, "Use a proxy (turn off to remove the proxy settings from the selected scopes)");
        ui.add_enabled_ui(s.enabled, |ui| {
            egui::Grid::new("dk_px").num_columns(5).spacing([10.0, 8.0]).show(ui, |ui| {
                for h in ["", "Variable", "Type", "Host", "Port"] {
                    ui.label(RichText::new(h).strong().color(p.text_dim));
                }
                ui.end_row();
                for (i, e) in s.e.iter_mut().enumerate() {
                    ui.checkbox(&mut e.on, "");
                    ui.label(NAMES[i]);
                    egui::ComboBox::from_id_salt(("dk_pxs", i)).selected_text(e.scheme.as_str()).width(80.0).show_ui(ui, |ui| {
                        for sc in ProxyScheme::ALL {
                            ui.selectable_value(&mut e.scheme, sc, sc.as_str());
                        }
                    });
                    ui.add_enabled(e.on, egui::TextEdit::singleline(&mut e.host).desired_width(220.0).hint_text("proxy.example.com"));
                    ui.add_enabled(e.on, egui::TextEdit::singleline(&mut e.port).desired_width(64.0).hint_text("3128"));
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                ui.label("NO_PROXY");
                ui.add(egui::TextEdit::singleline(&mut s.no_proxy).desired_width(420.0).hint_text("localhost,127.0.0.1,.corp.example,10.0.0.0/8"));
            });
            ui.horizontal(|ui| {
                ui.label("Authentication: user");
                ui.add(egui::TextEdit::singleline(&mut s.user).desired_width(140.0));
                ui.label("password");
                ui.add(egui::TextEdit::singleline(&mut s.password).password(true).desired_width(140.0).hint_text(if s.kept_pw.is_empty() { "" } else { "unchanged" }));
            });
            if !s.user.is_empty() {
                ui.label(RichText::new("Docker has no keyring for proxy passwords: they are stored in the configuration file (daemon.json is then made readable by root only). They are never shown here or written to logs.").color(p.text_dim).small());
            }
        });
        ui.separator();
        ui.label(RichText::new("Apply to").strong());
        ui.checkbox(&mut s.to_daemon, "Docker daemon — image pulls/pushes and registry access (needs administrator authentication and a daemon restart; ALL_PROXY is not supported by the daemon)");
        ui.checkbox(&mut s.to_client, "Containers and builds — variables the docker CLI injects into new containers and builds (your user, no restart)");
        ui.label(RichText::new("SOCKS5 works for the daemon and most tools; HTTPS means TLS to the proxy itself.").color(p.text_dim).small());
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Test through the proxy to");
            ui.add(egui::TextEdit::singleline(&mut s.test_host).desired_width(200.0));
            ui.label(":443");
            test = ui.add_enabled(s.enabled && s.test_rx.is_none(), egui::Button::new("Test connectivity")).clicked();
            if s.test_rx.is_some() {
                ui.spinner();
            }
        });
        for (k, r) in &s.tests {
            match r {
                Ok(m) => theme::status(ui, Kind::Success, &format!("{k}: {m}")),
                Err(e) => theme::status(ui, Kind::Error, &format!("{k}: {e}")),
            }
        }
        if let Some(e) = &s.error {
            theme::note(ui, Kind::Error, e);
        }
        ui.add_space(4.0);
        apply = ui.add_enabled((s.to_daemon || s.to_client) && s.apply_rx.is_empty(), egui::Button::new(RichText::new("Apply...").strong())).clicked();
    });
    if test {
        match d.px.urls() {
            Ok(urls) if !urls.is_empty() => {
                let host = d.px.test_host.trim().to_string();
                d.px.tests.clear();
                d.px.test_rx = Some(spawn(&ctx, move || urls.into_iter().map(|(i, u)| (format!("{} ({})", NAMES[i], u.masked()), config::test_proxy(&u, &host, 443))).collect()));
            }
            Ok(_) => d.px.error = Some("Enable at least one proxy entry to test".into()),
            Err(e) => d.px.error = Some(e),
        }
    }
    if apply {
        let set = match d.px.proxy_set() {
            Ok(s) => s,
            Err(e) => {
                d.px.error = Some(e);
                return;
            }
        };
        let (to_daemon, to_client) = (d.px.to_daemon, d.px.to_client);
        let daemon_set = ProxySet { all: String::new(), ..set.clone() };
        if let Err(e) = set.validate(false).and_then(|_| if to_daemon { daemon_set.validate(true) } else { Ok(()) }) {
            d.px.error = Some(e);
            return;
        }
        d.px.error = None;
        let m = set.masked();
        let mut lines = vec![if set.is_empty() { "Remove the proxy settings from:".to_string() } else { "Set:".to_string() }];
        if !set.is_empty() {
            for (k, v) in [("HTTP_PROXY", &m.http), ("HTTPS_PROXY", &m.https), ("ALL_PROXY", &m.all), ("NO_PROXY", &m.no_proxy)] {
                if !v.is_empty() {
                    lines.push(format!("  {k} = {v}"));
                }
            }
            lines.push("in:".into());
        }
        if to_daemon {
            lines.push(format!("• the Docker daemon ({}), merged with the existing settings{}", config::DAEMON_JSON, if set.all.is_empty() { "" } else { " (ALL_PROXY is left out: the daemon does not support it)" }));
        }
        if to_client {
            lines.push(format!("• {} (only the proxies.default entry changes)", config::client_config_path().display()));
        }
        if to_daemon {
            lines.push("A backup of daemon.json is taken first. Docker must be restarted afterwards.".into());
        }
        d.ask(Confirm {
            title: "Apply Docker proxy?".into(),
            lines,
            warning: if to_daemon { Some("Writing daemon.json asks for administrator authentication (PolicyKit).".into()) } else { None },
            button: "Apply".into(),
            danger: false,
            typed: None,
            input: String::new(),
            then: Box::new(move |d, ctx| {
                let summary = format!("http={} https={} all={} no_proxy={}", !set.http.is_empty(), !set.https.is_empty(), !set.all.is_empty(), !set.no_proxy.is_empty());
                if to_client {
                    let (s, sm) = (set.clone(), summary.clone());
                    d.px.apply_rx.push(spawn(ctx, move || {
                        let r = config::apply_client_proxies(&s);
                        crate::docker::audit::record("proxy: client config", &sm, &r);
                        (false, r)
                    }));
                }
                if to_daemon {
                    d.px.apply_rx.push(spawn(ctx, move || {
                        // merge into the file as it is now, not as it was when the page loaded
                        let r = config::read_daemon_json().and_then(|(cur, _)| {
                            let m = config::merge(&cur, &[("proxies", config::daemon_proxy_change(&daemon_set))]);
                            config::apply_daemon_json(&config::pretty(&m), "proxy change")
                        });
                        crate::docker::audit::record("proxy: daemon.json", &summary, &r);
                        (true, r)
                    }));
                }
                d.px.password.clear();
            }),
        });
    }
}
