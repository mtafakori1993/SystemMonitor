//! Graphics environment detection and fallback planning (no GUI dependency).
//!
//! The GUI needs an OpenGL context. This module inspects the machine *without* loading any
//! graphics library (it only looks at /sys, /dev and library directories), decides which
//! initialization stages to try (hardware, then Mesa software rendering, then X11 on a
//! Wayland session), and produces human-readable diagnostics and install hints.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Try hardware first, then software renderers (default).
    Auto,
    /// Hardware only; fail with diagnostics instead of falling back.
    Hardware,
    /// Force a Mesa software renderer (llvmpipe/softpipe).
    Software,
    /// Never open a window; print a text report.
    Headless,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Mode::Auto),
            "hardware" | "hw" | "gpu" => Some(Mode::Hardware),
            "software" | "sw" | "llvmpipe" => Some(Mode::Software),
            "headless" | "none" | "cli" => Some(Mode::Headless),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Hardware => "hardware",
            Mode::Software => "software",
            Mode::Headless => "headless",
        }
    }
}

/// One initialization attempt. Applied by re-executing the program with the stage's environment,
/// because Mesa reads its driver selection once per process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Hardware,
    Software,
    /// Software rendering through X11/XWayland (Wayland EGL can fail where X11 GLX works).
    SoftwareX11,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Stage::Hardware => "hardware OpenGL",
            Stage::Software => "Mesa software renderer",
            Stage::SoftwareX11 => "Mesa software renderer via X11",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub card: String,
    pub driver: String,
    pub vendor: String,
    pub virtual_kind: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub wayland: bool,
    pub x11: bool,
    pub session_type: String,
    pub gpus: Vec<Gpu>,
    pub render_nodes: Vec<String>,
    pub hypervisor: Option<String>,
    pub egl: Option<PathBuf>,
    pub gl: Option<PathBuf>,
    pub dri_drivers: Vec<PathBuf>,
    pub llvm: bool,
    pub wayland_libs: bool,
    pub x11_libs: bool,
    pub xkb: bool,
    pub xkb_x11: bool,
    pub distro: Distro,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Distro {
    Debian,
    Fedora,
    Arch,
    Alpine,
    Suse,
    #[default]
    Unknown,
}

pub fn lib_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = vec![];
    if let Some(p) = std::env::var_os("SYSMON_LIB_DIRS") {
        // test hook: replaces the search path
        return std::env::split_paths(&p).collect();
    }
    if let Some(p) = std::env::var_os("LD_LIBRARY_PATH") {
        v.extend(std::env::split_paths(&p));
    }
    for d in [
        "/usr/lib64", "/usr/lib", "/lib64", "/lib", "/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu", "/usr/lib/arm-linux-gnueabihf",
        "/usr/lib/riscv64-linux-gnu", "/usr/local/lib", "/usr/local/lib64",
    ] {
        v.push(d.into());
    }
    v
}

fn find_lib(dirs: &[PathBuf], prefix: &str) -> Option<PathBuf> {
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        let mut hits: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with(prefix)).unwrap_or(false)).collect();
        hits.sort();
        if let Some(h) = hits.into_iter().next() {
            return Some(h);
        }
    }
    None
}

fn dri_dirs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![];
    for d in dirs {
        for sub in ["dri", "gallium-pipe"] {
            let p = d.join(sub);
            if p.is_dir() {
                out.push(p);
            }
        }
        // Mesa >= 25 ships its drivers in a single libgallium-<ver>.so
        if let Ok(rd) = std::fs::read_dir(d) {
            out.extend(rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("libgallium-")).unwrap_or(false)));
        }
    }
    out
}

fn read(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn vendor_name(id: &str) -> &'static str {
    match id {
        "0x10de" => "NVIDIA",
        "0x1002" | "0x1022" => "AMD",
        "0x8086" => "Intel",
        "0x1af4" => "Red Hat (virtio)",
        "0x15ad" => "VMware",
        "0x1234" => "QEMU/Bochs",
        "0x80ee" => "VirtualBox",
        "0x1414" => "Microsoft Hyper-V",
        "0x1b36" => "Red Hat (QXL)",
        "0x1013" => "Cirrus",
        _ => "unknown vendor",
    }
}

fn virtual_kind(driver: &str, vendor: &str) -> Option<&'static str> {
    match driver {
        "virtio-pci" | "virtio_gpu" | "virtio-gpu" => Some("virtio-gpu"),
        "vmwgfx" => Some("VMware SVGA"),
        "qxl" => Some("QXL"),
        "bochs-drm" | "bochs" => Some("Bochs/QEMU std VGA"),
        "vboxvideo" | "vboxsf" => Some("VirtualBox"),
        "hyperv_drm" | "hyperv-fb" => Some("Hyper-V"),
        "simpledrm" | "simple-framebuffer" | "efifb" | "vesafb" => Some("firmware framebuffer (no GPU driver)"),
        "cirrus" | "cirrus-qemu" => Some("Cirrus"),
        "ast" | "mgag200" => None, // server BMC graphics are real, but have no 3D
        _ if vendor == "0x1af4" || vendor == "0x15ad" || vendor == "0x80ee" || vendor == "0x1234" => Some("virtual GPU"),
        _ => None,
    }
}

pub fn detect_distro() -> Distro {
    let osr = read("/etc/os-release").to_lowercase();
    let ids: String = osr.lines().filter(|l| l.starts_with("id=") || l.starts_with("id_like=")).collect::<Vec<_>>().join(" ");
    let has = |w: &[&str]| w.iter().any(|x| ids.contains(x));
    if has(&["alpine"]) {
        Distro::Alpine
    } else if has(&["debian", "ubuntu", "mint"]) {
        Distro::Debian
    } else if has(&["fedora", "rhel", "centos", "almalinux", "rocky"]) {
        Distro::Fedora
    } else if has(&["arch", "manjaro"]) {
        Distro::Arch
    } else if has(&["suse"]) {
        Distro::Suse
    } else {
        Distro::Unknown
    }
}

fn hypervisor() -> Option<String> {
    let v = read("/sys/class/dmi/id/sys_vendor").to_lowercase() + " " + &read("/sys/class/dmi/id/product_name").to_lowercase();
    for (k, n) in [("qemu", "QEMU/KVM"), ("kvm", "KVM"), ("vmware", "VMware"), ("virtualbox", "VirtualBox"), ("microsoft corporation virtual", "Hyper-V"), ("xen", "Xen"), ("parallels", "Parallels"), ("bochs", "Bochs"), ("amazon ec2", "AWS"), ("google compute", "Google Cloud")] {
        if v.contains(k) {
            return Some(n.to_string());
        }
    }
    if read("/proc/cpuinfo").lines().any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor")) {
        return Some("unknown hypervisor".into());
    }
    None
}

pub fn detect() -> Report {
    let dirs = lib_dirs();
    let mut r = Report {
        wayland: std::env::var_os("WAYLAND_DISPLAY").map(|v| !v.is_empty()).unwrap_or(false),
        x11: std::env::var_os("DISPLAY").map(|v| !v.is_empty()).unwrap_or(false),
        session_type: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        hypervisor: hypervisor(),
        egl: find_lib(&dirs, "libEGL.so"),
        gl: find_lib(&dirs, "libGL.so.1").or_else(|| find_lib(&dirs, "libGLX.so")),
        dri_drivers: dri_dirs(&dirs),
        llvm: find_lib(&dirs, "libLLVM").is_some(),
        wayland_libs: find_lib(&dirs, "libwayland-client.so").is_some() && find_lib(&dirs, "libwayland-egl.so").is_some(),
        x11_libs: find_lib(&dirs, "libX11.so").is_some() && find_lib(&dirs, "libxcb.so").is_some(),
        xkb: find_lib(&dirs, "libxkbcommon.so").is_some(),
        xkb_x11: find_lib(&dirs, "libxkbcommon-x11.so").is_some(),
        distro: detect_distro(),
        ..Default::default()
    };
    if let Ok(rd) = std::fs::read_dir("/sys/class/drm") {
        let mut cards: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("card") && !n.contains('-')).collect();
        cards.sort();
        for c in cards {
            let base = format!("/sys/class/drm/{c}/device");
            let driver = std::fs::read_link(format!("{base}/driver")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
            let vid = read(format!("{base}/vendor"));
            r.gpus.push(Gpu { virtual_kind: virtual_kind(&driver, &vid), vendor: vendor_name(&vid).into(), driver, card: c });
        }
    }
    if let Ok(rd) = std::fs::read_dir("/dev/dri") {
        r.render_nodes = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("renderD")).collect();
        r.render_nodes.sort();
    }
    r
}

impl Report {
    pub fn has_display(&self) -> bool {
        self.wayland || self.x11
    }
    /// EGL/GL client library present (necessary for any OpenGL, hardware or software).
    pub fn has_gl_loader(&self) -> bool {
        self.egl.is_some() || self.gl.is_some()
    }
    /// A Mesa software rasterizer is installed (llvmpipe needs LLVM; softpipe does not).
    pub fn software_renderers(&self) -> Vec<&'static str> {
        let mut v = vec![];
        let sw = self.dri_drivers.iter().any(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            n.starts_with("libgallium-") || p.is_dir() && (p.join("swrast_dri.so").exists() || p.join("kms_swrast_dri.so").exists() || p.join("libgallium_dri.so").exists())
        });
        if sw {
            if self.llvm || self.dri_drivers.iter().any(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("libgallium-")).unwrap_or(false)) {
                v.push("llvmpipe");
            }
            v.push("softpipe");
        }
        v
    }
    pub fn only_virtual_or_no_gpu(&self) -> bool {
        self.gpus.is_empty() || self.gpus.iter().all(|g| g.virtual_kind.is_some())
    }

    /// Ordered initialization stages for `mode`.
    pub fn plan(&self, mode: Mode) -> Vec<Stage> {
        match mode {
            Mode::Headless => vec![],
            Mode::Hardware => vec![Stage::Hardware],
            Mode::Software => {
                let mut v = vec![Stage::Software];
                if self.wayland && self.x11 {
                    v.push(Stage::SoftwareX11);
                }
                v
            }
            Mode::Auto => {
                // With no usable GPU (none, or only virtual/firmware framebuffers) go straight to software:
                // it avoids a failing or very slow hardware attempt and a restart.
                let mut v = if self.only_virtual_or_no_gpu() && !self.software_renderers().is_empty() { vec![Stage::Software] } else { vec![Stage::Hardware, Stage::Software] };
                if self.wayland && self.x11 {
                    v.push(Stage::SoftwareX11);
                }
                v
            }
        }
    }

    /// Environment variables that select `stage` (applied to the re-executed process).
    pub fn stage_env(&self, stage: Stage) -> Vec<(&'static str, Option<String>)> {
        match stage {
            Stage::Hardware => vec![],
            Stage::Software | Stage::SoftwareX11 => {
                let mut e: Vec<(&'static str, Option<String>)> = vec![
                    ("LIBGL_ALWAYS_SOFTWARE", Some("1".into())),
                    ("GALLIUM_DRIVER", Some(if self.software_renderers().contains(&"llvmpipe") { "llvmpipe" } else { "softpipe" }.into())),
                    ("__GLX_VENDOR_LIBRARY_NAME", Some("mesa".into())),
                    ("__EGL_VENDOR_LIBRARY_FILENAMES", None),
                ];
                if stage == Stage::SoftwareX11 {
                    e.push(("WAYLAND_DISPLAY", None));
                    e.push(("WINIT_UNIX_BACKEND", Some("x11".into())));
                }
                e
            }
        }
    }

    pub fn install_hint(&self) -> String {
        let (cmd, pkgs) = match self.distro {
            Distro::Debian => ("sudo apt install", "libegl1 libgl1 libgl1-mesa-dri libglx-mesa0 libegl-mesa0"),
            Distro::Fedora => ("sudo dnf install", "mesa-libEGL mesa-libGL mesa-dri-drivers"),
            Distro::Arch => ("sudo pacman -S", "mesa libglvnd"),
            Distro::Alpine => ("sudo apk add", "mesa-egl mesa-gl mesa-dri-gallium"),
            Distro::Suse => ("sudo zypper install", "Mesa-libEGL1 Mesa-libGL1 Mesa-dri"),
            Distro::Unknown => return "Install your distribution's Mesa packages (EGL, GL and the DRI/gallium drivers).".into(),
        };
        format!("{cmd} {pkgs}")
    }

    /// Multi-line diagnostics for the terminal / CLI.
    pub fn describe(&self) -> Vec<String> {
        let mut o = vec![];
        let yn = |b: bool| if b { "yes" } else { "no" };
        o.push(format!(
            "Session: {} (WAYLAND_DISPLAY set: {}, DISPLAY set: {})",
            if self.session_type.is_empty() { "unknown" } else { &self.session_type },
            yn(self.wayland),
            yn(self.x11)
        ));
        match &self.hypervisor {
            Some(h) => o.push(format!("Virtual machine: {h}")),
            None => o.push("Virtual machine: not detected".into()),
        }
        if self.gpus.is_empty() {
            o.push("GPU: none found in /sys/class/drm (no kernel graphics driver bound)".into());
        }
        for g in &self.gpus {
            o.push(format!(
                "GPU {}: {} driver '{}'{}",
                g.card,
                g.vendor,
                if g.driver.is_empty() { "?" } else { &g.driver },
                g.virtual_kind.map(|k| format!(" — {k}, no or limited 3D acceleration")).unwrap_or_default()
            ));
        }
        o.push(format!("DRM render nodes: {}", if self.render_nodes.is_empty() { "none (/dev/dri/renderD*)".into() } else { self.render_nodes.join(", ") }));
        o.push(format!("libEGL: {}", self.egl.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "NOT FOUND".into())));
        o.push(format!("libGL/libGLX: {}", self.gl.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into())));
        let sw = self.software_renderers();
        o.push(format!("Mesa software renderers: {}", if sw.is_empty() { "NOT FOUND".to_string() } else { sw.join(", ") }));
        o.push(format!("Wayland client libraries: {}   X11 libraries: {}   libxkbcommon: {}", yn(self.wayland_libs), yn(self.x11_libs), yn(self.xkb)));
        o
    }

    /// Problems that make a window impossible, each with a fix.
    pub fn problems(&self) -> Vec<String> {
        let mut p = vec![];
        if !self.has_display() {
            p.push("No display server: neither WAYLAND_DISPLAY nor DISPLAY is set. Start the app from a desktop session, use `ssh -X`/a remote-desktop session, or use headless mode (`SystemMonitor --cli`).".into());
        }
        if self.wayland && !self.wayland_libs && !(self.x11 && self.x11_libs) {
            p.push("Wayland session but libwayland-client/libwayland-egl are not installed.".into());
        }
        if self.x11 && !self.x11_libs && !(self.wayland && self.wayland_libs) {
            p.push("X11 session but libX11/libxcb are not installed.".into());
        }
        if self.x11 && self.x11_libs && !self.xkb_x11 && !(self.wayland && self.wayland_libs) {
            p.push("X11 session but libxkbcommon-x11 is not installed (Alpine: libxkbcommon-x11; Debian/Ubuntu: libxkbcommon-x11-0; Fedora: libxkbcommon-x11).".into());
        }
        if !self.xkb {
            p.push("libxkbcommon is not installed (needed for keyboard input).".into());
        }
        if !self.has_gl_loader() {
            p.push(format!("No OpenGL/EGL loader (libEGL/libGL) found. Install Mesa: {}", self.install_hint()));
        } else if self.only_virtual_or_no_gpu() && self.software_renderers().is_empty() {
            p.push(format!("No usable GPU and no Mesa software renderer (llvmpipe/softpipe) found. Install the Mesa drivers: {}", self.install_hint()));
        }
        p
    }
}

#[derive(Clone, Default, Debug)]
pub struct RuntimeInfo {
    pub stage: String,
    pub mode: String,
    pub renderer: String,
}

fn info_cell() -> &'static std::sync::Mutex<RuntimeInfo> {
    static I: std::sync::OnceLock<std::sync::Mutex<RuntimeInfo>> = std::sync::OnceLock::new();
    I.get_or_init(Default::default)
}

pub fn set_stage(stage: &str, mode: &str) {
    let mut i = info_cell().lock().unwrap_or_else(|e| e.into_inner());
    i.stage = stage.into();
    i.mode = mode.into();
}

pub fn set_renderer(r: &str) {
    info_cell().lock().unwrap_or_else(|e| e.into_inner()).renderer = r.into();
}

pub fn runtime_info() -> RuntimeInfo {
    info_cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn is_software_renderer(name: &str) -> bool {
    let n = name.to_lowercase();
    ["llvmpipe", "softpipe", "swrast", "software rasterizer"].iter().any(|k| n.contains(k))
}

/// Resolve the requested mode: command line > `SYSMON_GFX` > `settings.json` > auto.
pub fn requested_mode(args: &[String], from_settings: &str) -> Result<Mode, String> {
    let from_arg = args.iter().position(|a| a == "--gfx").map(|i| args.get(i + 1).cloned().unwrap_or_default()).or_else(|| args.iter().find_map(|a| a.strip_prefix("--gfx=").map(str::to_string)));
    for (src, v) in [("--gfx", from_arg), ("SYSMON_GFX", std::env::var("SYSMON_GFX").ok().filter(|s| !s.is_empty())), ("settings", Some(from_settings.to_string()))] {
        if let Some(v) = v {
            return Mode::parse(&v).ok_or_else(|| format!("unknown graphics mode '{v}' from {src} (use auto, hardware, software or headless)"));
        }
    }
    Ok(Mode::Auto)
}

/// Stage index of this process in the re-exec chain (0 for the first launch).
pub fn current_stage_index() -> usize {
    std::env::var("SYSMON_GFX_STAGE").ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep() -> Report {
        Report { wayland: true, x11: true, egl: Some("/usr/lib64/libEGL.so.1".into()), xkb: true, wayland_libs: true, x11_libs: true, ..Default::default() }
    }

    #[test]
    fn mode_parse() {
        assert_eq!(Mode::parse("LLVMpipe"), Some(Mode::Software));
        assert_eq!(Mode::parse(""), Some(Mode::Auto));
        assert_eq!(Mode::parse("bogus"), None);
        assert_eq!(requested_mode(&["x".into(), "--gfx".into(), "headless".into()], "auto").unwrap(), Mode::Headless);
        assert_eq!(requested_mode(&["--gfx=software".into()], "hardware").unwrap(), Mode::Software);
        assert!(requested_mode(&["--gfx=nope".into()], "auto").is_err());
    }

    #[test]
    fn plan_prefers_software_without_gpu() {
        let mut r = rep();
        r.dri_drivers = vec!["/usr/lib64/libgallium-25.so".into()];
        assert_eq!(r.plan(Mode::Auto), vec![Stage::Software, Stage::SoftwareX11]);
        r.gpus = vec![Gpu { card: "card0".into(), driver: "i915".into(), vendor: "Intel".into(), virtual_kind: None }];
        assert_eq!(r.plan(Mode::Auto), vec![Stage::Hardware, Stage::Software, Stage::SoftwareX11]);
        assert_eq!(r.plan(Mode::Hardware), vec![Stage::Hardware]);
        assert!(r.plan(Mode::Headless).is_empty());
        r.wayland = false;
        assert_eq!(r.plan(Mode::Auto), vec![Stage::Hardware, Stage::Software]);
    }

    #[test]
    fn problems_explain_missing_mesa() {
        let mut r = rep();
        r.egl = None;
        r.distro = Distro::Debian;
        let p = r.problems().join("\n");
        assert!(p.contains("apt install") && p.contains("libegl1"), "{p}");
        let mut r = rep();
        r.distro = Distro::Fedora;
        assert!(r.problems().join("\n").contains("mesa-dri-drivers"));
        let mut r = rep();
        r.wayland = false;
        r.x11 = false;
        assert!(r.problems().join("\n").contains("No display server"));
        assert!(rep().software_renderers().is_empty());
    }

    #[test]
    fn software_stage_env() {
        let mut r = rep();
        r.dri_drivers = vec!["/usr/lib64/libgallium-25.so".into()];
        let e = r.stage_env(Stage::SoftwareX11);
        assert!(e.contains(&("GALLIUM_DRIVER", Some("llvmpipe".into()))));
        assert!(e.contains(&("WAYLAND_DISPLAY", None)));
        assert!(r.stage_env(Stage::Hardware).is_empty());
    }
}
