//! Overview: engine status, object counts, disk usage against the host disk, live resource charts.
use super::{age, DockerUi, Tab};
use crate::charts::{line_chart, stacked_bar, Series};
use crate::linux::{fmt_ts, human_bytes, Kind};
use crate::theme::{self, pal};
use eframe::egui::{self, RichText, Ui};

pub(super) fn show(d: &mut DockerUi, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    if d.df_stale {
        d.refresh_df(&ctx);
    }
    let Some(inv) = d.inv.clone() else {
        ui.spinner();
        return;
    };
    let p = pal(ui);
    let (i, v) = (&inv.info, &inv.version);
    let totals = d.df.as_ref().map(|x| x.totals());
    let host = super::super::host_disk(&i.docker_root_dir);
    let live = d.live.as_ref().map(|l| l.snapshot());
    let mut goto = None;
    theme::card_grid(ui, 250.0, 4, |ui, k| match k {
        0 => theme::container(ui, "Docker Engine", |ui| {
            theme::status(ui, Kind::Success, "Running");
            theme::kv(ui, "Version", v.version.clone());
            theme::kv(ui, "API version", format!("{} (min {})", v.api_version, v.min_api_version));
            theme::kv(ui, "Storage driver", i.driver.clone());
            theme::kv(ui, "Logging driver", i.logging_driver.clone());
            theme::kv(ui, "cgroup", format!("{} v{}", i.cgroup_driver, i.cgroup_version));
            theme::kv(ui, "Live restore", if i.live_restore_enabled { "enabled" } else { "disabled" });
            theme::kv(ui, "Last cleanup", d.state.last_cleanup.map(|t| format!("{} UTC", fmt_ts(t))).unwrap_or_else(|| "never".into()));
        }),
        1 => theme::container(ui, "Containers", |ui| {
            theme::big_value(ui, "total", &inv.containers.len().to_string(), None);
            let run = inv.count_state("running");
            let paused = inv.count_state("paused");
            let stopped = inv.containers.iter().filter(|c| c.is_stopped()).count();
            stacked_bar(ui, 10.0, &[("running", p.success, run as f64), ("paused", p.warning, paused as f64), ("stopped", p.text_dim, stopped as f64)], &|x| format!("{x:.0}"));
            let unhealthy = inv.containers.iter().filter(|c| c.health() == "unhealthy").count();
            if unhealthy > 0 {
                theme::status(ui, Kind::Error, &format!("{unhealthy} unhealthy"));
            }
            if ui.link("Manage containers").clicked() {
                goto = Some(Tab::Containers);
            }
        }),
        2 => theme::container(ui, "Objects", |ui| {
            theme::kv(ui, "Images", inv.images.len().to_string());
            theme::kv(ui, "Dangling images", inv.images.iter().filter(|x| x.dangling()).count().to_string());
            theme::kv(ui, "Volumes", inv.volumes.len().to_string());
            theme::kv(ui, "Networks", inv.networks.len().to_string());
            theme::kv(ui, "CPUs / memory", format!("{} / {}", i.ncpu, human_bytes(i.mem_total)));
        }),
        _ => theme::container(ui, "Disk", |ui| match (totals, host) {
            (Some(t), Some((htotal, havail))) => {
                let pct = if htotal == 0 { 0.0 } else { 100.0 * t.total() as f32 / htotal as f32 };
                theme::big_value(ui, "used by Docker", &human_bytes(t.total()), None);
                ui.label(RichText::new(format!("{pct:.1}% of the host disk · {} free on {}", human_bytes(havail), if i.docker_root_dir.is_empty() { "/var/lib/docker" } else { &i.docker_root_dir })).color(p.text_dim).small());
                let used_other = htotal.saturating_sub(havail).saturating_sub(t.total());
                stacked_bar(ui, 10.0, &[("Docker", p.series[0], t.total() as f64), ("other", p.text_dim, used_other as f64), ("free", p.success.gamma_multiply(0.5), havail as f64)], &|x| human_bytes(x as u64));
                theme::kv(ui, "Reclaimable", human_bytes(t.reclaimable()));
                if ui.link("Storage details").clicked() {
                    goto = Some(Tab::Storage);
                }
            }
            _ => {
                if let Some(e) = &d.df_err {
                    theme::status(ui, Kind::Error, e);
                } else {
                    ui.spinner();
                    ui.label("Measuring disk usage...");
                }
            }
        }),
    });
    if let Some(t) = totals {
        theme::container(ui, "Docker disk usage by type", |ui| {
            stacked_bar(ui, 18.0, &[("Images", p.series[0], t.images as f64), ("Containers", p.series[1], t.containers as f64), ("Volumes", p.series[2], t.volumes as f64), ("Build cache", p.series[3], t.build_cache as f64)], &|x| human_bytes(x as u64));
            ui.label(RichText::new(format!("{} reclaimable: images {}, containers {}, volumes {}, build cache {}", human_bytes(t.reclaimable()), human_bytes(t.images_reclaimable), human_bytes(t.containers_reclaimable), human_bytes(t.volumes_reclaimable), human_bytes(t.build_cache_reclaimable))).color(p.text_dim));
            if t.reclaimable() > 0 && ui.button("Review cleanup").clicked() {
                goto = Some(Tab::Cleanup);
            }
        });
        ui.add_space(8.0);
    }
    if let Some(l) = live {
        theme::card_grid(ui, 380.0, 2, |ui, k| {
            if k == 0 {
                theme::container(ui, "CPU, all running containers", |ui| {
                    line_chart(ui, 120.0, super::super::live::HIST, &[Series { name: "", color: p.series[0], data: &l.total_cpu }], None, &|v| format!("{v:.0}%"));
                    ui.label(RichText::new("100% = one CPU core").color(p.text_dim).small());
                });
            } else {
                theme::container(ui, "Memory, all running containers", |ui| {
                    line_chart(ui, 120.0, super::super::live::HIST, &[Series { name: "", color: p.series[2], data: &l.total_mem }], None, &|v| human_bytes(v as u64));
                    if let Some(e) = &l.stats_error {
                        theme::status(ui, Kind::Warning, e);
                    }
                });
            }
        });
        let mut top: Vec<_> = l.stats.iter().collect();
        top.sort_by(|a, b| super::ord(a.1.cpu_pct, b.1.cpu_pct, false));
        if !top.is_empty() {
            theme::container(ui, "Busiest containers", |ui| {
                egui::Grid::new("dk_top").striped(true).spacing([18.0, 6.0]).show(ui, |ui| {
                    for h in ["Container", "CPU", "Memory", "Network I/O", "Block I/O", "PIDs"] {
                        ui.label(RichText::new(h).strong().color(p.text_dim));
                    }
                    ui.end_row();
                    for (_, s) in top.iter().take(8) {
                        ui.label(&s.name);
                        ui.label(RichText::new(format!("{:.1}%", s.cpu_pct)).color(theme::usage_color(&p, s.cpu_pct / i.ncpu.max(1) as f32)));
                        ui.label(format!("{}{}", human_bytes(s.mem), if s.mem_limit > 0 && s.mem_limit < i.mem_total { format!(" / {}", human_bytes(s.mem_limit)) } else { String::new() }));
                        ui.label(format!("{} / {}", human_bytes(s.net_rx), human_bytes(s.net_tx)));
                        ui.label(format!("{} / {}", human_bytes(s.blk_read), human_bytes(s.blk_write)));
                        ui.label(s.pids.to_string());
                        ui.end_row();
                    }
                });
            });
        }
    }
    if !i.warnings.is_empty() {
        ui.add_space(8.0);
        theme::container(ui, "Engine warnings", |ui| {
            for w in &i.warnings {
                theme::status(ui, Kind::Warning, w);
            }
        });
    }
    ui.label(RichText::new(format!("Updated {}", age(inv.fetched as i64))).color(p.text_dim).small());
    if let Some(t) = goto {
        d.tab = t;
    }
}
