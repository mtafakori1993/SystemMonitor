//! Read-only SMART health through `smartctl --json`.
//!
//! Safety: only the argument vectors built by `build_args` are ever run; they contain read-only
//! options (`-i -H -A -l error -j`, `-d <type>`), never `-s`, `-t`, `-X`, `-C`, `--set` or similar,
//! and a unit test checks that. "No SMART data" is a distinct state from "disk failing": the disk is
//! reported as failing only when the drive itself says so (`smart_status.passed == false`, or the
//! exit-status bit 3 "DISK FAILING").
use crate::linux::Kind;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Sata,
    Nvme,
    Usb,
    Mmc,
    Other,
}

impl Transport {
    pub fn of(dev: &str, sysfs_link: &str) -> Transport {
        if dev.starts_with("nvme") {
            Transport::Nvme
        } else if dev.starts_with("mmcblk") {
            Transport::Mmc
        } else if sysfs_link.contains("/usb") {
            Transport::Usb
        } else if dev.starts_with("sd") || dev.starts_with("hd") || dev.starts_with("vd") {
            Transport::Sata
        } else {
            Transport::Other
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum State {
    /// Drive answered and reports it is healthy.
    Healthy,
    /// Drive answered, healthy, but attributes need attention (see `warnings`).
    Warning,
    /// The drive itself reports failure (or failure is imminent).
    Failing,
    /// SMART data could not be read: not a statement about the disk's health.
    Unavailable(Unavailable),
}

#[derive(Clone, PartialEq, Debug)]
pub enum Unavailable {
    NotInstalled,
    PermissionDenied,
    Unsupported(String),
    VirtualOrNoDevice,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct Report {
    pub device: String,
    pub state: State,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    pub power_cycles: Option<u64>,
    /// Percentage of rated endurance consumed (NVMe `percentage_used`, or 100 - normalized SSD wear attribute).
    pub wear_used_pct: Option<u8>,
    pub spare_pct: Option<u8>,
    pub reallocated: Option<u64>,
    pub pending: Option<u64>,
    pub uncorrectable: Option<u64>,
    pub crc_errors: Option<u64>,
    pub media_errors: Option<u64>,
    pub unsafe_shutdowns: Option<u64>,
    pub warnings: Vec<String>,
    pub capabilities: Vec<String>,
    /// `-d` type that finally worked ("auto" if none was needed).
    pub device_type: String,
    pub elevated: bool,
}

impl Report {
    fn empty(dev: &str) -> Report {
        Report {
            device: dev.into(),
            state: State::Unavailable(Unavailable::Error("not read".into())),
            model: String::new(),
            serial: String::new(),
            firmware: String::new(),
            temperature_c: None,
            power_on_hours: None,
            power_cycles: None,
            wear_used_pct: None,
            spare_pct: None,
            reallocated: None,
            pending: None,
            uncorrectable: None,
            crc_errors: None,
            media_errors: None,
            unsafe_shutdowns: None,
            warnings: vec![],
            capabilities: vec![],
            device_type: "auto".into(),
            elevated: false,
        }
    }
    pub fn kind(&self) -> Kind {
        match &self.state {
            State::Healthy => Kind::Success,
            State::Warning => Kind::Warning,
            State::Failing => Kind::Error,
            State::Unavailable(_) => Kind::Inactive,
        }
    }
    /// One-line summary; unavailable data never says "failing".
    pub fn headline(&self) -> String {
        match &self.state {
            State::Healthy => "PASSED".into(),
            State::Warning => "PASSED (warnings)".into(),
            State::Failing => "FAILING — back up now".into(),
            State::Unavailable(Unavailable::NotInstalled) => "smartmontools is not installed".into(),
            State::Unavailable(Unavailable::PermissionDenied) => "needs permission (use Check (admin))".into(),
            State::Unavailable(Unavailable::Unsupported(w)) => format!("no SMART data: {w}"),
            State::Unavailable(Unavailable::VirtualOrNoDevice) => "virtual or missing device: no SMART".into(),
            State::Unavailable(Unavailable::Error(e)) => format!("could not read: {e}"),
        }
    }
}

/// Read-only smartctl arguments. `dtype` is smartctl's `-d` value ("auto" omits the option).
pub fn build_args(dev_path: &str, dtype: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["-j".into(), "-i".into(), "-H".into(), "-A".into(), "-c".into(), "-l".into(), "error".into()];
    if dtype != "auto" {
        a.push("-d".into());
        a.push(dtype.into());
    }
    a.push(dev_path.into());
    a
}

pub fn smartctl_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SYSMON_SMARTCTL") {
        // test hook: point at a fake executable that prints canned output
        return Some(PathBuf::from(p));
    }
    crate::linux::which("smartctl").or_else(|| ["/usr/sbin/smartctl", "/sbin/smartctl", "/usr/local/sbin/smartctl"].iter().map(PathBuf::from).find(|p| p.exists()))
}

fn valid_dev(dev: &str) -> bool {
    !dev.is_empty() && dev.chars().all(|c| c.is_ascii_alphanumeric()) && std::path::Path::new(&format!("/sys/block/{dev}")).exists() || std::env::var_os("SYSMON_SMARTCTL").is_some() && dev.chars().all(|c| c.is_ascii_alphanumeric()) && !dev.is_empty()
}

/// Check one block device. `elevated` runs smartctl through `pkexec` (only when the user asks for it).
pub fn check(dev: &str, elevated: bool) -> Report {
    let mut r = Report::empty(dev);
    if !valid_dev(dev) {
        r.state = State::Unavailable(Unavailable::VirtualOrNoDevice);
        return r;
    }
    let Some(sc) = smartctl_path() else {
        r.state = State::Unavailable(Unavailable::NotInstalled);
        return r;
    };
    let link = std::fs::read_link(format!("/sys/block/{dev}")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let transport = Transport::of(dev, &link);
    // USB bridges often need an explicit SAT pass-through; try it only after the plain attempt says "unknown".
    let mut types = vec!["auto"];
    if transport == Transport::Usb {
        types.extend(["sat", "sat,12", "scsi"]);
    }
    let path = format!("/dev/{dev}");
    let mut last = r.clone();
    for t in types {
        let args = build_args(&path, t);
        debug_assert!(is_read_only(&args));
        let out = if elevated {
            Command::new("pkexec").arg(&sc).args(&args).env("LC_ALL", "C").stdin(std::process::Stdio::null()).output()
        } else {
            Command::new(&sc).args(&args).env("LC_ALL", "C").stdin(std::process::Stdio::null()).output()
        };
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                last.state = State::Unavailable(Unavailable::Error(e.to_string()));
                return last;
            }
        };
        if elevated && out.status.code() == Some(126) || out.status.code() == Some(127) && elevated {
            last.state = State::Unavailable(Unavailable::Error("authentication was cancelled or refused".into()));
            return last;
        }
        let mut rep = parse(dev, &String::from_utf8_lossy(&out.stdout), out.status.code().unwrap_or(-1));
        rep.device_type = t.into();
        rep.elevated = elevated;
        let retry = matches!(&rep.state, State::Unavailable(Unavailable::Unsupported(_)));
        if !retry {
            return rep;
        }
        last = rep;
    }
    last
}

/// True when none of the arguments can change drive state.
pub fn is_read_only(args: &[String]) -> bool {
    const FORBIDDEN: [&str; 14] = ["-s", "--smart", "-t", "--test", "-X", "--abort", "-C", "--captive", "--set", "-o", "--offlineauto", "-S", "--saveauto", "--security"];
    !args.iter().any(|a| FORBIDDEN.contains(&a.as_str()) || a.starts_with("--set=") || a.starts_with("--security="))
}

fn u(v: &Value) -> Option<u64> {
    v.as_u64()
}

/// Parse `smartctl -j` output. `exit` is smartctl's exit status bit mask.
pub fn parse(dev: &str, json: &str, exit: i32) -> Report {
    let mut r = Report::empty(dev);
    let Ok(j) = serde_json::from_str::<Value>(json) else {
        r.state = State::Unavailable(Unavailable::Error(if json.trim().is_empty() { "smartctl produced no output".into() } else { "unparsable smartctl output".into() }));
        return r;
    };
    let messages: Vec<String> = j["smartctl"]["messages"].as_array().map(|a| a.iter().filter_map(|m| m["string"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    let msg_text = messages.join(" | ");
    let lower = msg_text.to_lowercase();
    let exit = if exit < 0 { j["smartctl"]["exit_status"].as_i64().unwrap_or(0) as i32 } else { exit };

    r.model = j["model_name"].as_str().or_else(|| j["scsi_model_name"].as_str()).unwrap_or("").to_string();
    r.serial = j["serial_number"].as_str().unwrap_or("").to_string();
    r.firmware = j["firmware_version"].as_str().unwrap_or("").to_string();
    r.temperature_c = j["temperature"]["current"].as_i64();
    r.power_on_hours = u(&j["power_on_time"]["hours"]);
    r.power_cycles = u(&j["power_cycle_count"]);
    let smart_supported = j["smart_support"]["available"].as_bool();
    let smart_enabled = j["smart_support"]["enabled"].as_bool();
    if smart_supported == Some(true) {
        r.capabilities.push(if smart_enabled == Some(false) { "SMART supported (disabled)".into() } else { "SMART".into() });
    }
    if j["ata_smart_data"]["capabilities"]["self_tests_supported"].as_bool() == Some(true) {
        r.capabilities.push("self-test".into());
    }
    if j["nvme_smart_health_information_log"].is_object() {
        r.capabilities.push("NVMe health log".into());
    }
    if j["ata_smart_attributes"]["table"].is_array() {
        r.capabilities.push("attributes".into());
    }

    // ---- ATA attribute table
    if let Some(t) = j["ata_smart_attributes"]["table"].as_array() {
        for a in t {
            let id = a["id"].as_u64().unwrap_or(0);
            let raw = a["raw"]["value"].as_u64();
            let norm = a["value"].as_u64();
            match id {
                5 => r.reallocated = raw,
                9 if r.power_on_hours.is_none() => r.power_on_hours = raw,
                12 if r.power_cycles.is_none() => r.power_cycles = raw,
                174 | 192 if id == 174 => r.unsafe_shutdowns = raw,
                197 => r.pending = raw,
                198 => r.uncorrectable = raw,
                199 => r.crc_errors = raw,
                // normalized "life remaining" attributes: 177 wear leveling (Samsung), 231 SSD life left, 233 media wearout (Intel)
                177 | 231 | 233 if r.wear_used_pct.is_none() => r.wear_used_pct = norm.filter(|n| *n <= 100).map(|n| 100 - n as u8),
                190 | 194 if r.temperature_c.is_none() => r.temperature_c = a["raw"]["value"].as_i64().map(|v| v & 0xff),
                _ => {}
            }
        }
    }
    // ---- NVMe log
    let nv = &j["nvme_smart_health_information_log"];
    if nv.is_object() {
        r.wear_used_pct = u(&nv["percentage_used"]).map(|p| p.min(255) as u8);
        r.spare_pct = u(&nv["available_spare"]).map(|p| p.min(100) as u8);
        r.media_errors = u(&nv["media_errors"]);
        r.unsafe_shutdowns = u(&nv["unsafe_shutdowns"]);
        r.power_cycles = u(&nv["power_cycles"]).or(r.power_cycles);
        r.power_on_hours = u(&nv["power_on_hours"]).or(r.power_on_hours);
        r.temperature_c = nv["temperature"].as_i64().or(r.temperature_c);
        let cw = u(&nv["critical_warning"]).unwrap_or(0);
        if cw & 1 != 0 {
            r.warnings.push("available spare below threshold".into());
        }
        if cw & 2 != 0 {
            r.warnings.push("temperature over/under threshold".into());
        }
        if cw & 4 != 0 {
            r.warnings.push("NVM subsystem reliability degraded".into());
        }
        if cw & 8 != 0 {
            r.warnings.push("media is in read-only mode".into());
        }
        if cw & 16 != 0 {
            r.warnings.push("volatile memory backup failed".into());
        }
    }

    // ---- could we read anything?
    let passed = j["smart_status"]["passed"].as_bool();
    let got_data = passed.is_some() || r.temperature_c.is_some() || j["ata_smart_attributes"].is_object() || nv.is_object();
    if !got_data {
        let why = if exit & 2 != 0 && (lower.contains("permission denied") || lower.contains("operation not permitted") || lower.contains("open device")) {
            Unavailable::PermissionDenied
        } else if lower.contains("unknown usb bridge") || lower.contains("unsupported") || lower.contains("not supported") || lower.contains("smart support is: unavailable") || lower.contains("unable to detect device type") || smart_supported == Some(false) {
            Unavailable::Unsupported(if msg_text.is_empty() { "device does not support SMART".into() } else { msg_text.clone() })
        } else if lower.contains("no such device") || lower.contains("no such file") {
            Unavailable::VirtualOrNoDevice
        } else {
            Unavailable::Error(if msg_text.is_empty() { format!("smartctl exit status {exit}") } else { msg_text.clone() })
        };
        r.state = State::Unavailable(why);
        return r;
    }

    // ---- warnings from counters (these are advisories, not failure)
    if r.reallocated.unwrap_or(0) > 0 {
        r.warnings.push(format!("{} reallocated sectors", r.reallocated.unwrap_or(0)));
    }
    if r.pending.unwrap_or(0) > 0 {
        r.warnings.push(format!("{} sectors pending reallocation", r.pending.unwrap_or(0)));
    }
    if r.uncorrectable.unwrap_or(0) > 0 {
        r.warnings.push(format!("{} offline-uncorrectable sectors", r.uncorrectable.unwrap_or(0)));
    }
    if r.crc_errors.unwrap_or(0) > 0 {
        r.warnings.push(format!("{} interface CRC errors (check the cable)", r.crc_errors.unwrap_or(0)));
    }
    if r.media_errors.unwrap_or(0) > 0 {
        r.warnings.push(format!("{} media/data-integrity errors", r.media_errors.unwrap_or(0)));
    }
    if let Some(w) = r.wear_used_pct {
        if w >= 90 {
            r.warnings.push(format!("{w}% of rated endurance used"));
        }
    }
    if let Some(s) = r.spare_pct {
        if s < 20 {
            r.warnings.push(format!("available spare is down to {s}%"));
        }
    }
    if let Some(t) = r.temperature_c {
        if t >= 70 {
            r.warnings.push(format!("temperature {t} °C is high"));
        }
    }
    if exit & 0x40 != 0 || j["ata_smart_error_log"]["summary"]["count"].as_u64().unwrap_or(0) > 0 {
        r.warnings.push("the drive's error log contains entries".into());
    }
    if exit & 0x20 != 0 {
        r.warnings.push("an attribute was at or below its threshold in the past".into());
    }
    if exit & 0x80 != 0 {
        r.warnings.push("the self-test log contains errors".into());
    }
    if smart_enabled == Some(false) {
        r.warnings.push("SMART is supported but disabled in the drive firmware".into());
    }

    let failing = passed == Some(false) || exit & 0x08 != 0 || exit & 0x10 != 0 && passed != Some(true);
    r.state = if failing {
        State::Failing
    } else if passed.is_none() && r.warnings.is_empty() && !nv.is_object() {
        // attributes but no overall verdict
        State::Unavailable(Unavailable::Unsupported("the drive gives no overall health verdict".into()))
    } else if !r.warnings.is_empty() {
        State::Warning
    } else {
        State::Healthy
    };
    if failing && passed == Some(false) {
        r.warnings.insert(0, "the drive's own SMART self-assessment is FAILED".into());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed but structurally faithful `smartctl -j` outputs (smartmontools 7.x).
    const ATA_OK: &str = r#"{"smartctl":{"exit_status":0},"model_name":"Samsung SSD 860 EVO 500GB","serial_number":"S3Z","firmware_version":"RVT04B6Q",
      "smart_support":{"available":true,"enabled":true},"smart_status":{"passed":true},"temperature":{"current":34},"power_on_time":{"hours":12345},"power_cycle_count":321,
      "ata_smart_data":{"capabilities":{"self_tests_supported":true}},
      "ata_smart_attributes":{"table":[
        {"id":5,"name":"Reallocated_Sector_Ct","value":100,"raw":{"value":0}},
        {"id":9,"name":"Power_On_Hours","value":95,"raw":{"value":12345}},
        {"id":177,"name":"Wear_Leveling_Count","value":97,"raw":{"value":42}},
        {"id":197,"name":"Current_Pending_Sector","value":100,"raw":{"value":0}},
        {"id":199,"name":"UDMA_CRC_Error_Count","value":100,"raw":{"value":0}}]}}"#;
    const ATA_FAIL: &str = r#"{"smartctl":{"exit_status":8},"model_name":"WDC WD10EZEX","smart_support":{"available":true,"enabled":true},"smart_status":{"passed":false},"temperature":{"current":41},
      "ata_smart_attributes":{"table":[{"id":5,"value":1,"raw":{"value":2048}},{"id":197,"value":90,"raw":{"value":8}}]}}"#;
    const ATA_WARN: &str = r#"{"smartctl":{"exit_status":64},"model_name":"ST2000DM","smart_support":{"available":true,"enabled":true},"smart_status":{"passed":true},"temperature":{"current":38},
      "ata_smart_attributes":{"table":[{"id":5,"value":98,"raw":{"value":16}},{"id":199,"value":100,"raw":{"value":3}}]}}"#;
    const NVME_OK: &str = r#"{"smartctl":{"exit_status":0},"model_name":"Lexar 256GB SSD","firmware_version":"V1","smart_support":{"available":true,"enabled":true},"smart_status":{"passed":true,"nvme":{"value":0}},
      "temperature":{"current":41},"power_cycle_count":900,"power_on_time":{"hours":4000},
      "nvme_smart_health_information_log":{"critical_warning":0,"temperature":41,"available_spare":100,"percentage_used":3,"power_cycles":900,"power_on_hours":4000,"unsafe_shutdowns":12,"media_errors":0}}"#;
    const NVME_SPARE: &str = r#"{"smartctl":{"exit_status":0},"smart_status":{"passed":true},"nvme_smart_health_information_log":{"critical_warning":1,"temperature":50,"available_spare":5,"percentage_used":95,"media_errors":3}}"#;
    const PERM: &str = r#"{"smartctl":{"exit_status":2,"messages":[{"string":"Smartctl open device: /dev/sda failed: Permission denied","severity":"error"}]}}"#;
    const USB_UNKNOWN: &str = r#"{"smartctl":{"exit_status":1,"messages":[{"string":"/dev/sdb: Unknown USB bridge [0x0bc2:0x2312 (0x100)]","severity":"error"}]}}"#;
    const NO_SMART: &str = r#"{"smartctl":{"exit_status":4,"messages":[{"string":"SMART support is: Unavailable - device lacks SMART capability.","severity":"warning"}]},"smart_support":{"available":false,"enabled":false}}"#;

    #[test]
    fn healthy_sata_ssd() {
        let r = parse("sda", ATA_OK, 0);
        assert_eq!(r.state, State::Healthy);
        assert_eq!((r.temperature_c, r.power_on_hours, r.power_cycles), (Some(34), Some(12345), Some(321)));
        assert_eq!(r.wear_used_pct, Some(3), "100 - normalized 97");
        assert_eq!(r.reallocated, Some(0));
        assert!(r.warnings.is_empty());
        assert!(r.capabilities.iter().any(|c| c == "self-test"));
        assert_eq!(r.kind(), Kind::Success);
    }

    #[test]
    fn failing_only_when_drive_says_so() {
        let r = parse("sda", ATA_FAIL, 8);
        assert_eq!(r.state, State::Failing);
        assert!(r.warnings[0].contains("FAILED"));
        assert!(r.warnings.iter().any(|w| w.contains("2048 reallocated")));
        assert_eq!(r.kind(), Kind::Error);
        // the same exit bit with a passing verdict from the drive is still failing (bit 3 = DISK FAILING)
        assert_eq!(parse("sda", ATA_OK, 8).state, State::Failing);
    }

    #[test]
    fn counters_warn_without_failing() {
        let r = parse("sda", ATA_WARN, 64);
        assert_eq!(r.state, State::Warning);
        assert!(r.warnings.iter().any(|w| w.contains("16 reallocated")));
        assert!(r.warnings.iter().any(|w| w.contains("CRC")));
        assert!(r.warnings.iter().any(|w| w.contains("error log")));
        assert_eq!(r.kind(), Kind::Warning);
    }

    #[test]
    fn nvme_wear_and_spare() {
        let r = parse("nvme0n1", NVME_OK, 0);
        assert_eq!(r.state, State::Healthy);
        assert_eq!((r.wear_used_pct, r.spare_pct, r.unsafe_shutdowns, r.temperature_c), (Some(3), Some(100), Some(12), Some(41)));
        let w = parse("nvme0n1", NVME_SPARE, 0);
        assert_eq!(w.state, State::Warning);
        assert!(w.warnings.iter().any(|x| x.contains("spare below threshold")) && w.warnings.iter().any(|x| x.contains("95%")) && w.warnings.iter().any(|x| x.contains("media")));
    }

    #[test]
    fn unavailable_is_not_failure() {
        let p = parse("sda", PERM, 2);
        assert_eq!(p.state, State::Unavailable(Unavailable::PermissionDenied));
        assert_eq!(p.kind(), Kind::Inactive);
        assert!(!p.headline().to_lowercase().contains("fail"));
        assert!(matches!(parse("sdb", USB_UNKNOWN, 1).state, State::Unavailable(Unavailable::Unsupported(_))));
        assert!(matches!(parse("mmcblk0", NO_SMART, 4).state, State::Unavailable(Unavailable::Unsupported(_))));
        assert!(matches!(parse("sda", "", 1).state, State::Unavailable(Unavailable::Error(_))));
        assert!(matches!(parse("sda", "not json", 1).state, State::Unavailable(Unavailable::Error(_))));
    }

    #[test]
    fn arguments_are_read_only() {
        for t in ["auto", "sat", "sat,12", "scsi", "nvme"] {
            let a = build_args("/dev/sda", t);
            assert!(is_read_only(&a), "{a:?}");
            assert_eq!(a.last().unwrap(), "/dev/sda");
        }
        assert!(!is_read_only(&["-s".into(), "on".into()]));
        assert!(!is_read_only(&["-t".into(), "long".into()]));
        assert!(!is_read_only(&["--set=standby,now".into()]));
        assert!(!is_read_only(&["-X".into()]));
    }

    #[test]
    fn transport_detection() {
        assert_eq!(Transport::of("nvme0n1", ""), Transport::Nvme);
        assert_eq!(Transport::of("sdb", "../devices/pci0000:00/usb2/2-1/host6/target6:0:0/6:0:0:0/block/sdb"), Transport::Usb);
        assert_eq!(Transport::of("sda", "../devices/pci0000:00/0000:00:17.0/ata1/host0/block/sda"), Transport::Sata);
        assert_eq!(Transport::of("mmcblk0", ""), Transport::Mmc);
    }

    /// Drive `check` end to end against a fake smartctl executable (no hardware needed).
    #[test]
    fn check_runs_the_configured_smartctl() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("sysmon-smart-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let fake = d.join("smartctl");
        std::fs::write(&fake, format!("#!/bin/sh\ncat <<'EOF'\n{NVME_OK}\nEOF\nexit 0\n")).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("SYSMON_SMARTCTL", &fake) };
        let r = check("nvme0n1", false);
        unsafe { std::env::remove_var("SYSMON_SMARTCTL") };
        assert_eq!(r.state, State::Healthy, "{:?}", r.state);
        assert_eq!(r.model, "Lexar 256GB SSD");
        let _ = std::fs::remove_dir_all(d);
    }
}
