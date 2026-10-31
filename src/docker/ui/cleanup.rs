//! Cleanup: choose categories (or a `docker system prune` preset), preview exactly what would go,
//! confirm, run item by item, show the results and the space recovered.
use super::{Confirm, DockerUi};
use crate::app::Notices;
use crate::docker::api::Engine;
use crate::docker::audit;
use crate::docker::cleanup::{self, Category, Inputs, Plan, Report, Selection, VolumeScope};
use crate::docker::Inventory;
use crate::linux::{human_bytes, spawn, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};
use std::sync::mpsc::Receiver;

const VOLUME_WORD: &str = "DELETE VOLUMES";

#[derive(Default)]
pub(super) struct State {
    sel: Selection,
    plan: Option<Plan>,
    plan_err: Option<String>,
    plan_rx: Option<Receiver<Result<Plan, String>>>,
    run_rx: Option<Receiver<Report>>,
    report: Option<Report>,
    auto_preview: bool,
    show_protected: bool,
}

impl State {
    /// Select categories and preview them as soon as the tab is shown.
    pub(super) fn preset(&mut self, s: Selection) {
        self.sel = s;
        self.plan = None;
        self.report = None;
        self.auto_preview = true;
    }

    pub(super) fn poll(&mut self, st: &mut audit::State, n: &mut Notices) {
        if let Some(rx) = &self.plan_rx {
            if let Ok(r) = rx.try_recv() {
                self.plan_rx = None;
                match r {
                    Ok(p) => {
                        self.plan = Some(p);
                        self.plan_err = None;
                    }
                    Err(e) => self.plan_err = Some(e),
                }
            }
        }
        if let Some(rx) = &self.run_rx {
            if let Ok(r) = rx.try_recv() {
                self.run_rx = None;
                *st = audit::load_state();
                if r.failed() == 0 {
                    n.ok(format!("Cleanup finished: {} items removed, about {} recovered", r.ok(), human_bytes(r.reclaimed)));
                } else {
                    n.push(Kind::Warning, format!("Cleanup finished with {} failures ({} removed)", r.failed(), r.ok()));
                }
                self.report = Some(r);
                self.plan = None;
            }
        }
    }
}

fn make_plan(e: &Engine, sel: Selection) -> Result<Plan, String> {
    // always from fresh data: the preview must match the state the deletion will see
    let inv = Inventory::fetch(e).map_err(|e| e.to_string())?;
    let df = e.disk_usage().map_err(|e| e.to_string())?;
    Ok(cleanup::plan(sel, &Inputs { containers: &inv.containers, images: &inv.images, networks: &inv.networks, volumes: &inv.volumes, df: Some(&df) }))
}

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let p = pal(ui);
    let ctx = ui.ctx().clone();
    let before = d.cl.sel;
    theme::container(ui, "What to clean", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Presets:");
            if ui.button("docker system prune").on_hover_text("Stopped containers, unused networks, dangling images, build cache").clicked() {
                d.cl.sel = Selection::system_prune();
            }
            if ui.button("system prune -a").on_hover_text("… plus every image not used by a container").clicked() {
                d.cl.sel = Selection::system_prune_all();
            }
            if ui.button("system prune --volumes").on_hover_text("… plus unused anonymous volumes").clicked() {
                d.cl.sel = Selection::system_prune_volumes();
            }
            if ui.button("Clear selection").clicked() {
                d.cl.sel = Selection::default();
            }
        });
        ui.separator();
        let s = &mut d.cl.sel;
        ui.checkbox(&mut s.stopped_containers, "Stopped containers (exited, created, dead; running and paused ones are never touched)");
        ui.checkbox(&mut s.dangling_images, "Dangling images (untagged leftovers of rebuilds and pulls)");
        ui.checkbox(&mut s.unused_images, "Unused images (not used by any remaining container; can be pulled again)");
        ui.checkbox(&mut s.unused_networks, "Unused networks (custom networks with no container attached)");
        ui.checkbox(&mut s.build_cache, "Build cache (everything not in use by a running build)");
        ui.horizontal(|ui| {
            ui.label("Unused volumes:");
            ui.selectable_value(&mut s.volumes, VolumeScope::None, "keep all");
            ui.selectable_value(&mut s.volumes, VolumeScope::Anonymous, "anonymous only");
            ui.selectable_value(&mut s.volumes, VolumeScope::All, "anonymous and named");
        });
        if s.volumes != VolumeScope::None {
            theme::note(ui, Kind::Warning, "Volumes hold application data (databases, uploads...). Deleted volumes cannot be recovered. You will be asked to type a confirmation, and volumes used by any container are always kept.");
        }
    });
    if d.cl.sel != before {
        d.cl.plan = None;
    }
    ui.add_space(8.0);
    let busy = d.cl.plan_rx.is_some() || d.cl.run_rx.is_some();
    ui.horizontal(|ui| {
        let go = ui.add_enabled(!busy && !d.cl.sel.is_empty(), egui::Button::new(RichText::new("Preview cleanup").strong())).clicked();
        if go || (d.cl.auto_preview && !busy) {
            d.cl.auto_preview = false;
            if let Some(e) = d.engine() {
                let sel = d.cl.sel;
                d.cl.report = None;
                d.cl.plan_rx = Some(spawn(&ctx, move || make_plan(&e, sel)));
            }
        }
        if busy {
            ui.spinner();
            ui.label(if d.cl.run_rx.is_some() { "Cleaning up..." } else { "Building preview..." });
        }
    });
    if let Some(e) = &d.cl.plan_err {
        theme::note(ui, Kind::Error, e);
    }
    if let Some(r) = d.cl.report.clone() {
        report(ui, &r);
    }
    let Some(plan) = d.cl.plan.clone() else {
        if d.cl.report.is_none() {
            ui.label(RichText::new("Nothing is deleted without a preview and a confirmation.").color(p.text_dim));
        }
        return;
    };
    ui.add_space(8.0);
    theme::container(ui, "Preview", |ui| {
        if plan.items.is_empty() {
            theme::status(ui, Kind::Success, "Nothing to clean up for this selection.");
        } else {
            theme::big_value(ui, &format!("{} items would be deleted", plan.items.len()), &format!("≈ {}", human_bytes(plan.estimate())), None);
            ui.label(RichText::new("Estimate: image sizes exclude layers shared with kept images; build cache is reported exactly by the engine after pruning.").color(p.text_dim).small());
        }
        for cat in [Category::StoppedContainers, Category::DanglingImages, Category::UnusedImages, Category::UnusedNetworks, Category::UnusedVolumes, Category::BuildCache] {
            let items: Vec<_> = plan.items.iter().filter(|i| i.cat == cat).collect();
            if items.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(format!("{} — {} ({})", cat.label(), items.len(), human_bytes(plan.bytes(cat)))).id_salt(("dk_cl", cat.label())).default_open(cat == Category::UnusedVolumes || items.len() <= 8).show(ui, |ui| {
                egui::Grid::new(("dk_clg", cat.label())).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                    for it in items {
                        ui.label(RichText::new(&it.name).strong());
                        ui.label(it.size.map(human_bytes).unwrap_or_else(|| "?".into()));
                        ui.add(egui::Label::new(RichText::new(&it.detail).color(p.text_dim)).truncate());
                        match &it.warning {
                            Some(w) => super::kind_text(ui, Kind::Warning, w),
                            None => {
                                ui.label("");
                            }
                        }
                        ui.end_row();
                    }
                });
            });
        }
        if !plan.protected.is_empty() {
            ui.checkbox(&mut d.cl.show_protected, format!("Show {} protected objects that are kept", plan.protected.len()));
            if d.cl.show_protected {
                for (n, why) in &plan.protected {
                    ui.label(RichText::new(format!("kept: {n} — {why}")).color(p.text_dim));
                }
            }
        }
        if !plan.items.is_empty() {
            ui.add_space(6.0);
            if theme::danger_button(ui, &format!("Delete {} items...", plan.items.len())).clicked() {
                confirm(d, plan.clone());
            }
        }
    });
}

fn confirm(d: &mut DockerUi, plan: Plan) {
    let mut lines = vec![];
    for cat in [Category::StoppedContainers, Category::DanglingImages, Category::UnusedImages, Category::UnusedNetworks, Category::UnusedVolumes, Category::BuildCache] {
        let n = plan.count(cat);
        if n > 0 {
            lines.push(format!("• {}: {n} ({})", cat.label(), human_bytes(plan.bytes(cat))));
        }
    }
    lines.push(format!("Estimated space recovered: {}", human_bytes(plan.estimate())));
    lines.push("Items are deleted one by one without force: anything that started being used after the preview is refused by Docker and reported.".into());
    let vols = plan.has_volumes();
    let data: u64 = plan.items.iter().filter(|i| i.cat == Category::UnusedVolumes).filter_map(|i| i.size).sum();
    d.ask(Confirm {
        title: "Delete these Docker objects?".into(),
        lines,
        warning: if vols { Some(format!("{} volumes will be deleted permanently (at least {} of data). This cannot be undone.", plan.count(Category::UnusedVolumes), human_bytes(data))) } else { None },
        button: "Delete".into(),
        danger: true,
        typed: if vols { Some(VOLUME_WORD.into()) } else { None },
        input: String::new(),
        then: Box::new(move |d, ctx| {
            let Some(e) = d.engine() else { return };
            let root = d.inv.as_ref().map(|i| i.info.docker_root_dir.clone()).unwrap_or_default();
            d.cl.run_rx = Some(spawn(ctx, move || {
                let free = move || crate::docker::host_disk(&root).map(|(_, a)| a);
                // the typed confirmation above is what sets `confirm_volumes`
                let r = cleanup::execute(&e, &plan, vols, &free);
                audit::save_cleanup(r.reclaimed);
                r
            }));
        }),
    });
}

fn report(ui: &mut Ui, r: &Report) {
    let p = pal(ui);
    theme::container(ui, "Cleanup results", |ui| {
        theme::status(ui, if r.failed() == 0 { Kind::Success } else { Kind::Warning }, &format!("{} removed, {} failed", r.ok(), r.failed()));
        theme::kv(ui, "Space recovered (estimate + build cache)", human_bytes(r.reclaimed));
        if let (Some(a), Some(b)) = (r.host_free_before, r.host_free_after) {
            theme::kv(ui, "Free space on the Docker disk", format!("{} → {} ({}{})", human_bytes(a), human_bytes(b), if b >= a { "+" } else { "-" }, human_bytes(b.abs_diff(a))));
        }
        egui::ScrollArea::vertical().id_salt("dk_report").max_height(260.0).show(ui, |ui| {
            egui::Grid::new("dk_report_g").striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
                for o in &r.outcomes {
                    match &o.result {
                        Ok(()) => super::kind_text(ui, Kind::Success, "removed"),
                        Err(_) => super::kind_text(ui, Kind::Error, "failed"),
                    }
                    ui.label(o.cat.label());
                    ui.label(&o.name);
                    ui.label(RichText::new(o.result.as_ref().err().cloned().unwrap_or_else(|| o.size.map(human_bytes).unwrap_or_default())).color(p.text_dim));
                    ui.end_row();
                }
            });
        });
    });
}
