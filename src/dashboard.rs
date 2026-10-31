//! Dashboard overview page.
use crate::charts::{line_chart, Series};
use crate::linux::{human_bytes, human_duration, human_rate, Services};
use crate::monitor::Snapshot;
use crate::theme::{self, pal, Kind};
use eframe::egui::{self, RichText, Ui};

pub fn page(ui: &mut Ui, s: &Snapshot, services: &Services, proxy_active: &str, dns_active: &str) {
    let p = pal(ui);
    theme::page_title(ui, "Dashboard", &format!("{} · Linux {} · up {}", s.hostname, s.kernel, human_duration(s.uptime)));
    theme::card_grid(ui, 250.0, 4, |ui, i| match i {
        0 => theme::container(ui, "CPU", |ui| {
            theme::big_value(ui, &format!("{} · {} threads", if s.cpu.model.len() > 30 { &s.cpu.model[..30] } else { &s.cpu.model }, s.cpu.threads), &format!("{:.1}%", s.cpu.usage), Some(theme::usage_color(&p, s.cpu.usage)));
            line_chart(ui, 70.0, s.cap, &[Series { name: "", color: p.series[0], data: &s.cpu.hist }], Some(100.0), &|v| format!("{v:.0}%"));
            ui.label(RichText::new(format!("load {:.2} {:.2} {:.2}{}", s.cpu.load[0], s.cpu.load[1], s.cpu.load[2], s.cpu.temp.map(|t| format!(" · {t:.0} °C")).unwrap_or_default())).color(p.text_dim));
        }),
        1 => theme::container(ui, "Memory", |ui| {
            theme::big_value(ui, &format!("{} of {}", human_bytes(s.mem.used), human_bytes(s.mem.total)), &format!("{:.1}%", s.mem.pct()), Some(theme::usage_color(&p, s.mem.pct())));
            line_chart(ui, 70.0, s.cap, &[Series { name: "", color: p.series[2], data: &s.mem.hist }], Some(100.0), &|v| format!("{v:.0}%"));
            ui.label(RichText::new(format!("available {} · swap {:.0}%", human_bytes(s.mem.available), s.mem.swap_pct())).color(p.text_dim));
        }),
        2 => theme::container(ui, "Network", |ui| {
            theme::big_value(ui, "Download", &human_rate(s.net.rx_bps), Some(p.series[0]));
            line_chart(ui, 70.0, s.cap, &[Series { name: "Down", color: p.series[0], data: &s.net.hist_rx }, Series { name: "Up", color: p.series[1], data: &s.net.hist_tx }], None, &|v| human_rate(v as f64));
            ui.label(RichText::new(format!("upload {}", human_rate(s.net.tx_bps))).color(p.text_dim));
        }),
        _ => theme::container(ui, "Storage", |ui| {
            let (tot, used): (u64, u64) = s.storage.fs.iter().fold((0, 0), |a, f| (a.0 + f.total, a.1 + f.used));
            let pct = if tot == 0 { 0.0 } else { 100.0 * used as f32 / tot as f32 };
            theme::big_value(ui, &format!("{} of {}", human_bytes(used), human_bytes(tot)), &format!("{pct:.0}%"), Some(theme::usage_color(&p, pct)));
            line_chart(ui, 70.0, s.cap, &[Series { name: "R", color: p.series[0], data: &s.storage.hist_r }, Series { name: "W", color: p.series[1], data: &s.storage.hist_w }], None, &|v| human_rate(v as f64));
            ui.label(RichText::new(format!("{} filesystems", s.storage.fs.len())).color(p.text_dim));
        }),
    });
    theme::card_grid(ui, 420.0, 2, |ui, i| {
        if i == 0 {
            theme::container(ui, "Top processes by CPU", |ui| {
                let mut v: Vec<_> = s.procs.list.iter().collect();
                v.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
                egui::Grid::new("topproc").num_columns(4).spacing([16.0, 6.0]).show(ui, |ui| {
                    for h in ["PID", "Name", "CPU", "Memory"] {
                        ui.label(RichText::new(h).strong().color(p.text_dim));
                    }
                    ui.end_row();
                    for pr in v.iter().take(6) {
                        ui.label(pr.pid.to_string());
                        ui.label(if pr.name.len() > 18 { &pr.name[..18] } else { &pr.name });
                        ui.label(format!("{:.1}%", pr.cpu));
                        ui.label(human_bytes(pr.mem));
                        ui.end_row();
                    }
                });
            });
        } else {
            theme::container(ui, "Network configuration & services", |ui| {
                theme::kv(ui, "Proxy profile", proxy_active);
                theme::kv(ui, "DNS profile", dns_active);
                theme::kv(ui, "DNS servers", if s.net.dns.is_empty() { "—".into() } else { s.net.dns.join(", ") });
                ui.add_space(4.0);
                for (name, ok, _) in crate::settings::service_rows(services).into_iter().take(4) {
                    theme::status(ui, if ok { Kind::Success } else { Kind::Inactive }, &format!("{name}: {}", if ok { "available" } else { "unavailable" }));
                }
            });
        }
    });
}
