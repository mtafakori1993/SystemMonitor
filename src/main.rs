// Without the GUI the DNS/proxy/profile logic is only partly reachable (the CLI exposes status commands), so unused items are expected.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "gui")]
mod charts;
mod cli;
mod cpu;
#[cfg(feature = "gui")]
mod dashboard;
mod dns;
mod dnsenv;
mod gfx;
mod install;
mod linux;
mod memory;
mod monitor;
mod network;
mod processes;
mod proxy;
mod secrets;
mod settings;
mod shellenv;
mod smart;
mod storage;
mod store;
#[cfg(feature = "gui")]
mod theme;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("SystemMonitor {} ({})", env!("CARGO_PKG_VERSION"), build_flavor());
        return;
    }
    if args.iter().any(|a| a == "--install" || a == "--uninstall") {
        std::process::exit(install_cmd(args.iter().any(|a| a == "--install")));
    }
    if args.len() == 3 && args[1] == "--helper-etc-environment" {
        std::process::exit(proxy::helper_main(&args[2]));
    }
    if let Some(i) = args.iter().position(|a| a == "--cli") {
        std::process::exit(cli::run(&args[i + 1..]));
    }
    #[cfg(feature = "gui")]
    std::process::exit(launch::run(&args));
    #[cfg(not(feature = "gui"))]
    std::process::exit(cli::run(&args[1..]));
}

fn build_flavor() -> &'static str {
    match (cfg!(feature = "gui"), cfg!(target_env = "musl")) {
        (true, false) => "GUI, glibc",
        (true, true) => "GUI, musl",
        (false, false) => "core/CLI, glibc",
        (false, true) => "core/CLI, musl",
    }
}

#[cfg(feature = "gui")]
fn install_cmd(install: bool) -> i32 {
    match if install { install::install() } else { install::uninstall() } {
        Ok(m) => {
            println!("{m}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(not(feature = "gui"))]
fn install_cmd(_install: bool) -> i32 {
    eprintln!("This build has no GUI, so there is nothing to add to the application menu.");
    1
}

#[cfg(feature = "gui")]
mod launch {
    use crate::{app, cli, gfx, install, settings::Settings};
    use std::os::unix::process::CommandExt;

    /// Start the window following the graphics plan; never crashes when no backend works.
    pub fn run(args: &[String]) -> i32 {
        let settings = Settings::load();
        let mode = match gfx::requested_mode(args, &settings.graphics) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        let report = gfx::detect();
        if cfg!(all(target_env = "musl", target_feature = "crt-static")) && mode != gfx::Mode::Headless {
            // A static musl executable cannot dlopen libEGL/libwayland/libX11: retrying cannot help.
            eprintln!("This is a static musl build: the GUI libraries (OpenGL, Wayland, X11) cannot be loaded at run time.\nUse the dynamic glibc or dynamic musl GUI build for a window; showing the text-mode report instead.\n");
            return cli::run(&[]);
        }
        if mode == gfx::Mode::Headless {
            return cli::run(&[]);
        }
        if !report.has_display() {
            eprintln!("No graphical session detected; showing the text-mode report instead.");
            for p in report.problems() {
                eprintln!("  - {p}");
            }
            return cli::run(&[]);
        }
        let plan = report.plan(mode);
        let idx = gfx::current_stage_index().min(plan.len().saturating_sub(1));
        let stage = plan[idx];
        // Nothing has loaded OpenGL yet and no thread exists: safe to set the environment here.
        for (k, v) in report.stage_env(stage) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        gfx::set_stage(stage.name(), mode.name());
        let failed = match attempt(idx) {
            Ok(()) => return 0,
            Err(e) => e,
        };
        eprintln!("Graphics stage {}/{} ({}) failed: {failed}", idx + 1, plan.len(), stage.name());
        if idx + 1 < plan.len() {
            eprintln!("Retrying with: {}", plan[idx + 1].name());
            // Mesa/EGL keep per-process state, so the next stage starts from a fresh process.
            let err = std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "SystemMonitor".into())).args(&args[1..]).env("SYSMON_GFX_STAGE", (idx + 1).to_string()).exec();
            eprintln!("could not restart: {err}");
        }
        eprintln!("\nNo graphics backend could be initialized.");
        for l in report.describe() {
            eprintln!("  {l}");
        }
        let probs = report.problems();
        for p in &probs {
            eprintln!("  PROBLEM: {p}");
        }
        if probs.is_empty() {
            eprintln!("  No obvious cause found. Try `--gfx software`, or `--cli gfx` for details. Mesa install hint: {}", report.install_hint());
        }
        eprintln!("\nShowing the text-mode report instead (run with --gfx headless to skip the window attempt).\n");
        cli::run(&[]);
        3
    }

    fn attempt(idx: usize) -> Result<(), String> {
        if std::env::var("SYSMON_GFX_TEST_FAIL").ok().and_then(|v| v.parse::<usize>().ok()).map(|n| idx < n).unwrap_or(false) {
            return Err("simulated failure (SYSMON_GFX_TEST_FAIL)".into());
        }
        let opts = eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_title("System Monitor")
                .with_app_id("SystemMonitor")
                .with_inner_size([1280.0, 800.0])
                .with_min_inner_size([640.0, 480.0])
                .with_icon(std::sync::Arc::new(install::window_icon())),
            ..Default::default()
        };
        // Window-system and driver failures usually surface as errors, occasionally as panics.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| eframe::run_native("System Monitor", opts, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))));
        match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(p) => Err(p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic during graphics initialization".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{dns, linux, proxy};

    #[test]
    fn validation() {
        assert!(linux::valid_host("proxy.example.com") && linux::valid_host("10.0.0.1") && linux::valid_host("::1"));
        for bad in ["", "a b", "x;rm", "-a.com", "a..b", "a$b", "h'x"] {
            assert!(!linux::valid_host(bad), "{bad}");
        }
        assert_eq!(linux::parse_port("8080"), Some(8080));
        assert_eq!(linux::parse_port("0"), None);
        assert_eq!(linux::parse_port("70000"), None);
        assert!(linux::parse_dns_ip("1.1.1.1").is_ok() && linux::parse_dns_ip("2606:4700:4700::1111").is_ok());
        assert!(linux::parse_dns_ip("0.0.0.0").is_err() && linux::parse_dns_ip("224.0.0.1").is_err() && linux::parse_dns_ip("abc").is_err());
        assert!(linux::valid_bypass(".corp.example") && linux::valid_bypass("10.0.0.0/8") && !linux::valid_bypass("a b") && !linux::valid_bypass("$(x)"));
        assert_eq!(linux::fmt_ts(0), "1970-01-01 00:00:00");
        assert_eq!(linux::fmt_ts(1_761_876_000), "2025-10-31 02:00:00");
    }

    #[test]
    fn dns_profile_validation() {
        let mk = |v4: &[&str], v6: &[&str]| dns::DnsProfile { name: "x".into(), builtin: false, auto: false, v4: v4.iter().map(|s| s.to_string()).collect(), v6: v6.iter().map(|s| s.to_string()).collect() };
        assert!(dns::validate(&mk(&["9.9.9.9"], &["2620:fe::fe"])).is_ok());
        assert!(dns::validate(&mk(&[], &[])).is_err());
        assert!(dns::validate(&mk(&["::1"], &[])).is_err(), "v6 in v4 list");
        assert!(dns::validate(&mk(&["1.1.1.1; reboot"], &[])).is_err());
    }

    #[test]
    fn proxy_etc_helper() {
        let dir = std::env::temp_dir().join(format!("sysmon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (target, staged) = (dir.join("environment"), dir.join("staged"));
        std::fs::write(&target, "PATH=/usr/bin\nLANG=C\n").unwrap();
        unsafe { std::env::set_var("SYSMON_TEST_ETC_ENVIRONMENT", &target) };
        std::fs::write(&staged, "http_proxy=http://p.example:8080\nNO_PROXY=localhost,127.0.0.1\n").unwrap();
        assert_eq!(proxy::helper_main(staged.to_str().unwrap()), 0);
        let t = std::fs::read_to_string(&target).unwrap();
        assert!(t.contains("PATH=/usr/bin") && t.contains("http_proxy=http://p.example:8080") && t.contains("# BEGIN SystemMonitor proxy"));
        // replacing the block does not duplicate it
        assert_eq!(proxy::helper_main(staged.to_str().unwrap()), 0);
        assert_eq!(std::fs::read_to_string(&target).unwrap().matches("BEGIN SystemMonitor").count(), 1);
        // injection attempts are rejected and leave the file untouched
        let before = std::fs::read_to_string(&target).unwrap();
        for evil in ["PATH=/evil\n", "http_proxy=$(reboot)\n", "http_proxy=a b\n", "LD_PRELOAD=/x\n"] {
            std::fs::write(&staged, evil).unwrap();
            assert_ne!(proxy::helper_main(staged.to_str().unwrap()), 0, "{evil}");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
        // empty staged file removes the block
        std::fs::write(&staged, "").unwrap();
        assert_eq!(proxy::helper_main(staged.to_str().unwrap()), 0);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "PATH=/usr/bin\nLANG=C\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "needs network"]
    fn live_dns_query() {
        let a = dns::query("1.1.1.1".parse().unwrap(), "example.com", false, std::time::Duration::from_secs(3)).expect("query");
        assert_eq!(a.rcode, 0);
        assert!(!a.addrs.is_empty());
        println!("1.1.1.1 -> {:?} in {:?}", a.addrs, a.rtt);
        let a = dns::query("2606:4700:4700::1111".parse().unwrap(), "example.com", true, std::time::Duration::from_secs(3));
        println!("v6: {:?}", a.map(|a| (a.addrs, a.rtt)).map_err(|e| e));
        let nx = dns::query("1.1.1.1".parse().unwrap(), "no-such-name-zzqq.invalid", false, std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(nx.rcode, 3);
    }
}
