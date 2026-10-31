//! Volumes: list with size and users, unused filter, create, inspect, protected removal.
use super::{hscroll, matches, ord, search, sort_header, Confirm, DockerUi};
use crate::docker::api::{short_id, valid_object_name};
use crate::linux::{human_bytes, spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use serde_json::Value;
use std::sync::mpsc::Receiver;

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Size,
    Users,
}

pub(super) struct State {
    q: String,
    sort: (Col, bool),
    unused_only: bool,
    selected: Option<String>,
    new_name: String,
    new_driver: String,
    inspect: Option<(String, Value)>,
    inspect_rx: Option<Receiver<(String, Value)>>,
}

impl Default for State {
    fn default() -> Self {
        Self { q: String::new(), sort: (Col::Size, false), unused_only: false, selected: None, new_name: String::new(), new_driver: "local".into(), inspect: None, inspect_rx: None }
    }
}

impl State {
    pub(super) fn poll(&mut self) {
        if let Some(rx) = &self.inspect_rx {
            if let Ok(v) = rx.try_recv() {
                self.inspect = Some(v);
                self.inspect_rx = None;
            }
        }
    }
}

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let Some(inv) = d.inv.clone() else {
        ui.spinner();
        return;
    };
    let ctx = ui.ctx().clone();
    if d.df_stale {
        d.refresh_df(&ctx);
    }
    let p = pal(ui);
    let users = inv.volume_users();
    // sizes are only measured by /system/df
    let sizes: std::collections::HashMap<String, Option<u64>> = d.df.as_ref().map(|df| df.volumes.iter().map(|v| (v.name.clone(), v.size())).collect()).unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        search(ui, &mut d.vol.q, "Search volumes");
        ui.checkbox(&mut d.vol.unused_only, "Unused only");
        ui.add_space(12.0);
        ui.label("New volume");
        ui.add(egui::TextEdit::singleline(&mut d.vol.new_name).desired_width(160.0).hint_text("name"));
        ui.add(egui::TextEdit::singleline(&mut d.vol.new_driver).desired_width(70.0));
        let ok = valid_object_name(d.vol.new_name.trim()) && d.vol.new_name.trim().len() >= 2 && valid_object_name(d.vol.new_driver.trim());
        if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
            let (n, dr) = (d.vol.new_name.trim().to_string(), d.vol.new_driver.trim().to_string());
            let m = format!("Volume {n} created");
            d.run(&ctx, format!("create volume {n}"), Some(("volume create".into(), n.clone())), move |e| e.create_volume(&n, &dr).map(|_| m).map_err(|e| e.to_string()));
            d.vol.new_name.clear();
        }
    });
    let size = |n: &str| sizes.get(n).copied().flatten();
    let nusers = |n: &str| users.get(n).map(|v| v.len()).unwrap_or(0);
    let mut rows: Vec<_> = inv.volumes.iter().filter(|v| matches(&d.vol.q, &[&v.name, &v.driver, &v.mountpoint])).filter(|v| !d.vol.unused_only || nusers(&v.name) == 0).collect();
    let s = d.vol.sort;
    rows.sort_by(|a, b| match s.0 {
        Col::Name => ord(&a.name, &b.name, s.1),
        Col::Size => ord(size(&a.name).unwrap_or(0), size(&b.name).unwrap_or(0), s.1),
        Col::Users => ord(nusers(&a.name), nusers(&b.name), s.1),
    });
    ui.label(RichText::new(format!("{} volumes, {} unused", inv.volumes.len(), inv.volumes.iter().filter(|v| nusers(&v.name) == 0).count())).color(p.text_dim).small());
    let mut clicked = None;
    hscroll(ui, "dk_vtable", |ui| {
        TableBuilder::new(ui)
            .id_salt("dk_volumes")
            .striped(true)
            .sense(egui::Sense::click())
            .max_scroll_height(340.0)
            .min_scrolled_height(100.0)
            .column(Column::initial(220.0).at_least(120.0).clip(true))
            .column(Column::exact(70.0))
            .column(Column::exact(90.0))
            .column(Column::initial(200.0).clip(true))
            .column(Column::exact(110.0))
            .column(Column::initial(280.0).clip(true))
            .header(26.0, |mut h| {
                h.col(|ui| sort_header(ui, "Name", Col::Name, &mut d.vol.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Driver").strong().color(p.text_dim));
                });
                h.col(|ui| sort_header(ui, "Size", Col::Size, &mut d.vol.sort));
                h.col(|ui| sort_header(ui, "Used by", Col::Users, &mut d.vol.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Created").strong().color(p.text_dim));
                });
                h.col(|ui| {
                    ui.label(RichText::new("Mountpoint").strong().color(p.text_dim));
                });
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let v = rows[row.index()];
                    row.set_selected(d.vol.selected.as_deref() == Some(v.name.as_str()));
                    row.col(|ui| {
                        ui.add(egui::Label::new(if v.anonymous() { format!("{} (anonymous)", short_id(&v.name)) } else { v.name.clone() }).truncate());
                    });
                    row.col(|ui| {
                        ui.label(&v.driver);
                    });
                    row.col(|ui| {
                        ui.label(match sizes.get(&v.name) {
                            Some(Some(b)) => human_bytes(*b),
                            Some(None) => "not measurable".into(),
                            None => "…".into(),
                        });
                    });
                    row.col(|ui| match users.get(v.name.as_str()) {
                        Some(u) => {
                            ui.add(egui::Label::new(u.join(", ")).truncate());
                        }
                        None => super::kind_text(ui, Kind::Warning, "unused"),
                    });
                    row.col(|ui| {
                        ui.label(created(&v.created_at));
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&v.mountpoint).color(p.text_dim)).truncate());
                    });
                    if row.response().clicked() {
                        clicked = Some(v.name.clone());
                    }
                });
            });
    });
    if let Some(n) = clicked {
        d.vol.selected = Some(n);
    }
    let Some(v) = d.vol.selected.clone().and_then(|n| inv.volumes.iter().find(|v| v.name == n).cloned()) else { return };
    if d.vol.inspect.as_ref().map(|(i, _)| *i != v.name).unwrap_or(true) && d.vol.inspect_rx.is_none() {
        if let Some(e) = d.engine() {
            let n = v.name.clone();
            d.vol.inspect_rx = Some(spawn(&ctx, move || (n.clone(), e.inspect_volume(&n).unwrap_or_else(|e| Value::String(e.to_string())))));
        }
    }
    ui.add_space(8.0);
    let used = users.get(v.name.as_str()).cloned().unwrap_or_default();
    let sz = size(&v.name);
    theme::container(ui, &format!("Volume {}", if v.anonymous() { short_id(&v.name).to_string() } else { v.name.clone() }), |ui| {
        theme::kv(ui, "Name", v.name.clone());
        theme::kv(ui, "Driver / scope", format!("{} / {}", v.driver, v.scope));
        theme::kv(ui, "Mountpoint", v.mountpoint.clone());
        theme::kv(ui, "Size", sz.map(human_bytes).unwrap_or_else(|| "not measurable by this driver".into()));
        theme::kv(ui, "Used by", if used.is_empty() { "no container".into() } else { used.join(", ") });
        if !v.labels.is_empty() {
            theme::kv(ui, "Labels", v.labels.iter().map(|(k, x)| format!("{k}={x}")).collect::<Vec<_>>().join(", "));
        }
        let r = ui.add_enabled(used.is_empty(), egui::Button::new(RichText::new("Remove volume").color(p.danger)));
        if r.on_disabled_hover_text("Used by containers: remove those first").clicked() {
            let name = v.name.clone();
            let holds_data = sz != Some(0);
            d.ask(Confirm {
                title: "Delete volume?".into(),
                lines: vec![format!("Delete volume {name}."), format!("Size: {}", sz.map(human_bytes).unwrap_or_else(|| "unknown".into())), "Everything stored in it is erased. Docker keeps no copy.".into()],
                warning: if holds_data { Some("This volume may contain persistent application data (databases, uploads, configuration). Make a backup first if in doubt.".into()) } else { None },
                button: "Delete volume".into(),
                danger: true,
                // a volume with (possibly) data requires typing its name
                typed: if holds_data { Some(name.clone()) } else { None },
                input: String::new(),
                then: Box::new(move |d, ctx| {
                    d.vol.selected = None;
                    let m = format!("Volume {name} deleted");
                    d.run(ctx, format!("remove volume {name}"), Some(("volume remove".into(), name.clone())), move |e| e.remove_volume(&name).map(|_| m).map_err(|e| e.to_string()));
                }),
            });
        }
        if let Some((n, val)) = &d.vol.inspect {
            if *n == v.name {
                egui::CollapsingHeader::new("Inspect (JSON)").id_salt("dk_vinsp").show(ui, |ui| super::json_box(ui, "dk_vinspect", val));
            }
        }
    });
}

fn created(s: &str) -> String {
    s.get(..19).map(|x| x.replace('T', " ")).unwrap_or_else(|| s.to_string())
}
