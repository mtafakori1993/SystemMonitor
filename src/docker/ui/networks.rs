//! Networks: list, inspect, create, remove, connect and disconnect containers.
use super::{hscroll, matches, ord, search, sort_header, Confirm, DockerUi};
use crate::docker::api::{short_id, NetworkSpec, NETWORK_DRIVERS};
use crate::linux::{spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use serde_json::Value;
use std::sync::mpsc::Receiver;

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Driver,
    Containers,
}

pub(super) struct State {
    q: String,
    sort: (Col, bool),
    selected: Option<String>,
    spec: NetworkSpec,
    show_create: bool,
    connect_to: String,
    inspect: Option<(String, Value)>,
    inspect_rx: Option<Receiver<(String, Value)>>,
}

impl Default for State {
    fn default() -> Self {
        Self { q: String::new(), sort: (Col::Name, true), selected: None, spec: NetworkSpec { driver: "bridge".into(), attachable: true, ..Default::default() }, show_create: false, connect_to: String::new(), inspect: None, inspect_rx: None }
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
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    let users = inv.network_users();
    ui.horizontal_wrapped(|ui| {
        search(ui, &mut d.net.q, "Search networks");
        if ui.button(if d.net.show_create { "Hide create form" } else { "Create network..." }).clicked() {
            d.net.show_create = !d.net.show_create;
        }
    });
    if d.net.show_create {
        let mut create = false;
        theme::container(ui, "Create network", |ui| {
            let s = &mut d.net.spec;
            egui::Grid::new("dk_netform").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(220.0).hint_text("backend"));
                ui.end_row();
                ui.label("Driver");
                egui::ComboBox::from_id_salt("dk_netdrv").selected_text(s.driver.clone()).show_ui(ui, |ui| {
                    for dr in NETWORK_DRIVERS {
                        ui.selectable_value(&mut s.driver, dr.to_string(), dr);
                    }
                });
                ui.end_row();
                ui.label("Subnet (optional)");
                ui.add(egui::TextEdit::singleline(&mut s.subnet).desired_width(220.0).hint_text("172.30.0.0/16"));
                ui.end_row();
                ui.label("Gateway (optional)");
                ui.add(egui::TextEdit::singleline(&mut s.gateway).desired_width(220.0).hint_text("172.30.0.1"));
                ui.end_row();
                if matches!(s.driver.as_str(), "macvlan" | "ipvlan") {
                    ui.label("Parent interface");
                    ui.add(egui::TextEdit::singleline(&mut s.parent).desired_width(220.0).hint_text("eth0"));
                    ui.end_row();
                }
                ui.label("");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.internal, "Internal (no outside access)");
                    ui.checkbox(&mut s.attachable, "Attachable");
                });
                ui.end_row();
            });
            if s.driver == "overlay" {
                ui.label(RichText::new("overlay networks need swarm mode.").color(p.text_dim).small());
            }
            match s.validate() {
                Ok(()) => create = theme::primary_button(ui, "Create").clicked(),
                Err(e) if !s.name.is_empty() => theme::status(ui, Kind::Error, &e),
                Err(_) => {}
            }
        });
        if create {
            let spec = d.net.spec.clone();
            let n = spec.name.clone();
            d.run(&ctx, format!("create network {n}"), Some(("network create".into(), n.clone())), move |e| e.create_network(&spec).map(|id| format!("Network {n} created ({})", short_id(&id))).map_err(|e| e.to_string()));
            d.net.spec.name.clear();
        }
        ui.add_space(8.0);
    }
    let mut rows: Vec<_> = inv.networks.iter().filter(|n| matches(&d.net.q, &[&n.name, &n.id, &n.driver, &n.subnets()])).collect();
    let cnt = |id: &str| users.get(id).map(|v| v.len()).unwrap_or(0);
    let s = d.net.sort;
    rows.sort_by(|a, b| match s.0 {
        Col::Name => ord(&a.name, &b.name, s.1),
        Col::Driver => ord(&a.driver, &b.driver, s.1),
        Col::Containers => ord(cnt(&a.id), cnt(&b.id), s.1),
    });
    let mut clicked = None;
    hscroll(ui, "dk_ntable", |ui| {
        TableBuilder::new(ui)
            .id_salt("dk_networks")
            .striped(true)
            .sense(egui::Sense::click())
            .max_scroll_height(300.0)
            .min_scrolled_height(100.0)
            .column(Column::initial(180.0).clip(true))
            .column(Column::exact(110.0))
            .column(Column::exact(80.0))
            .column(Column::exact(70.0))
            .column(Column::initial(170.0).clip(true))
            .column(Column::initial(130.0).clip(true))
            .column(Column::exact(90.0))
            .column(Column::initial(120.0).clip(true))
            .header(26.0, |mut h| {
                h.col(|ui| sort_header(ui, "Name", Col::Name, &mut d.net.sort));
                h.col(|ui| {
                    ui.label(RichText::new("ID").strong().color(p.text_dim));
                });
                h.col(|ui| sort_header(ui, "Driver", Col::Driver, &mut d.net.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Scope").strong().color(p.text_dim));
                });
                h.col(|ui| {
                    ui.label(RichText::new("Subnet").strong().color(p.text_dim));
                });
                h.col(|ui| {
                    ui.label(RichText::new("Gateway").strong().color(p.text_dim));
                });
                h.col(|ui| sort_header(ui, "Containers", Col::Containers, &mut d.net.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Flags").strong().color(p.text_dim));
                });
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let n = rows[row.index()];
                    row.set_selected(d.net.selected.as_deref() == Some(n.id.as_str()));
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&n.name).strong()).truncate());
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(short_id(&n.id)).monospace());
                    });
                    row.col(|ui| {
                        ui.label(&n.driver);
                    });
                    row.col(|ui| {
                        ui.label(&n.scope);
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(n.subnets()).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(n.gateways()).truncate());
                    });
                    row.col(|ui| {
                        ui.label(cnt(&n.id).to_string());
                    });
                    row.col(|ui| {
                        let mut f = vec![];
                        if n.predefined() {
                            f.push("built-in");
                        }
                        if n.internal {
                            f.push("internal");
                        }
                        if n.enable_ipv6 {
                            f.push("IPv6");
                        }
                        ui.label(RichText::new(f.join(", ")).color(p.text_dim));
                    });
                    if row.response().clicked() {
                        clicked = Some(n.id.clone());
                    }
                });
            });
    });
    if let Some(id) = clicked {
        d.net.selected = Some(id);
    }
    let Some(n) = d.net.selected.clone().and_then(|id| inv.networks.iter().find(|n| n.id == id).cloned()) else { return };
    if d.net.inspect.as_ref().map(|(i, _)| *i != n.id).unwrap_or(true) && d.net.inspect_rx.is_none() {
        if let Some(e) = d.engine() {
            let id = n.id.clone();
            d.net.inspect_rx = Some(spawn(&ctx, move || (id.clone(), e.inspect_network_raw(&id).unwrap_or_else(|e| Value::String(e.to_string())))));
        }
    }
    ui.add_space(8.0);
    let attached = users.get(&n.id).cloned().unwrap_or_default();
    theme::container(ui, &format!("Network {}", n.name), |ui| {
        theme::kv(ui, "Driver / scope", format!("{} / {}", n.driver, n.scope));
        theme::kv(ui, "Subnet / gateway", format!("{} / {}", n.subnets(), n.gateways()));
        ui.label(RichText::new(format!("Attached containers ({})", attached.len())).strong());
        for (cid, cname) in &attached {
            ui.horizontal(|ui| {
                ui.label(cname);
                if ui.small_button("Disconnect").clicked() {
                    let (net, nname, cid, cname) = (n.id.clone(), n.name.clone(), cid.clone(), cname.clone());
                    d.ask(Confirm {
                        title: "Disconnect container?".into(),
                        lines: vec![format!("Disconnect {cname} from {nname}."), "Its connections over this network drop immediately.".into()],
                        warning: None,
                        button: "Disconnect".into(),
                        danger: true,
                        typed: None,
                        input: String::new(),
                        then: Box::new(move |d, ctx| {
                            let m = format!("{cname} disconnected from {nname}");
                            d.run(ctx, format!("disconnect {cname}"), Some(("network disconnect".into(), format!("{cname} from {nname}"))), move |e| e.disconnect_network(&net, &cid).map(|_| m).map_err(|e| e.to_string()));
                        }),
                    });
                }
            });
        }
        if n.name != "host" && n.name != "none" {
            ui.horizontal(|ui| {
                let candidates: Vec<_> = inv.containers.iter().filter(|c| !attached.iter().any(|(id, _)| *id == c.id)).collect();
                egui::ComboBox::from_id_salt("dk_netconn").selected_text(if d.net.connect_to.is_empty() { "choose a container".to_string() } else { candidates.iter().find(|c| c.id == d.net.connect_to).map(|c| c.name()).unwrap_or_default() }).show_ui(ui, |ui| {
                    for c in &candidates {
                        ui.selectable_value(&mut d.net.connect_to, c.id.clone(), c.name());
                    }
                });
                if ui.add_enabled(!d.net.connect_to.is_empty(), egui::Button::new("Connect")).clicked() {
                    let (net, nname, cid) = (n.id.clone(), n.name.clone(), std::mem::take(&mut d.net.connect_to));
                    let cname = inv.containers.iter().find(|c| c.id == cid).map(|c| c.name()).unwrap_or_default();
                    let m = format!("{cname} connected to {nname}");
                    d.run(&ctx, format!("connect {cname}"), Some(("network connect".into(), format!("{cname} to {nname}"))), move |e| e.connect_network(&net, &cid).map(|_| m).map_err(|e| e.to_string()));
                }
            });
        }
        ui.add_space(4.0);
        let removable = !n.predefined() && attached.is_empty();
        if ui.add_enabled(removable, egui::Button::new("Remove network")).on_disabled_hover_text(if n.predefined() { "Built-in networks cannot be removed" } else { "Disconnect all containers first" }).clicked() {
            let (id, name) = (n.id.clone(), n.name.clone());
            d.ask(Confirm {
                title: "Remove network?".into(),
                lines: vec![format!("Remove network {name} ({}, {}).", n.driver, n.subnets())],
                warning: None,
                button: "Remove".into(),
                danger: true,
                typed: None,
                input: String::new(),
                then: Box::new(move |d, ctx| {
                    d.net.selected = None;
                    let m = format!("Network {name} removed");
                    d.run(ctx, format!("remove network {name}"), Some(("network remove".into(), name.clone())), move |e| e.remove_network(&id).map(|_| m).map_err(|e| e.to_string()));
                }),
            });
        }
        if let Some((id, v)) = &d.net.inspect {
            if *id == n.id {
                egui::CollapsingHeader::new("Inspect (JSON)").id_salt("dk_ninsp").show(ui, |ui| super::json_box(ui, "dk_ninspect", v));
            }
        }
    });
}
