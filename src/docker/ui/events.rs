//! Events & monitoring: the engine's live event stream with filters, and resource usage over time.
use super::{hscroll, DockerUi};
use crate::charts::{line_chart, Series};
use crate::docker::api::Event;
use crate::linux::{fmt_ts, human_bytes, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};

#[derive(Default)]
pub(super) struct State {
    kind: String,
    action: String,
    container: String,
    paused: Option<Vec<Event>>,
}

const KINDS: [&str; 9] = ["", "container", "image", "volume", "network", "daemon", "plugin", "builder", "service"];

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let p = pal(ui);
    let Some(live) = d.live.as_ref().map(|l| l.snapshot()) else { return };
    theme::card_grid(ui, 380.0, 2, |ui, k| {
        if k == 0 {
            theme::container(ui, "CPU over time (all containers)", |ui| {
                line_chart(ui, 110.0, super::super::live::HIST, &[Series { name: "", color: p.series[0], data: &live.total_cpu }], None, &|v| format!("{v:.0}%"));
            });
        } else {
            theme::container(ui, "Memory over time (all containers)", |ui| {
                line_chart(ui, 110.0, super::super::live::HIST, &[Series { name: "", color: p.series[2], data: &live.total_mem }], None, &|v| human_bytes(v as u64));
            });
        }
    });
    ui.label(RichText::new("Sampled every 2 s while a Docker page is open (about 5 minutes of history).").color(p.text_dim).small());
    ui.add_space(6.0);
    let mut clear = false;
    theme::container(ui, "Events", |ui| {
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("dk_evkind").selected_text(if d.ev.kind.is_empty() { "all types".to_string() } else { d.ev.kind.clone() }).show_ui(ui, |ui| {
                for k in KINDS {
                    ui.selectable_value(&mut d.ev.kind, k.to_string(), if k.is_empty() { "all types" } else { k });
                }
            });
            ui.add(egui::TextEdit::singleline(&mut d.ev.action).hint_text("action (start, die, pull, destroy...)").desired_width(200.0));
            ui.add(egui::TextEdit::singleline(&mut d.ev.container).hint_text("container / object name or ID").desired_width(200.0));
            let paused = d.ev.paused.is_some();
            if ui.button(if paused { "Resume live updates" } else { "Pause" }).clicked() {
                d.ev.paused = if paused { None } else { Some(live.events.iter().cloned().collect()) };
            }
            clear = ui.button("Clear").clicked();
        });
        match &live.events_error {
            Some(e) => theme::status(ui, Kind::Warning, e),
            None => theme::status(ui, if d.ev.paused.is_some() { Kind::Inactive } else { Kind::Success }, if d.ev.paused.is_some() { "paused" } else { "live" }),
        }
        let src: Vec<&Event> = match &d.ev.paused {
            Some(v) => v.iter().collect(),
            None => live.events.iter().collect(),
        };
        let (k, a, c) = (d.ev.kind.as_str(), d.ev.action.to_lowercase(), d.ev.container.to_lowercase());
        let rows: Vec<&Event> = src
            .into_iter()
            .rev()
            .filter(|e| k.is_empty() || e.kind == k)
            .filter(|e| a.is_empty() || e.action.to_lowercase().contains(&a))
            .filter(|e| c.is_empty() || e.name().to_lowercase().contains(&c) || e.actor.id.starts_with(&c) || e.actor.attributes.get("container").map(|x| x.starts_with(&c)).unwrap_or(false))
            .collect();
        ui.label(RichText::new(format!("{} events (newest first; the last hour is replayed from the engine at start)", rows.len())).color(p.text_dim).small());
        hscroll(ui, "dk_evtable", |ui| {
            TableBuilder::new(ui)
                .id_salt("dk_events")
                .striped(true)
                .max_scroll_height(420.0)
                .min_scrolled_height(120.0)
                .column(Column::exact(150.0))
                .column(Column::exact(90.0))
                .column(Column::exact(140.0))
                .column(Column::initial(200.0).clip(true))
                .column(Column::initial(360.0).clip(true))
                .header(24.0, |mut h| {
                    for t in ["Time (UTC)", "Type", "Action", "Object", "Details"] {
                        h.col(|ui| {
                            ui.label(RichText::new(t).strong().color(p.text_dim));
                        });
                    }
                })
                .body(|body| {
                    body.rows(22.0, rows.len(), |mut row| {
                        let e = rows[row.index()];
                        row.col(|ui| {
                            ui.label(fmt_ts(e.time.max(0) as u64));
                        });
                        row.col(|ui| {
                            ui.label(&e.kind);
                        });
                        row.col(|ui| {
                            let kd = match e.action.split(':').next().unwrap_or("") {
                                "die" | "kill" | "oom" | "destroy" | "delete" | "untag" => Kind::Warning,
                                "start" | "create" | "pull" | "unpause" => Kind::Success,
                                _ => Kind::Info,
                            };
                            super::kind_text(ui, if e.action.contains("unhealthy") || e.action == "oom" { Kind::Error } else { kd }, &e.action);
                        });
                        row.col(|ui| {
                            ui.add(egui::Label::new(e.name()).truncate());
                        });
                        row.col(|ui| {
                            let mut attrs: Vec<String> = e.actor.attributes.iter().filter(|(k, _)| !k.starts_with("com.docker.compose") && *k != "name").map(|(k, v)| format!("{k}={}", crate::docker::audit::scrub(v))).collect();
                            attrs.sort();
                            ui.add(egui::Label::new(RichText::new(attrs.join(" ")).color(p.text_dim).size(12.0)).truncate());
                        });
                    });
                });
        });
    });
    if clear {
        if let Some(l) = &d.live {
            l.clear_events();
        }
        d.ev.paused = None;
    }
}
