//! Per-user launcher integration: application-menu entry, icon, and a stable copy of the binary.
use crate::linux::{home, run, which};
use std::fs;
use std::path::PathBuf;

const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#0f141a"/><rect x="8" y="8" width="48" height="48" rx="8" fill="#161d26" stroke="#429ae8" stroke-width="2"/><path d="M12 42 L22 30 L30 36 L40 20 L52 28" fill="none" stroke="#429ae8" stroke-width="3.5" stroke-linecap="round" stroke-linejoin="round"/><rect x="14" y="46" width="8" height="6" rx="1.5" fill="#2ea597"/><rect x="26" y="44" width="8" height="8" rx="1.5" fill="#e07941"/><rect x="38" y="47" width="8" height="5" rx="1.5" fill="#e0a41a"/></svg>"##;

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".local/share"))
}
fn bin_path() -> PathBuf {
    home().join(".local/bin/SystemMonitor")
}
fn desktop_path() -> PathBuf {
    data_home().join("applications/SystemMonitor.desktop")
}
fn icon_path() -> PathBuf {
    data_home().join("icons/hicolor/scalable/apps/SystemMonitor.svg")
}

pub fn is_installed() -> bool {
    desktop_path().exists()
}

pub fn install() -> Result<String, String> {
    let exe = std::env::current_exe().and_then(fs::canonicalize).map_err(|e| e.to_string())?;
    let dest = bin_path();
    if exe != dest {
        fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = dest.with_extension("new");
        fs::copy(&exe, &tmp).map_err(|e| format!("copy to {}: {e}", dest.display()))?;
        fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(icon_path().parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(icon_path(), ICON_SVG).map_err(|e| e.to_string())?;
    let exec = dest.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=System Monitor\nGenericName=System Monitor & Network Manager\nComment=CPU, memory, network, storage, processes, proxy and DNS manager\nExec=\"{exec}\"\nIcon=SystemMonitor\nTerminal=false\nCategories=System;Monitor;\nKeywords=cpu;memory;network;proxy;dns;process;\nStartupWMClass=SystemMonitor\nActions=Proxy;Dns;\n\n[Desktop Action Proxy]\nName=Proxy Manager\nExec=\"{exec}\" --page proxy\n\n[Desktop Action Dns]\nName=DNS Manager\nExec=\"{exec}\" --page dns\n"
    );
    fs::create_dir_all(desktop_path().parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(desktop_path(), entry).map_err(|e| e.to_string())?;
    refresh_caches();
    Ok(format!("Installed: application menu entry \"System Monitor\" and {}", dest.display()))
}

pub fn uninstall() -> Result<String, String> {
    for p in [desktop_path(), icon_path(), bin_path()] {
        if p.exists() && std::env::current_exe().and_then(fs::canonicalize).map(|e| e != p).unwrap_or(true) {
            fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    let _ = fs::remove_file(desktop_path());
    refresh_caches();
    Ok("Launcher entry removed".into())
}

fn refresh_caches() {
    if which("update-desktop-database").is_some() {
        let _ = run("update-desktop-database", &[&data_home().join("applications").to_string_lossy()]);
    }
    for k in ["kbuildsycoca6", "kbuildsycoca5"] {
        if which(k).is_some() {
            let _ = run(k, &[]);
            break;
        }
    }
    if which("gtk-update-icon-cache").is_some() {
        let _ = run("gtk-update-icon-cache", &["-f", "-t", &data_home().join("icons/hicolor").to_string_lossy()]);
    }
}

#[cfg(feature = "gui")]
/// 64x64 RGBA window icon, drawn procedurally (same motif as the SVG).
pub fn window_icon() -> eframe::egui::IconData {
    let n = 64usize;
    let mut px = vec![0u8; n * n * 4];
    let put = |px: &mut Vec<u8>, x: usize, y: usize, c: [u8; 4]| {
        let i = (y * n + x) * 4;
        px[i..i + 4].copy_from_slice(&c);
    };
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = ((x as f32 - 31.5).abs() - 17.5, (y as f32 - 31.5).abs() - 17.5);
            let d = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - 14.0;
            if d < 0.0 {
                put(&mut px, x, y, [0x16, 0x1d, 0x26, 255]);
                if d > -2.5 {
                    put(&mut px, x, y, [0x42, 0x9a, 0xe8, 255]);
                }
            }
        }
    }
    let pts = [(12.0, 42.0), (22.0, 30.0), (30.0, 36.0), (40.0, 20.0), (52.0, 28.0)];
    for w in pts.windows(2) {
        for s in 0..=60 {
            let t = s as f32 / 60.0;
            let (cx, cy) = (w[0].0 + (w[1].0 - w[0].0) * t, w[0].1 + (w[1].1 - w[0].1) * t);
            for oy in -2i32..=2 {
                for ox in -2i32..=2 {
                    if ox * ox + oy * oy <= 5 {
                        put(&mut px, (cx as i32 + ox).clamp(0, 63) as usize, (cy as i32 + oy).clamp(0, 63) as usize, [0x42, 0x9a, 0xe8, 255]);
                    }
                }
            }
        }
    }
    for (x0, h, c) in [(14usize, 6usize, [0x2e, 0xa5, 0x97, 255]), (26, 8, [0xe0, 0x79, 0x41, 255]), (38, 5, [0xe0, 0xa4, 0x1a, 255])] {
        for y in (52 - h)..52 {
            for x in x0..x0 + 8 {
                put(&mut px, x, y, c);
            }
        }
    }
    eframe::egui::IconData { rgba: px, width: n as u32, height: n as u32 }
}
