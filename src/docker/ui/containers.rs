//! Containers: searchable/sortable list, lifecycle actions, details, live logs, exec terminal.
use super::{age, hscroll, mask_env, matches, ord, search, sort_header, state_kind, Confirm, DockerUi};
use crate::charts::{line_chart, Series};
use crate::docker::api::{parse_exec_cmd, short_id, Container};
use crate::docker::http::{self, Canceller};
use crate::linux::{human_bytes, human_rate, spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Image,
    State,
    Created,
    Cpu,
    Mem,
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Running,
    Paused,
    Stopped,
}

#[derive(Clone, Copy, PartialEq)]
enum Detail {
    Summary,
    Env,
    Ports,
    Stats,
    Logs,
    Terminal,
    Inspect,
}

const LOG_LINES: usize = 5000;
const TERM_BYTES: usize = 256 << 10;

enum LogMsg {
    Text(String),
    End(String),
}

struct LogSession {
    id: String,
    rx: Receiver<LogMsg>,
    cancel: Arc<Mutex<Option<Canceller>>>,
    lines: VecDeque<String>,
    partial: String,
    ended: Option<String>,
}

impl Drop for LogSession {
    fn drop(&mut self) {
        if let Some(c) = self.cancel.lock().unwrap_or_else(|e| e.into_inner()).take() {
            c.cancel();
        }
    }
}

enum TermMsg {
    Bytes(Vec<u8>),
    End(String),
}

struct TermSession {
    container: String,
    writer: std::os::unix::net::UnixStream,
    rx: Receiver<TermMsg>,
    raw: Vec<u8>,
    strip: AnsiStrip,
    text: String,
    input: String,
    ended: Option<String>,
}

impl Drop for TermSession {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }
}

pub(super) struct State {
    q: String,
    filter: Filter,
    sort: (Col, bool),
    selected: Option<String>,
    detail: Detail,
    inspect: Option<(String, Value)>,
    inspect_rx: Option<Receiver<(String, Result<Value, String>)>>,
    reveal_env: bool,
    logs: Option<LogSession>,
    log_tail: u32,
    log_ts: bool,
    log_q: String,
    term: Option<TermSession>,
    term_rx: Option<Receiver<Result<TermSession, String>>>,
    term_err: Option<String>,
    term_cmd: String,
}

impl Default for State {
    fn default() -> Self {
        Self { q: String::new(), filter: Filter::All, sort: (Col::Name, true), selected: None, detail: Detail::Summary, inspect: None, inspect_rx: None, reveal_env: false, logs: None, log_tail: 500, log_ts: false, log_q: String::new(), term: None, term_rx: None, term_err: None, term_cmd: "/bin/sh".into() }
    }
}

impl State {
    pub(super) fn poll(&mut self, _ctx: &egui::Context) {
        if let Some(rx) = &self.inspect_rx {
            if let Ok((id, r)) = rx.try_recv() {
                self.inspect_rx = None;
                self.inspect = Some((id, r.unwrap_or_else(|e| Value::String(format!("error: {e}")))));
            }
        }
        if let Some(l) = &mut self.logs {
            while let Ok(m) = l.rx.try_recv() {
                match m {
                    LogMsg::Text(t) => {
                        l.partial.push_str(&t);
                        while let Some(i) = l.partial.find('\n') {
                            let line: String = l.partial.drain(..=i).collect();
                            l.lines.push_back(line.trim_end_matches(['\n', '\r']).to_string());
                        }
                        while l.lines.len() > LOG_LINES {
                            l.lines.pop_front();
                        }
                    }
                    LogMsg::End(e) => l.ended = Some(e),
                }
            }
        }
        if let Some(rx) = &self.term_rx {
            if let Ok(r) = rx.try_recv() {
                self.term_rx = None;
                match r {
                    Ok(t) => {
                        self.term = Some(t);
                        self.term_err = None;
                    }
                    Err(e) => self.term_err = Some(e),
                }
            }
        }
        if let Some(t) = &mut self.term {
            let mut changed = false;
            while let Ok(m) = t.rx.try_recv() {
                match m {
                    TermMsg::Bytes(b) => {
                        t.strip.feed(&b, &mut t.raw);
                        changed = true;
                    }
                    TermMsg::End(e) => t.ended = Some(e),
                }
            }
            if t.raw.len() > TERM_BYTES {
                let cut = t.raw.len() - TERM_BYTES;
                t.raw.drain(..cut);
            }
            if changed {
                t.text = String::from_utf8_lossy(&t.raw).into_owned();
            }
        }
    }
}

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let Some(inv) = d.inv.clone() else {
        ui.spinner();
        return;
    };
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    let live = d.live.as_ref().map(|l| l.snapshot()).unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        search(ui, &mut d.c.q, "Search name, ID, image, port");
        for (f, l) in [(Filter::All, "All"), (Filter::Running, "Running"), (Filter::Paused, "Paused"), (Filter::Stopped, "Stopped")] {
            ui.selectable_value(&mut d.c.filter, f, l);
        }
    });
    let st = &d.c;
    let mut rows: Vec<&Container> = inv
        .containers
        .iter()
        .filter(|c| match st.filter {
            Filter::All => true,
            Filter::Running => c.is_running(),
            Filter::Paused => c.state == "paused",
            Filter::Stopped => c.is_stopped(),
        })
        .filter(|c| matches(&st.q, &[&c.name(), &c.id, &c.image, &c.ports_text(), &c.state]))
        .collect();
    let cpu = |c: &Container| live.stats.get(&c.id).map(|s| s.cpu_pct).unwrap_or(-1.0);
    let mem = |c: &Container| live.stats.get(&c.id).map(|s| s.mem).unwrap_or(0);
    rows.sort_by(|a, b| match st.sort.0 {
        Col::Name => ord(a.name().to_lowercase(), b.name().to_lowercase(), st.sort.1),
        Col::Image => ord(&a.image, &b.image, st.sort.1),
        Col::State => ord(&a.state, &b.state, st.sort.1),
        Col::Created => ord(a.created, b.created, st.sort.1),
        Col::Cpu => ord(cpu(a), cpu(b), st.sort.1),
        Col::Mem => ord(mem(a), mem(b), st.sort.1),
    });
    ui.label(RichText::new(format!("{} of {} containers", rows.len(), inv.containers.len())).color(p.text_dim).small());
    let mut clicked = None;
    hscroll(ui, "dk_ctable", |ui| {
        TableBuilder::new(ui)
            .id_salt("dk_containers")
            .striped(true)
            .sense(egui::Sense::click())
            .vscroll(true)
            .max_scroll_height(340.0)
            .min_scrolled_height(120.0)
            .column(Column::initial(170.0).at_least(110.0).clip(true))
            .column(Column::exact(110.0))
            .column(Column::initial(180.0).at_least(100.0).clip(true))
            .column(Column::exact(150.0))
            .column(Column::initial(160.0).clip(true))
            .column(Column::initial(170.0).clip(true))
            .column(Column::exact(100.0))
            .column(Column::exact(70.0))
            .column(Column::exact(90.0))
            .header(26.0, |mut h| {
                h.col(|ui| sort_header(ui, "Name", Col::Name, &mut d.c.sort));
                h.col(|ui| {
                    ui.label(RichText::new("ID").strong().color(p.text_dim));
                });
                h.col(|ui| sort_header(ui, "Image", Col::Image, &mut d.c.sort));
                h.col(|ui| sort_header(ui, "State", Col::State, &mut d.c.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Status / uptime").strong().color(p.text_dim));
                });
                h.col(|ui| {
                    ui.label(RichText::new("Ports").strong().color(p.text_dim));
                });
                h.col(|ui| sort_header(ui, "Created", Col::Created, &mut d.c.sort));
                h.col(|ui| sort_header(ui, "CPU", Col::Cpu, &mut d.c.sort));
                h.col(|ui| sort_header(ui, "Memory", Col::Mem, &mut d.c.sort));
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let c = rows[row.index()];
                    row.set_selected(d.c.selected.as_deref() == Some(c.id.as_str()));
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(c.name()).strong()).truncate());
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(short_id(&c.id)).monospace());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&c.image).truncate());
                    });
                    row.col(|ui| {
                        let h = c.health();
                        theme::status(ui, state_kind(&c.state, h), &if h.is_empty() { c.state.clone() } else { format!("{} ({h})", c.state) });
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&c.status).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(c.ports_text()).truncate());
                    });
                    row.col(|ui| {
                        ui.label(age(c.created));
                    });
                    row.col(|ui| {
                        ui.label(live.stats.get(&c.id).map(|s| format!("{:.1}%", s.cpu_pct)).unwrap_or_else(|| "—".into()));
                    });
                    row.col(|ui| {
                        ui.label(live.stats.get(&c.id).map(|s| human_bytes(s.mem)).unwrap_or_else(|| "—".into()));
                    });
                    if row.response().clicked() {
                        clicked = Some(c.id.clone());
                    }
                });
            });
    });
    if let Some(id) = clicked {
        if d.c.selected.as_deref() != Some(&id) {
            d.c.selected = Some(id);
            d.c.inspect = None;
            d.c.logs = None;
        }
    }
    let Some(sel) = d.c.selected.clone().and_then(|id| inv.containers.iter().find(|c| c.id == id).cloned()) else {
        ui.label(RichText::new("Select a container for actions, logs, statistics and a terminal.").color(p.text_dim));
        return;
    };
    ui.add_space(6.0);
    actions(d, ui, &sel);
    ui.add_space(6.0);
    if d.c.inspect.as_ref().map(|(i, _)| i != &sel.id).unwrap_or(true) && d.c.inspect_rx.is_none() {
        if let Some(e) = d.engine() {
            let id = sel.id.clone();
            d.c.inspect_rx = Some(spawn(&ctx, move || (id.clone(), e.inspect_container(&id).map_err(|e| e.to_string()))));
        }
    }
    theme::container(ui, &format!("Container {}", sel.name()), |ui| {
        ui.horizontal_wrapped(|ui| {
            for (t, l) in [(Detail::Summary, "Summary"), (Detail::Env, "Environment"), (Detail::Ports, "Ports, mounts, networks"), (Detail::Stats, "Statistics"), (Detail::Logs, "Logs"), (Detail::Terminal, "Terminal"), (Detail::Inspect, "Inspect")] {
                ui.selectable_value(&mut d.c.detail, t, l);
            }
        });
        ui.separator();
        let insp = d.c.inspect.as_ref().filter(|(i, _)| *i == sel.id).map(|(_, v)| v.clone());
        match d.c.detail {
            Detail::Summary => summary(ui, &sel, insp.as_ref()),
            Detail::Env => env(d, ui, insp.as_ref()),
            Detail::Ports => ports(ui, &sel, insp.as_ref()),
            Detail::Stats => stats(d, ui, &sel),
            Detail::Logs => logs(d, ui, &sel),
            Detail::Terminal => terminal(d, ui, &sel),
            Detail::Inspect => match insp {
                Some(v) => {
                    // environment values can hold secrets: masked here too unless revealed
                    let mut v = v.clone();
                    if let Some(a) = v.pointer_mut("/Config/Env").and_then(Value::as_array_mut) {
                        for e in a.iter_mut() {
                            *e = Value::String(mask_env(e.as_str().unwrap_or_default(), d.c.reveal_env));
                        }
                    }
                    ui.checkbox(&mut d.c.reveal_env, "Show secret-looking environment values");
                    super::json_box(ui, "dk_cinspect", &v);
                }
                None => {
                    ui.spinner();
                }
            },
        }
    });
}

fn actions(d: &mut DockerUi, ui: &mut Ui, c: &Container) {
    let ctx = ui.ctx().clone();
    let (id, name) = (c.id.clone(), c.name());
    let mut act: Option<&'static str> = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&name).strong());
        let running = c.is_running();
        let paused = c.state == "paused";
        if ui.add_enabled(!running && !paused, egui::Button::new("Start")).clicked() {
            act = Some("start");
        }
        if ui.add_enabled(running || paused, egui::Button::new("Stop")).clicked() {
            act = Some("stop");
        }
        if ui.add_enabled(running || paused || c.is_stopped(), egui::Button::new("Restart")).clicked() {
            act = Some("restart");
        }
        if paused {
            if ui.button("Unpause").clicked() {
                act = Some("unpause");
            }
        } else if ui.add_enabled(running, egui::Button::new("Pause")).clicked() {
            act = Some("pause");
        }
        if ui.add_enabled(!running && !paused, egui::Button::new("Remove")).on_hover_text("Remove the stopped container (its volumes are kept)").clicked() {
            act = Some("remove");
        }
        if theme::danger_button(ui, "Force remove").on_hover_text("Kill and remove, even if running").clicked() {
            act = Some("force");
        }
    });
    match act {
        Some(a @ ("start" | "unpause")) => {
            let (i, n) = (id.clone(), name.clone());
            d.run(&ctx, format!("{a} {name}"), Some((format!("container {a}"), name.clone())), move |e| e.container_action(&i, a).map(|_| format!("{n}: {a}ed")).map_err(|e| e.to_string()));
        }
        Some(a @ ("stop" | "restart" | "pause")) => {
            let n = name.clone();
            d.ask(Confirm {
                title: format!("{} container?", capital(a)),
                lines: vec![format!("{} \"{name}\" ({}).", capital(a), short_id(&id)), if a == "pause" { "All processes are frozen until unpaused.".into() } else { "The main process gets SIGTERM and up to 10 seconds to exit before it is killed.".into() }],
                warning: None,
                button: capital(a),
                danger: a == "stop",
                typed: None,
                input: String::new(),
                then: Box::new(move |d, ctx| {
                    let (i, n2) = (id.clone(), n.clone());
                    d.run(ctx, format!("{a} {n}"), Some((format!("container {a}"), n.clone())), move |e| e.container_action(&i, a).map(|_| format!("{n2}: {a} done")).map_err(|e| e.to_string()));
                }),
            });
        }
        Some(a @ ("remove" | "force")) => {
            let force = a == "force";
            let n = name.clone();
            let mut lines = vec![format!("Remove container \"{name}\" ({}) created from {}.", short_id(&id), c.image), "Its writable layer is deleted. Named and anonymous volumes are kept (remove them on the Volumes page).".into()];
            if force && c.is_running() {
                lines.push("The container is RUNNING: it is killed immediately (SIGKILL) and removed.".into());
            }
            d.ask(Confirm {
                title: if force { "Force remove container?".into() } else { "Remove container?".into() },
                lines,
                warning: if force { Some("Force removal cannot be undone.".into()) } else { None },
                button: "Remove".into(),
                danger: true,
                typed: if force && (c.is_running() || c.state == "paused") { Some(name.clone()) } else { None },
                input: String::new(),
                then: Box::new(move |d, ctx| {
                    let (i, n2) = (id.clone(), n.clone());
                    d.c.selected = None;
                    d.run(ctx, format!("remove {n}"), Some((if force { "container force remove".into() } else { "container remove".into() }, n.clone())), move |e| e.remove_container(&i, force).map(|_| format!("Container {n2} removed")).map_err(|e| e.to_string()));
                }),
            });
        }
        _ => {}
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn s(v: &Value, ptr: &str) -> String {
    match v.pointer(ptr) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(x) => x.to_string(),
    }
}

fn summary(ui: &mut Ui, c: &Container, v: Option<&Value>) {
    theme::kv(ui, "ID", c.id.clone());
    theme::kv(ui, "Image", format!("{} ({})", c.image, short_id(&c.image_id)));
    theme::kv(ui, "Command", c.command.clone());
    theme::kv(ui, "State", c.status.clone());
    let Some(v) = v else {
        ui.spinner();
        return;
    };
    theme::kv(ui, "Started", s(v, "/State/StartedAt"));
    theme::kv(ui, "Restart policy", format!("{} (restarted {} times)", s(v, "/HostConfig/RestartPolicy/Name"), s(v, "/RestartCount")));
    theme::kv(ui, "Exit code", s(v, "/State/ExitCode"));
    if !s(v, "/State/Error").is_empty() {
        theme::status(ui, Kind::Error, &s(v, "/State/Error"));
    }
    if v.pointer("/State/OOMKilled").and_then(Value::as_bool) == Some(true) {
        theme::status(ui, Kind::Error, "Killed by the kernel: out of memory");
    }
    theme::kv(ui, "TTY", s(v, "/Config/Tty"));
    let lim = v.pointer("/HostConfig/Memory").and_then(Value::as_u64).unwrap_or(0);
    theme::kv(ui, "Memory limit", if lim == 0 { "none".into() } else { human_bytes(lim) });
    let cpus = v.pointer("/HostConfig/NanoCpus").and_then(Value::as_u64).unwrap_or(0);
    theme::kv(ui, "CPU limit", if cpus == 0 { "none".into() } else { format!("{:.2} CPUs", cpus as f64 / 1e9) });
    match v.pointer("/State/Health") {
        Some(h) if !h.is_null() => {
            let st = s(h, "/Status");
            theme::status(ui, state_kind("running", &st), &format!("Health: {st} (failing streak {})", s(h, "/FailingStreak")));
            if let Some(log) = h.get("Log").and_then(Value::as_array) {
                for e in log.iter().rev().take(3) {
                    ui.label(RichText::new(format!("{}  exit {}  {}", s(e, "/End"), s(e, "/ExitCode"), s(e, "/Output").trim())).monospace().size(12.0));
                }
            }
        }
        _ => theme::kv(ui, "Health check", "none configured"),
    }
}

fn env(d: &mut DockerUi, ui: &mut Ui, v: Option<&Value>) {
    let Some(v) = v else {
        ui.spinner();
        return;
    };
    ui.checkbox(&mut d.c.reveal_env, "Show values of secret-looking variables (PASSWORD, TOKEN, KEY, ...)");
    let list: Vec<String> = v.pointer("/Config/Env").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|e| mask_env(e, d.c.reveal_env)).collect()).unwrap_or_default();
    if list.is_empty() {
        ui.label("No environment variables.");
    }
    egui::ScrollArea::vertical().id_salt("dk_env").max_height(320.0).show(ui, |ui| {
        for e in list {
            ui.add(egui::Label::new(RichText::new(e).monospace().size(12.5)).wrap());
        }
    });
}

fn ports(ui: &mut Ui, c: &Container, v: Option<&Value>) {
    let p = pal(ui);
    ui.label(RichText::new("Port mappings").strong());
    if c.ports.is_empty() {
        ui.label(RichText::new("none published").color(p.text_dim));
    }
    for pt in &c.ports {
        ui.label(if pt.public_port > 0 { format!("{}:{} → container {}/{}", if pt.ip.is_empty() { "*" } else { &pt.ip }, pt.public_port, pt.private_port, pt.kind) } else { format!("{}/{} (exposed, not published)", pt.private_port, pt.kind) });
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Mounts").strong());
    let mounts = v.and_then(|v| v.get("Mounts")).and_then(Value::as_array).cloned().unwrap_or_default();
    if mounts.is_empty() {
        ui.label(RichText::new("none").color(p.text_dim));
    }
    egui::Grid::new("dk_mounts").striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
        for m in &mounts {
            ui.label(s(m, "/Type"));
            ui.label(if s(m, "/Name").is_empty() { s(m, "/Source") } else { s(m, "/Name") });
            ui.label(format!("→ {}", s(m, "/Destination")));
            ui.label(if m.get("RW").and_then(Value::as_bool) == Some(false) { "read-only" } else { "read-write" });
            ui.end_row();
        }
    });
    ui.add_space(6.0);
    ui.label(RichText::new("Networks").strong());
    for (n, ep) in &c.network_settings.networks {
        ui.label(format!("{n}: {}", if ep.ip_address.is_empty() { "no address".to_string() } else { ep.ip_address.clone() }));
    }
}

fn stats(d: &mut DockerUi, ui: &mut Ui, c: &Container) {
    let p = pal(ui);
    let Some(l) = d.live.as_ref().map(|l| l.snapshot()) else { return };
    let (Some(s), Some(h)) = (l.stats.get(&c.id), l.hist.get(&c.id)) else {
        ui.label(if c.is_running() { "Collecting statistics..." } else { "Statistics are only available for running containers." });
        return;
    };
    theme::card_grid(ui, 300.0, 4, |ui, k| match k {
        0 => theme::container(ui, "CPU", |ui| {
            theme::big_value(ui, "100% = one core", &format!("{:.1}%", s.cpu_pct), None);
            line_chart(ui, 80.0, super::super::live::HIST, &[Series { name: "", color: p.series[0], data: &h.cpu }], None, &|v| format!("{v:.0}%"));
        }),
        1 => theme::container(ui, "Memory", |ui| {
            theme::big_value(ui, &if s.mem_limit > 0 { format!("limit {}", human_bytes(s.mem_limit)) } else { String::new() }, &human_bytes(s.mem), None);
            line_chart(ui, 80.0, super::super::live::HIST, &[Series { name: "", color: p.series[2], data: &h.mem }], None, &|v| human_bytes(v as u64));
        }),
        2 => theme::container(ui, "Network I/O", |ui| {
            theme::big_value(ui, &format!("total in {} / out {}", human_bytes(s.net_rx), human_bytes(s.net_tx)), &human_rate(s.net_rx_bps + s.net_tx_bps), None);
            line_chart(ui, 80.0, super::super::live::HIST, &[Series { name: "in", color: p.series[0], data: &h.net_rx }, Series { name: "out", color: p.series[1], data: &h.net_tx }], None, &|v| human_rate(v as f64));
        }),
        _ => theme::container(ui, "Disk I/O", |ui| {
            theme::big_value(ui, &format!("total read {} / write {}", human_bytes(s.blk_read), human_bytes(s.blk_write)), &human_rate(s.blk_read_bps + s.blk_write_bps), None);
            line_chart(ui, 80.0, super::super::live::HIST, &[Series { name: "read", color: p.series[0], data: &h.blk_r }, Series { name: "write", color: p.series[1], data: &h.blk_w }], None, &|v| human_rate(v as f64));
        }),
    });
    ui.label(RichText::new(format!("{} processes · sampled every 2 s while this page is open", s.pids)).color(p.text_dim).small());
}

fn start_logs(d: &mut DockerUi, ctx: &egui::Context, id: &str) {
    let Some(e) = d.engine() else { return };
    let (tx, rx) = channel();
    let cancel: Arc<Mutex<Option<Canceller>>> = Arc::default();
    let (c2, id2, tail, ts, ctx2) = (cancel.clone(), id.to_string(), d.c.log_tail, d.c.log_ts, ctx.clone());
    std::thread::Builder::new()
        .name("docker-logs".into())
        .spawn(move || {
            let end = match e.logs(&id2, tail, true, ts) {
                Ok(mut s) => {
                    if let Ok(c) = s.canceller() {
                        *c2.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
                    }
                    // TTY containers send raw text, the others Docker's 8-byte framed stdout/stderr
                    let multiplexed = s.reader.fill_buf().map(|b| b.len() >= 8 && b[0] <= 2 && b[1..4] == [0, 0, 0]).unwrap_or(false);
                    loop {
                        let chunk = if multiplexed {
                            match http::read_frame(&mut s.reader) {
                                Ok(Some((_, p))) => p,
                                _ => break,
                            }
                        } else {
                            let mut b = vec![0u8; 8192];
                            match s.reader.read(&mut b) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    b.truncate(n);
                                    b
                                }
                            }
                        };
                        if tx.send(LogMsg::Text(String::from_utf8_lossy(&chunk).into_owned())).is_err() {
                            return;
                        }
                        ctx2.request_repaint();
                    }
                    "log stream ended (container stopped or stream closed)".to_string()
                }
                Err(err) => format!("cannot read logs: {err}"),
            };
            let _ = tx.send(LogMsg::End(end));
            ctx2.request_repaint();
        })
        .expect("spawn");
    d.c.logs = Some(LogSession { id: id.to_string(), rx, cancel, lines: VecDeque::new(), partial: String::new(), ended: None });
}

fn logs(d: &mut DockerUi, ui: &mut Ui, c: &Container) {
    let ctx = ui.ctx().clone();
    let p = pal(ui);
    let mut restart = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Last");
        egui::ComboBox::from_id_salt("dk_logtail").selected_text(d.c.log_tail.to_string()).width(70.0).show_ui(ui, |ui| {
            for n in [100, 500, 2000, 5000] {
                restart |= ui.selectable_value(&mut d.c.log_tail, n, n.to_string()).changed();
            }
        });
        ui.label("lines");
        restart |= ui.checkbox(&mut d.c.log_ts, "Timestamps").changed();
        restart |= ui.button("Reload").clicked();
        search(ui, &mut d.c.log_q, "Filter lines");
    });
    if restart || d.c.logs.as_ref().map(|l| l.id != c.id).unwrap_or(true) {
        start_logs(d, &ctx, &c.id);
    }
    let Some(l) = &d.c.logs else { return };
    let q = d.c.log_q.to_lowercase();
    let shown: Vec<&String> = l.lines.iter().filter(|x| q.is_empty() || x.to_lowercase().contains(&q)).collect();
    egui::Frame::new().fill(p.container_alt).corner_radius(4).inner_margin(8).show(ui, |ui| {
        egui::ScrollArea::both().id_salt("dk_logs").max_height(420.0).stick_to_bottom(true).auto_shrink([false, true]).show_rows(ui, 16.0, shown.len(), |ui, range| {
            for line in &shown[range] {
                ui.label(RichText::new(line.as_str()).monospace().size(12.5));
            }
        });
    });
    ui.label(RichText::new(match &l.ended {
        Some(e) => format!("{} lines · {e}", l.lines.len()),
        None => format!("{} lines · following live", l.lines.len()),
    }).color(p.text_dim).small());
}

fn terminal(d: &mut DockerUi, ui: &mut Ui, c: &Container) {
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    if !c.is_running() {
        ui.label("The container must be running to open a terminal.");
        return;
    }
    if d.c.term.as_ref().map(|t| t.container != c.id).unwrap_or(false) {
        d.c.term = None;
    }
    if d.c.term.is_none() {
        ui.horizontal(|ui| {
            ui.label("Command");
            ui.add(egui::TextEdit::singleline(&mut d.c.term_cmd).desired_width(220.0).hint_text("/bin/sh"));
            for sh in ["/bin/sh", "/bin/bash", "/bin/ash"] {
                if ui.small_button(sh).clicked() {
                    d.c.term_cmd = sh.into();
                }
            }
        });
        ui.label(RichText::new("Runs inside the container through the Engine API (no shell on the host is involved). The command line is split on spaces. The session is recorded in the audit log.").color(p.text_dim).small());
        if let Some(e) = &d.c.term_err {
            theme::status(ui, Kind::Error, &format!("Could not start the command: {e}"));
        }
        if d.c.term_rx.is_some() {
            ui.spinner();
        } else if theme::primary_button(ui, "Open terminal").clicked() {
            match (parse_exec_cmd(&d.c.term_cmd), d.engine()) {
                (Ok(cmd), Some(e)) => {
                    let (id, name) = (c.id.clone(), c.name());
                    let ctx2 = ctx.clone();
                    d.c.term_rx = Some(spawn(&ctx, move || {
                        let r = open_exec(&e, &id, &cmd, ctx2);
                        crate::docker::audit::record("container exec", &format!("{name}: {}", cmd.join(" ")), &r.as_ref().map(|_| ()));
                        r
                    }));
                }
                (Err(e), _) => d.toast(Kind::Error, e.to_string()),
                _ => {}
            }
        }
        return;
    }
    let mut close = false;
    if let Some(t) = &mut d.c.term {
        egui::Frame::new().fill(p.container_alt).corner_radius(4).inner_margin(8).show(ui, |ui| {
            egui::ScrollArea::both().id_salt("dk_term").max_height(420.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(t.text.as_str()).monospace().size(12.5)).selectable(true));
            });
        });
        if let Some(e) = &t.ended {
            theme::status(ui, Kind::Inactive, e);
        }
        ui.horizontal(|ui| {
            let r = ui.add_enabled(t.ended.is_none(), egui::TextEdit::singleline(&mut t.input).desired_width(ui.available_width() - 260.0).font(egui::TextStyle::Monospace).hint_text("type a command, Enter to send"));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter || ui.add_enabled(t.ended.is_none(), egui::Button::new("Send")).clicked() {
                let line = std::mem::take(&mut t.input) + "\n";
                let _ = t.writer.write_all(line.as_bytes());
                r.request_focus();
            }
            if ui.button("Ctrl+C").clicked() {
                let _ = t.writer.write_all(&[3]);
            }
            if ui.button("Ctrl+D").clicked() {
                let _ = t.writer.write_all(&[4]);
            }
            if ui.button("Close").clicked() {
                close = true;
            }
        });
    }
    if close {
        d.c.term = None;
    }
}

fn open_exec(e: &crate::docker::api::Engine, id: &str, cmd: &[String], ctx: egui::Context) -> Result<TermSession, String> {
    let exec = e.exec_create(id, cmd, true).map_err(|e| e.to_string())?;
    let (mut reader, writer) = e.exec_start(&exec, true).map_err(|e| e.to_string())?;
    let _ = e.exec_resize(&exec, 160, 40);
    let (tx, rx) = channel();
    let e2 = e.clone();
    std::thread::Builder::new()
        .name("docker-exec".into())
        .spawn(move || {
            let mut b = vec![0u8; 8192];
            loop {
                match reader.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(TermMsg::Bytes(b[..n].to_vec())).is_err() {
                            return;
                        }
                        ctx.request_repaint();
                    }
                }
            }
            let code = e2.exec_exit_code(&exec).ok().flatten();
            let _ = tx.send(TermMsg::End(format!("session ended{}", code.map(|c| format!(" (exit code {c})")).unwrap_or_default())));
            ctx.request_repaint();
        })
        .map_err(|e| e.to_string())?;
    Ok(TermSession { container: id.to_string(), writer, rx, raw: vec![], strip: AnsiStrip::default(), text: String::new(), input: String::new(), ended: None })
}

/// Removes terminal control sequences (colours, cursor movement, titles) so TTY output reads as text.
#[derive(Default)]
pub(crate) struct AnsiStrip {
    st: u8, // 0 text, 1 after ESC, 2 CSI, 3 OSC, 4 OSC after ESC
}

impl AnsiStrip {
    pub(crate) fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match (self.st, b) {
                (0, 0x1b) => self.st = 1,
                (0, b'\r' | 0x07) => {}
                (0, 0x08) => {
                    // backspace: drop the previous (possibly multi-byte) character
                    while let Some(x) = out.pop() {
                        if x & 0xC0 != 0x80 {
                            break;
                        }
                    }
                }
                (0, _) => out.push(b),
                (1, b'[') => self.st = 2,
                (1, b']') => self.st = 3,
                (1, _) => self.st = 0,
                (2, 0x40..=0x7e) => self.st = 0,
                (2, _) => {}
                (3, 0x07) => self.st = 0,
                (3, 0x1b) => self.st = 4,
                (3, _) => {}
                (4, _) => self.st = 0,
                _ => self.st = 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ansi_is_stripped_across_chunks() {
        let mut s = super::AnsiStrip::default();
        let mut out = vec![];
        s.feed(b"\x1b[1;32mgreen\x1b[0m ok\r\n\x1b]0;title\x07$ ", &mut out);
        s.feed(b"ab\x08c \x1b[", &mut out);
        s.feed(b"2Kdone\n", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "green ok\n$ ac done\n");
    }
}
