//! Storage analysis from `/system/df`: totals, reclaimable space, share of the host disk, per-object
//! breakdowns sortable by size, and the unused / orphaned objects that cleanup could remove.
use super::{hscroll, ord, sort_header, DockerUi, Tab};
use crate::charts::stacked_bar;
use crate::docker::api::short_id;
use crate::docker::cleanup::{Selection, VolumeScope};
use crate::linux::{human_bytes, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use egui_extras::{Column, TableBuilder};

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Size,
    Status,
}

#[derive(Clone, Copy, PartialEq, Default)]
enum View {
    #[default]
    Images,
    Containers,
    Volumes,
    BuildCache,
}

pub(super) struct State {
    view: View,
    sort: (Col, bool),
    unused_only: bool,
}

impl Default for State {
    fn default() -> Self {
        Self { view: View::Images, sort: (Col::Size, false), unused_only: false }
    }
}

struct Row {
    name: String,
    detail: String,
    size: Option<u64>,
    unused: bool,
    status: String,
}

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    if d.df_stale {
        d.refresh_df(&ctx);
    }
    let p = pal(ui);
    let Some(df) = d.df.clone() else {
        match &d.df_err {
            Some(e) => theme::note(ui, Kind::Error, &format!("Could not measure disk usage: {e}")),
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Measuring Docker disk usage (walks every layer and volume)...");
                });
            }
        }
        return;
    };
    let t = df.totals();
    let root = d.inv.as_ref().map(|i| i.info.docker_root_dir.clone()).unwrap_or_default();
    let host = crate::docker::host_disk(&root);
    let mut goto: Option<Selection> = None;
    theme::card_grid(ui, 220.0, 4, |ui, k| match k {
        0 => theme::container(ui, "Docker total", |ui| {
            theme::big_value(ui, "images + containers + volumes + cache", &human_bytes(t.total()), None);
        }),
        1 => theme::container(ui, "Reclaimable", |ui| {
            theme::big_value(ui, "removable by cleanup", &human_bytes(t.reclaimable()), Some(p.success));
        }),
        2 => theme::container(ui, "Share of host disk", |ui| match host {
            Some((total, avail)) => {
                let pct = if total == 0 { 0.0 } else { 100.0 * t.total() as f32 / total as f32 };
                theme::big_value(ui, &format!("{} free of {}", human_bytes(avail), human_bytes(total)), &format!("{pct:.1}%"), Some(theme::usage_color(&p, pct * 2.0)));
                theme::meter(ui, pct / 100.0, p.series[0], 6.0);
            }
            None => {
                ui.label("unknown");
            }
        }),
        _ => theme::container(ui, "Layers", |ui| {
            theme::big_value(ui, "de-duplicated image layers", &human_bytes(df.layers_size.max(0) as u64), None);
        }),
    });
    theme::container(ui, "Breakdown", |ui| {
        stacked_bar(ui, 18.0, &[("Images", p.series[0], t.images as f64), ("Container layers", p.series[1], t.containers as f64), ("Volumes", p.series[2], t.volumes as f64), ("Build cache", p.series[3], t.build_cache as f64)], &|x| human_bytes(x as u64));
        egui::Grid::new("dk_df").striped(true).spacing([24.0, 6.0]).show(ui, |ui| {
            for h in ["Type", "Objects", "Size", "Reclaimable", ""] {
                ui.label(RichText::new(h).strong().color(p.text_dim));
            }
            ui.end_row();
            let rows: [(&str, usize, u64, u64, Selection); 4] = [
                ("Images", df.images.len(), t.images, t.images_reclaimable, Selection { dangling_images: true, unused_images: true, ..Default::default() }),
                ("Container writable layers", df.containers.len(), t.containers, t.containers_reclaimable, Selection { stopped_containers: true, ..Default::default() }),
                ("Volumes", df.volumes.len(), t.volumes, t.volumes_reclaimable, Selection { volumes: VolumeScope::Anonymous, ..Default::default() }),
                ("Build cache", df.build_cache.len(), t.build_cache, t.build_cache_reclaimable, Selection { build_cache: true, ..Default::default() }),
            ];
            for (name, n, size, rec, sel) in rows {
                ui.label(name);
                ui.label(n.to_string());
                ui.label(human_bytes(size));
                ui.label(RichText::new(format!("{} ({:.0}%)", human_bytes(rec), if size == 0 { 0.0 } else { 100.0 * rec as f64 / size as f64 })).color(if rec > 0 { p.success } else { p.text_dim }));
                if rec > 0 && ui.small_button("Clean up...").clicked() {
                    goto = Some(sel);
                } else if rec == 0 {
                    ui.label("");
                }
                ui.end_row();
            }
        });
    });
    ui.add_space(8.0);
    // unused / orphaned summary
    let inv = d.inv.clone();
    theme::container(ui, "Unused and orphaned objects", |ui| {
        let dangling = df.images.iter().filter(|i| i.dangling() && i.containers <= 0).count();
        let unused_imgs = df.images.iter().filter(|i| i.containers == 0).count();
        let stopped = df.containers.iter().filter(|c| c.is_stopped()).count();
        let unused_vols = df.volumes.iter().filter(|v| v.usage_data.as_ref().map(|u| u.ref_count == 0).unwrap_or(false)).count();
        let anon_orphans = df.volumes.iter().filter(|v| v.anonymous() && v.usage_data.as_ref().map(|u| u.ref_count == 0).unwrap_or(false)).count();
        let cache = df.build_cache.iter().filter(|b| !b.in_use).count();
        let unused_nets = inv.as_ref().map(|i| {
            let users = i.network_users();
            i.networks.iter().filter(|n| !n.predefined() && n.scope != "swarm" && users.get(&n.id).map(|v| v.is_empty()).unwrap_or(true)).count()
        });
        for (k, n, why) in [
            (Kind::Info, unused_imgs, "images not used by any container"),
            (Kind::Info, dangling, "dangling images (untagged, unused)"),
            (Kind::Info, stopped, "stopped containers"),
            (Kind::Warning, unused_vols, "volumes not mounted by any container (may hold data)"),
            (Kind::Warning, anon_orphans, "of them anonymous volumes left behind by removed containers (orphaned)"),
            (Kind::Info, cache, "build cache records not in use"),
            (Kind::Info, unused_nets.unwrap_or(0), "custom networks without containers"),
        ] {
            theme::status(ui, if n == 0 { Kind::Inactive } else { k }, &format!("{n} {why}"));
        }
    });
    ui.add_space(8.0);
    // per-object tables
    ui.horizontal_wrapped(|ui| {
        for (v, l) in [(View::Images, "Images"), (View::Containers, "Containers"), (View::Volumes, "Volumes"), (View::BuildCache, "Build cache")] {
            ui.selectable_value(&mut d.st.view, v, l);
        }
        ui.checkbox(&mut d.st.unused_only, "Unused only");
    });
    let mut rows: Vec<Row> = match d.st.view {
        View::Images => df.images.iter().flat_map(|i| {
            let name = if i.dangling() { format!("<none> {}", short_id(&i.id)) } else { i.tags().join(", ") };
            Some(Row { name, detail: short_id(&i.id).into(), size: u64::try_from(i.size).ok(), unused: i.containers == 0, status: format!("{} containers · shared {}", i.containers.max(0), human_bytes(i.shared_size.max(0) as u64)) })
        }).collect(),
        View::Containers => df.containers.iter().map(|c| Row { name: c.name(), detail: c.image.clone(), size: c.size_rw.or(c.size_root_fs.map(|_| 0)).and_then(|s| u64::try_from(s).ok()), unused: c.is_stopped(), status: c.status.clone() }).collect(),
        View::Volumes => df.volumes.iter().map(|v| {
            let refs = v.usage_data.as_ref().map(|u| u.ref_count).unwrap_or(-1);
            Row { name: if v.anonymous() { format!("{} (anonymous)", short_id(&v.name)) } else { v.name.clone() }, detail: v.mountpoint.clone(), size: v.size(), unused: refs == 0, status: if refs < 0 { "usage unknown".into() } else { format!("{refs} containers") } }
        }).collect(),
        View::BuildCache => df.build_cache.iter().map(|b| Row { name: format!("{} {}", b.kind, short_id(&b.id)), detail: b.description.clone(), size: u64::try_from(b.size).ok(), unused: !b.in_use, status: format!("{}{} · used {} times", if b.in_use { "in use" } else { "unused" }, if b.shared { ", shared" } else { "" }, b.usage_count) }).collect(),
    };
    if d.st.unused_only {
        rows.retain(|r| r.unused);
    }
    let s = d.st.sort;
    rows.sort_by(|a, b| match s.0 {
        Col::Name => ord(a.name.to_lowercase(), b.name.to_lowercase(), s.1),
        Col::Size => ord(a.size.unwrap_or(0), b.size.unwrap_or(0), s.1),
        Col::Status => ord(a.unused, b.unused, s.1),
    });
    hscroll(ui, "dk_stable", |ui| {
        TableBuilder::new(ui)
            .id_salt(("dk_storage", d.st.view as u8))
            .striped(true)
            .max_scroll_height(360.0)
            .min_scrolled_height(100.0)
            .column(Column::initial(280.0).at_least(140.0).clip(true))
            .column(Column::exact(100.0))
            .column(Column::exact(90.0))
            .column(Column::initial(220.0).clip(true))
            .column(Column::initial(260.0).clip(true))
            .header(26.0, |mut h| {
                h.col(|ui| sort_header(ui, "Name", Col::Name, &mut d.st.sort));
                h.col(|ui| sort_header(ui, "Size", Col::Size, &mut d.st.sort));
                h.col(|ui| sort_header(ui, "Unused", Col::Status, &mut d.st.sort));
                h.col(|ui| {
                    ui.label(RichText::new("Usage").strong().color(p.text_dim));
                });
                h.col(|ui| {
                    ui.label(RichText::new("Details").strong().color(p.text_dim));
                });
            })
            .body(|body| {
                body.rows(24.0, rows.len(), |mut row| {
                    let r = &rows[row.index()];
                    row.col(|ui| {
                        ui.add(egui::Label::new(&r.name).truncate());
                    });
                    row.col(|ui| {
                        ui.label(r.size.map(human_bytes).unwrap_or_else(|| "unknown".into()));
                    });
                    row.col(|ui| {
                        if r.unused {
                            super::kind_text(ui, Kind::Warning, "unused");
                        } else {
                            ui.label(RichText::new("in use").color(p.text_dim));
                        }
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&r.status).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&r.detail).color(p.text_dim)).truncate());
                    });
                });
            });
    });
    if let Some(sel) = goto {
        d.cl.preset(sel);
        d.tab = Tab::Cleanup;
    }
}
