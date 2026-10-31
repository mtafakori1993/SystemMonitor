//! Text-mode interface: works in every build (also the static, GUI-less one) and on machines without a display.
use crate::linux::{self, human_bytes, human_duration, human_rate};
use crate::monitor::{self, Config};
use std::sync::Arc;
use std::time::Duration;

pub const USAGE: &str = "\
System Monitor (text mode)

  SystemMonitor --cli [command]

Commands:
  status         one-shot CPU / memory / network / storage summary (default)
  json           the same as JSON
  services       which host services were detected (NetworkManager, systemd-resolved, ...)
  gfx            graphics diagnostics: GPU, OpenGL/Mesa libraries, software renderers, install hints
  smart [dev]    disk health through smartctl (read-only); e.g. smart sda
  dns            DNS stack, conflicts and persistence report
  proxy          proxy status: GNOME gsettings/dconf probe, session environment, shell hooks
  proxy-env SH   print proxy environment for bash|zsh|fish (use: eval \"$(SystemMonitor --cli proxy-env bash)\")
  shell-setup    status | install | remove [bash] [zsh] [fish]: managed, reversible start-up-file block
  docker [what]  Docker engine: status (default) | df | containers | images | preview [-a] [--volumes] | events | audit
  help           this text
";

pub fn run(args: &[String]) -> i32 {
    let cmd = args.first().map(String::as_str).unwrap_or("status");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match cmd {
        "status" => status(false),
        "json" => status(true),
        "services" => services(),
        "gfx" => gfx(),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            0
        }
        other => extra(other, rest),
    }
}

fn snapshot_once() -> Arc<monitor::Snapshot> {
    let cfg = Arc::new(Config { interval_ms: 250.into(), history: 30.into() });
    let s = monitor::start(|| {}, cfg);
    // rates need two samples
    std::thread::sleep(Duration::from_millis(700));
    s.snapshot()
}

fn status(json: bool) -> i32 {
    let s = snapshot_once();
    if json {
        let v = serde_json::json!({
            "hostname": s.hostname, "kernel": s.kernel, "uptime_s": s.uptime,
            "cpu": {"model": s.cpu.model, "threads": s.cpu.threads, "usage_pct": s.cpu.usage, "load": s.cpu.load, "temp_c": s.cpu.temp},
            "memory": {"total": s.mem.total, "used": s.mem.used, "swap_total": s.mem.swap_total, "swap_used": s.mem.swap_used},
            "network": s.net.ifaces.iter().map(|i| serde_json::json!({"name": i.name, "state": i.state, "rx_bps": i.rx_bps, "tx_bps": i.tx_bps})).collect::<Vec<_>>(),
            "filesystems": s.storage.fs.iter().map(|f| serde_json::json!({"device": f.device, "mount": f.mount, "type": f.fstype, "total": f.total, "used": f.used})).collect::<Vec<_>>(),
            "devices": s.storage.devices.iter().map(|d| serde_json::json!({"name": d.name, "model": d.model, "size": d.size, "kind": d.kind, "temp_c": d.temp})).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    println!("{} · Linux {} · up {}", s.hostname, s.kernel, human_duration(s.uptime));
    println!("CPU     {} ({} threads) {:.1}%  load {:.2} {:.2} {:.2}{}", s.cpu.model, s.cpu.threads, s.cpu.usage, s.cpu.load[0], s.cpu.load[1], s.cpu.load[2], s.cpu.temp.map(|t| format!("  {t:.0} °C")).unwrap_or_default());
    println!("Memory  {} of {} ({:.1}%)  swap {} of {}", human_bytes(s.mem.used), human_bytes(s.mem.total), s.mem.pct(), human_bytes(s.mem.swap_used), human_bytes(s.mem.swap_total));
    for i in s.net.ifaces.iter().filter(|i| i.name != "lo") {
        println!("Net     {:<12} {:<8} ↓ {:>12} ↑ {:>12}", i.name, i.state, human_rate(i.rx_bps), human_rate(i.tx_bps));
    }
    for f in &s.storage.fs {
        println!("Disk    {:<24} {:<8} {} used of {}", f.mount, f.fstype, human_bytes(f.used), human_bytes(f.total));
    }
    for d in &s.storage.devices {
        println!("Device  /dev/{:<10} {:<9} {:>10} {}{}", d.name, d.kind, human_bytes(d.size), d.model, d.temp.map(|t| format!("  {t:.0} °C")).unwrap_or_default());
    }
    0
}

fn services() -> i32 {
    let s = linux::detect_services();
    for (name, ok, why) in crate::settings::service_rows(&s) {
        println!("{:<28} {}", name, if ok { "available" } else { why });
    }
    0
}

fn gfx() -> i32 {
    let r = crate::gfx::detect();
    for l in r.describe() {
        println!("{l}");
    }
    let plan = r.plan(crate::gfx::Mode::Auto);
    println!("Auto plan: {}", if plan.is_empty() { "none".into() } else { plan.iter().map(|s| s.name()).collect::<Vec<_>>().join(" → ") });
    let p = r.problems();
    if p.is_empty() {
        println!("No problems found.");
        0
    } else {
        for l in &p {
            println!("PROBLEM: {l}");
        }
        1
    }
}

fn smart(rest: &[String]) -> i32 {
    use crate::smart::{self, State};
    let devs: Vec<String> = if rest.is_empty() { snapshot_once().storage.devices.iter().map(|d| d.name.clone()).collect() } else { rest.to_vec() };
    let mut code = 0;
    for d in devs {
        let r = smart::check(&d, false);
        println!("/dev/{d}: {}", r.headline());
        if !r.model.is_empty() {
            println!("  model {} firmware {}", r.model, r.firmware);
        }
        for (k, v) in [("temperature °C", r.temperature_c.map(|v| v.to_string())), ("power-on hours", r.power_on_hours.map(|v| v.to_string())), ("endurance used %", r.wear_used_pct.map(|v| v.to_string())), ("spare %", r.spare_pct.map(|v| v.to_string()))] {
            if let Some(v) = v {
                println!("  {k}: {v}");
            }
        }
        for w in &r.warnings {
            println!("  warning: {w}");
        }
        match r.state {
            State::Failing => code = 1,
            State::Unavailable(_) if code == 0 => code = 0,
            _ => {}
        }
    }
    code
}

fn proxy_env(rest: &[String]) -> i32 {
    use crate::shellenv::{export_script, Shell};
    let Some(sh) = rest.first().and_then(|s| Shell::parse(s)) else {
        eprintln!("usage: SystemMonitor --cli proxy-env bash|zsh|fish\n  eval \"$(SystemMonitor --cli proxy-env bash)\"   # bring this terminal up to date\n  SystemMonitor --cli proxy-env fish | source");
        return 2;
    };
    print!("{}", export_script(sh, &crate::proxy::session_env()));
    0
}

fn shell_setup(rest: &[String]) -> i32 {
    use crate::shellenv::{self, Dirs, Shell};
    let d = Dirs::current();
    let action = rest.first().map(String::as_str).unwrap_or("status");
    let mut shells: Vec<Shell> = rest.iter().skip(1).filter_map(|s| Shell::parse(s)).collect();
    if shells.is_empty() {
        shells = shellenv::ALL.iter().copied().filter(|s| s.installed()).collect();
    }
    let mut code = 0;
    match action {
        "status" => shellenv::status(&d).iter().for_each(|l| println!("{l}")),
        "install" | "remove" => {
            for s in shells {
                match if action == "install" { shellenv::install(s, &d) } else { shellenv::remove(s, &d) } {
                    Ok(m) => println!("{m}"),
                    Err(e) => {
                        eprintln!("{e}");
                        code = 1;
                    }
                }
            }
            if action == "install" {
                println!("Already-open terminals do not change by themselves (a process cannot alter its parent's environment).\nIn each one run:  eval \"$(SystemMonitor --cli proxy-env bash)\"   (zsh: same; fish: SystemMonitor --cli proxy-env fish | source)");
            }
        }
        _ => {
            eprintln!("usage: SystemMonitor --cli shell-setup [status|install|remove] [bash] [zsh] [fish]");
            code = 2;
        }
    }
    code
}

fn proxy_status() -> i32 {
    let d = linux::desktop();
    let g = crate::proxy::gnome_probe(&d);
    println!("GNOME session: {}   gsettings: {}   schema org.gnome.system.proxy: {}   backend: {}   dconf module: {}", g.gnome_session, g.gsettings, g.schema, g.backend, g.dconf_module);
    println!("GNOME proxy writes: {}", if g.usable() { "usable (each write is read back and rolled back on mismatch)" } else { "NOT usable" });
    for p in &g.problems {
        println!("  problem: {p}");
    }
    let env = crate::proxy::session_env();
    println!("Session environment (new processes): {}", if env.is_empty() { "no proxy".to_string() } else { env.iter().filter(|(k, _)| k.chars().all(|c| c.is_lowercase() || c == '_')).map(|(k, v)| format!("{k}={}", crate::proxy::mask(v))).collect::<Vec<_>>().join(" ") });
    for l in crate::shellenv::status(&crate::shellenv::Dirs::current()) {
        println!("Shell {l}");
    }
    0
}

fn dns_report() -> i32 {
    let s = linux::detect_services();
    let env = crate::dnsenv::detect(&s);
    for l in env.describe() {
        println!("{l}");
    }
    if env.conflicts.is_empty() {
        println!("No conflicts detected.");
        return 0;
    }
    for c in &env.conflicts {
        println!("CONFLICT: {c}");
    }
    1
}

fn extra(cmd: &str, rest: &[String]) -> i32 {
    match cmd {
        "dns" => return dns_report(),
        "smart" => return smart(rest),
        "proxy-env" => return proxy_env(rest),
        "shell-setup" => return shell_setup(rest),
        "proxy" => return proxy_status(),
        "docker" => return crate::docker::cli::run(rest),
        _ => {}
    }
    eprintln!("unknown command '{cmd}'\n\n{USAGE}");
    2
}
