//! `SystemMonitor --cli docker …`: read-only Docker reports for headless machines and scripts.
//! Nothing here deletes or changes anything; `preview` only shows what a cleanup would remove.
use super::api::{short_id, Engine};
use super::cleanup::{self, Inputs, Selection, VolumeScope};
use super::{audit, probe, Access, Inventory};
use crate::linux::{fmt_ts, human_bytes};

fn engine() -> Result<Engine, i32> {
    match probe() {
        Access::Ready(e) => Ok(e),
        Access::NotInstalled => {
            eprintln!("Docker is not installed.");
            Err(3)
        }
        Access::NotRunning(s) => {
            eprintln!("Docker is not running ({s}). Start it with: sudo systemctl start docker");
            Err(3)
        }
        Access::Denied(s) => {
            eprintln!("Permission denied on the Docker socket ({s}).\nOnly root and members of the 'docker' group (which is root-equivalent) may manage Docker.");
            Err(4)
        }
        Access::Failed(s) => {
            eprintln!("{s}");
            Err(1)
        }
    }
}

pub fn run(rest: &[String]) -> i32 {
    let what = rest.first().map(String::as_str).unwrap_or("status");
    if what == "audit" {
        for e in audit::recent(50) {
            println!("{}  {:<10} {:<4} {:<40} {}{}", fmt_ts(e.ts), e.user, if e.ok { "ok" } else { "FAIL" }, e.action, e.target, if e.error.is_empty() { String::new() } else { format!("  ({})", e.error) });
        }
        return 0;
    }
    let e = match engine() {
        Ok(e) => e,
        Err(c) => return c,
    };
    let r = match what {
        "status" => status(&e),
        "df" => df(&e),
        "containers" => containers(&e),
        "images" => images(&e),
        "preview" => preview(&e, rest),
        "events" => events(&e),
        _ => {
            eprintln!("usage: SystemMonitor --cli docker [status|df|containers|images|preview [-a] [--volumes]|events|audit]");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

fn status(e: &Engine) -> Result<(), super::Error> {
    let inv = Inventory::fetch(e)?;
    let (v, i) = (&inv.version, &inv.info);
    println!("Docker Engine {} (API {}, using {}) on {} · storage driver {} · logging {}", v.version, v.api_version, e.api, i.operating_system, i.driver, i.logging_driver);
    println!("Containers {} (running {}, paused {}, stopped {})  Images {}  Volumes {}  Networks {}", i.containers, i.containers_running, i.containers_paused, i.containers_stopped, inv.images.len(), inv.volumes.len(), inv.networks.len());
    if let Some((total, avail)) = super::host_disk(&i.docker_root_dir) {
        println!("Data root {}: {} free of {}", i.docker_root_dir, human_bytes(avail), human_bytes(total));
    }
    if !i.http_proxy.is_empty() || !i.https_proxy.is_empty() {
        println!("Daemon proxy: http={} https={} no_proxy={}", i.http_proxy, i.https_proxy, i.no_proxy);
    }
    let st = audit::load_state();
    println!("Last cleanup: {}", st.last_cleanup.map(|t| format!("{} UTC ({} reclaimed)", fmt_ts(t), human_bytes(st.last_reclaimed))).unwrap_or_else(|| "never (from this program)".into()));
    for w in &i.warnings {
        println!("WARNING: {w}");
    }
    Ok(())
}

fn df(e: &Engine) -> Result<(), super::Error> {
    let d = e.disk_usage()?;
    let t = d.totals();
    println!("{:<14} {:>12} {:>14}", "TYPE", "SIZE", "RECLAIMABLE");
    for (k, a, b) in [("Images", t.images, t.images_reclaimable), ("Containers", t.containers, t.containers_reclaimable), ("Local volumes", t.volumes, t.volumes_reclaimable), ("Build cache", t.build_cache, t.build_cache_reclaimable)] {
        println!("{k:<14} {:>12} {:>14}", human_bytes(a), human_bytes(b));
    }
    println!("{:<14} {:>12} {:>14}", "Total", human_bytes(t.total()), human_bytes(t.reclaimable()));
    Ok(())
}

fn containers(e: &Engine) -> Result<(), super::Error> {
    for c in e.containers()? {
        println!("{:<13} {:<28} {:<10} {:<30} {}", short_id(&c.id), c.name(), c.state, c.image, c.ports_text());
    }
    Ok(())
}

fn images(e: &Engine) -> Result<(), super::Error> {
    for i in e.images()? {
        for (r, t) in i.repo_tag_pairs() {
            println!("{:<13} {:<40} {:<16} {:>10}", short_id(&i.id), r, t, human_bytes(i.size.max(0) as u64));
        }
    }
    Ok(())
}

fn preview(e: &Engine, rest: &[String]) -> Result<(), super::Error> {
    let mut sel = Selection::system_prune();
    if rest.iter().any(|a| a == "-a" || a == "--all") {
        sel.unused_images = true;
    }
    if rest.iter().any(|a| a == "--volumes") {
        sel.volumes = VolumeScope::Anonymous;
    }
    let inv = Inventory::fetch(e)?;
    let df = e.disk_usage()?;
    let p = cleanup::plan(sel, &Inputs { containers: &inv.containers, images: &inv.images, networks: &inv.networks, volumes: &inv.volumes, df: Some(&df) });
    println!("Preview only: nothing is deleted. Run the cleanup from the Docker page of the GUI.\n");
    for it in &p.items {
        println!("{:<20} {:<40} {:>10}  {}{}", it.cat.label(), it.name, it.size.map(human_bytes).unwrap_or_else(|| "?".into()), it.detail, it.warning.as_ref().map(|w| format!("  WARNING: {w}")).unwrap_or_default());
    }
    for (n, why) in &p.protected {
        println!("{:<20} {:<40} kept: {why}", "protected", n);
    }
    println!("\n{} items, about {} reclaimable", p.items.len(), human_bytes(p.estimate()));
    Ok(())
}

fn events(e: &Engine) -> Result<(), super::Error> {
    let mut s = e.events(crate::linux::now_secs().saturating_sub(3600))?;
    println!("Docker events (last hour, then live; Ctrl+C to stop)");
    let mut line = String::new();
    loop {
        line.clear();
        if s.reader.read_line(&mut line).map_err(super::Error::from)? == 0 {
            return Ok(());
        }
        if let Ok(ev) = serde_json::from_str::<super::api::Event>(line.trim()) {
            println!("{}  {:<10} {:<16} {}", fmt_ts(ev.time.max(0) as u64), ev.kind, ev.action, ev.name());
        }
    }
}
