//! Images: list with usage and flags (dangling, outdated, duplicate tags), pull, remove, layers, inspect.
use super::{age, hscroll, matches, ord, search, sort_header, Confirm, DockerUi, Tab};
use crate::docker::api::{short_id, valid_image_ref, HistoryEntry, Image};
use crate::docker::cleanup::Selection;
use crate::linux::{human_bytes, spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};
use serde_json::Value;
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Repo,
    Size,
    Created,
    Used,
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    InUse,
    Unused,
    Dangling,
    Outdated,
}

pub(super) struct State {
    q: String,
    filter: Filter,
    sort: (Col, bool),
    selected: Option<String>,
    pull: String,
    pull_rx: Option<Receiver<Result<String, String>>>,
    pull_progress: Option<Receiver<String>>,
    pull_line: String,
    detail_rx: Option<Receiver<(String, Result<(Vec<HistoryEntry>, Value), String>)>>,
    detail: Option<(String, Result<(Vec<HistoryEntry>, Value), String>)>,
    show_inspect: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { q: String::new(), filter: Filter::All, sort: (Col::Size, false), selected: None, pull: String::new(), pull_rx: None, pull_progress: None, pull_line: String::new(), detail_rx: None, detail: None, show_inspect: false }
    }
}

impl State {
    pub(super) fn poll(&mut self) {
        if let Some(rx) = &self.pull_progress {
            while let Ok(l) = rx.try_recv() {
                self.pull_line = l;
            }
        }
        if let Some(rx) = &self.detail_rx {
            if let Ok(r) = rx.try_recv() {
                self.detail = Some(r);
                self.detail_rx = None;
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
    // pull finished?
    if let Some(rx) = &d.im.pull_rx {
        if let Ok(r) = rx.try_recv() {
            d.im.pull_rx = None;
            d.im.pull_progress = None;
            match r {
                Ok(m) => d.toast(Kind::Success, m),
                Err(e) => d.toast(Kind::Error, format!("Pull failed: {e}")),
            }
            d.refresh(&ctx);
            d.df_stale = true;
        }
    }
    theme::container(ui, "Pull an image", |ui| {
        ui.horizontal_wrapped(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut d.im.pull).hint_text("nginx:1.27-alpine, ghcr.io/org/app:v1, registry:5000/img").desired_width(380.0));
            let busy = d.im.pull_rx.is_some();
            let valid = valid_image_ref(d.im.pull.trim());
            let go = ui.add_enabled(!busy && valid, egui::Button::new("Pull")).clicked() || (valid && !busy && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if !d.im.pull.trim().is_empty() && !valid {
                super::kind_text(ui, Kind::Error, "not a valid image reference");
            }
            if busy {
                ui.spinner();
                ui.add(egui::Label::new(RichText::new(&d.im.pull_line).monospace().size(12.0)).truncate());
            }
            if go {
                if let Some(e) = d.engine() {
                    let r = d.im.pull.trim().to_string();
                    let (ptx, prx) = channel();
                    let ctx2 = ctx.clone();
                    d.im.pull_progress = Some(prx);
                    d.im.pull_line = format!("pulling {r}...");
                    d.im.pull_rx = Some(spawn(&ctx, move || {
                        let res = e.pull(&r, |l| {
                            let _ = ptx.send(l);
                            ctx2.request_repaint();
                        });
                        crate::docker::audit::record("image pull", &r, &res);
                        res.map(|_| format!("Pulled {r}")).map_err(|e| e.to_string())
                    }));
                }
            }
        });
        ui.label(RichText::new("Pulls go through the daemon, so its proxy and registry mirror settings apply. Private registries need a prior `docker login`.").color(p.text_dim).small());
    });
    ui.add_space(8.0);
    let users = inv.image_users();
    let running: std::collections::HashSet<&str> = inv.containers.iter().filter(|c| !c.is_stopped()).map(|c| c.image_id.as_str()).collect();
    let flags = super::super::image_flags(&inv.images);
    ui.horizontal_wrapped(|ui| {
        search(ui, &mut d.im.q, "Search repository, tag or ID");
        for (f, l) in [(Filter::All, "All"), (Filter::InUse, "In use"), (Filter::Unused, "Unused"), (Filter::Dangling, "Dangling"), (Filter::Outdated, "Older versions")] {
            ui.selectable_value(&mut d.im.filter, f, l);
        }
        ui.add_space(12.0);
        if ui.button("Prune dangling images...").on_hover_text("Opens a cleanup preview first").clicked() {
            d.cl.preset(Selection { dangling_images: true, ..Default::default() });
            d.tab = Tab::Cleanup;
        }
        if ui.button("Remove unused images...").on_hover_text("Opens a cleanup preview first").clicked() {
            d.cl.preset(Selection { dangling_images: true, unused_images: true, ..Default::default() });
            d.tab = Tab::Cleanup;
        }
    });
    let st = &d.im;
    // one row per (image, tag)
    let mut rows: Vec<(&Image, String, String)> = vec![];
    for i in &inv.images {
        let used = users.contains_key(i.id.as_str());
        let ok = match st.filter {
            Filter::All => true,
            Filter::InUse => used,
            Filter::Unused => !used,
            Filter::Dangling => i.dangling(),
            Filter::Outdated => flags.get(&i.id).map(|f| f.outdated).unwrap_or(false),
        };
        if !ok {
            continue;
        }
        for (r, t) in i.repo_tag_pairs() {
            if matches(&st.q, &[&r, &t, &i.id]) {
                rows.push((i, r, t));
            }
        }
    }
    let nused = |i: &Image| users.get(i.id.as_str()).map(|v| v.len()).unwrap_or(0);
    rows.sort_by(|a, b| match st.sort.0 {
        Col::Repo => ord((&a.1, &a.2), (&b.1, &b.2), st.sort.1),
        Col::Size => ord(a.0.size, b.0.size, st.sort.1),
        Col::Created => ord(a.0.created, b.0.created, st.sort.1),
        Col::Used => ord(nused(a.0), nused(b.0), st.sort.1),
    });
    let total: i64 = inv.images.iter().map(|i| i.size).sum();
    ui.label(RichText::new(format!("{} images · {} (shared layers counted per image; see Storage for the de-duplicated total)", inv.images.len(), human_bytes(total.max(0) as u64))).color(p.text_dim).small());
    let mut clicked = None;
    hscroll(ui, "dk_itable", |ui| {
        TableBuilder::new(ui)
            .id_salt("dk_images")
            .striped(true)
            .sense(egui::Sense::click())
            .max_scroll_height(360.0)
            .min_scrolled_height(120.0)
            .column(Column::initial(260.0).at_least(140.0).clip(true))
            .column(Column::initial(120.0).clip(true))
            .column(Column::exact(110.0))
            .column(Column::exact(90.0))
            .column(Column::exact(110.0))
            .column(Column::initial(180.0).clip(true))
            .column(Column::initial(160.0).clip(true))
            .header(26.0, |mut h| {
                h.col(|ui| sort_header(ui, "Repository", Col::Repo, &mut d.im.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Tag").strong().color(p.text_dim));
                });
                h.col(|ui| {
                    ui.label(RichText::new("Image ID").strong().color(p.text_dim));
                });
                h.col(|ui| sort_header(ui, "Size", Col::Size, &mut d.im.sort));
                h.col(|ui| sort_header(ui, "Created", Col::Created, &mut d.im.sort));
                h.col(|ui| sort_header(ui, "Used by", Col::Used, &mut d.im.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Notes").strong().color(p.text_dim));
                });
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let (i, r, t) = &rows[row.index()];
                    row.set_selected(d.im.selected.as_deref() == Some(i.id.as_str()));
                    row.col(|ui| {
                        ui.add(egui::Label::new(r.as_str()).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(t.as_str()).truncate());
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(short_id(&i.id)).monospace());
                    });
                    row.col(|ui| {
                        ui.label(human_bytes(i.size.max(0) as u64));
                    });
                    row.col(|ui| {
                        ui.label(age(i.created));
                    });
                    row.col(|ui| {
                        let u = users.get(i.id.as_str()).map(|v| v.join(", ")).unwrap_or_default();
                        if u.is_empty() {
                            ui.label(RichText::new("unused").color(p.text_dim));
                        } else {
                            super::kind_text(ui, if running.contains(i.id.as_str()) { Kind::Success } else { Kind::Info }, &u);
                        }
                    });
                    row.col(|ui| {
                        let f = flags.get(&i.id).cloned().unwrap_or_default();
                        let mut n = vec![];
                        if i.dangling() {
                            n.push("dangling");
                        }
                        if f.outdated {
                            n.push("older version");
                        }
                        if f.duplicate_tags {
                            n.push("several tags");
                        }
                        ui.label(RichText::new(n.join(", ")).color(p.warning));
                    });
                    if row.response().clicked() {
                        clicked = Some(i.id.clone());
                    }
                });
            });
    });
    if let Some(id) = clicked {
        d.im.selected = Some(id);
    }
    let Some(img) = d.im.selected.clone().and_then(|id| inv.images.iter().find(|i| i.id == id).cloned()) else { return };
    ui.add_space(8.0);
    let used_by = users.get(img.id.as_str()).cloned().unwrap_or_default();
    let in_running = running.contains(img.id.as_str());
    theme::container(ui, &format!("Image {}", img.tags().first().copied().unwrap_or(short_id(&img.id))), |ui| {
        theme::kv(ui, "ID", img.id.clone());
        theme::kv(ui, "Tags", if img.tags().is_empty() { "none (dangling)".into() } else { img.tags().join(", ") });
        theme::kv(ui, "Digests", img.repo_digests.join(", "));
        theme::kv(ui, "Size", format!("{} (unique {})", human_bytes(img.size.max(0) as u64), human_bytes((img.size - img.shared_size.max(0)).max(0) as u64)));
        theme::kv(ui, "Used by", if used_by.is_empty() { "no container".into() } else { used_by.join(", ") });
        ui.horizontal_wrapped(|ui| {
            for t in img.tags() {
                if ui.add_enabled(!in_running || img.tags().len() > 1, egui::Button::new(format!("Untag {t}"))).clicked() {
                    let (t, id) = (t.to_string(), img.id.clone());
                    d.ask(Confirm {
                        title: "Remove tag?".into(),
                        lines: vec![format!("Remove the tag {t}."), if img.tags().len() > 1 { "The image keeps its other tags.".into() } else { "This is the last tag: the image is deleted if no container uses it.".into() }],
                        warning: None,
                        button: "Remove tag".into(),
                        danger: true,
                        typed: None,
                        input: String::new(),
                        then: Box::new(move |d, ctx| {
                            let _ = &id;
                            let t2 = t.clone();
                            d.run(ctx, format!("untag {t}"), Some(("image remove tag".into(), t.clone())), move |e| e.remove_image(&t2, false).map(|r| format!("{t2}: {}", r.join("; "))).map_err(|e| e.to_string()));
                        }),
                    });
                }
            }
            let blocked = !used_by.is_empty();
            if theme::danger_button(ui, "Remove image").on_hover_text(if blocked { "Used by containers: remove them first" } else { "Delete the image and all its tags" }).clicked() {
                if blocked {
                    d.toast(Kind::Warning, format!("The image is used by {}; remove those containers first", used_by.join(", ")));
                } else {
                    let (id, tags) = (img.id.clone(), img.tags().iter().map(|s| s.to_string()).collect::<Vec<_>>());
                    let label = tags.first().cloned().unwrap_or_else(|| short_id(&id).to_string());
                    d.ask(Confirm {
                        title: "Remove image?".into(),
                        lines: vec![format!("Delete {} ({}), {}.", label, short_id(&id), human_bytes(img.size.max(0) as u64)), format!("Tags removed: {}", if tags.is_empty() { "none".into() } else { tags.join(", ") }), "It can be pulled again later if it came from a registry.".into()],
                        warning: None,
                        button: "Remove image".into(),
                        danger: true,
                        typed: None,
                        input: String::new(),
                        then: Box::new(move |d, ctx| {
                            d.im.selected = None;
                            d.run(ctx, format!("remove image {label}"), Some(("image remove".into(), label.clone())), move |e| {
                                // a multi-tag image cannot be removed by ID without force; remove tag by tag
                                let mut out = vec![];
                                if tags.len() > 1 {
                                    for t in &tags {
                                        out.extend(e.remove_image(t, false).map_err(|e| e.to_string())?);
                                    }
                                } else {
                                    out = e.remove_image(&id, false).map_err(|e| e.to_string())?;
                                }
                                Ok(format!("Image removed ({} entries)", out.len()))
                            });
                        }),
                    });
                }
            }
            if ui.button("Layers & inspect").clicked() {
                if let Some(e) = d.engine() {
                    let id = img.id.clone();
                    d.im.detail = None;
                    d.im.detail_rx = Some(spawn(&ctx, move || (id.clone(), e.image_history(&id).and_then(|h| e.inspect_image(&id).map(|v| (h, v))).map_err(|e| e.to_string()))));
                }
            }
        });
        if d.im.detail_rx.is_some() {
            ui.spinner();
        }
        if let Some((id, r)) = &d.im.detail {
            if *id == img.id {
                match r {
                    Ok((hist, v)) => {
                        ui.label(RichText::new(format!("{} layers", hist.len())).strong());
                        egui::ScrollArea::vertical().id_salt("dk_layers").max_height(260.0).show(ui, |ui| {
                            egui::Grid::new("dk_layers_g").striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                                for h in hist {
                                    ui.label(human_bytes(h.size.max(0) as u64));
                                    ui.label(age(h.created));
                                    ui.add(egui::Label::new(RichText::new(h.created_by.replace("/bin/sh -c #(nop) ", "").chars().take(160).collect::<String>()).monospace().size(12.0)).truncate());
                                    ui.end_row();
                                }
                            });
                        });
                        ui.checkbox(&mut d.im.show_inspect, "Show full inspection (JSON)");
                        if d.im.show_inspect {
                            super::json_box(ui, "dk_iinspect", v);
                        }
                    }
                    Err(e) => theme::status(ui, Kind::Error, e),
                }
            }
        }
    });
}
