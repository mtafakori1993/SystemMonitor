//! Shell integration for the proxy environment (Bash, Zsh, Fish).
//!
//! Why this exists: environment variables are copied into a process when it starts. A shell that is
//! already running keeps its own copy, and no program can change another process's environment, so
//! changing the proxy never affects open terminals by itself. New processes inherit from their
//! parent: the login session (systemd `environment.d`, `/etc/environment`) for graphical launches,
//! and the shell start-up files for terminals. This module installs one small, clearly delimited,
//! removable block that makes *new* shells read `~/.config/SystemMonitor/proxy.env`, and renders the
//! script that an *existing* shell can `eval` to catch up.
//!
//! Safety rules: never touch anything outside the managed block, follow symlinked dotfiles instead
//! of replacing them, back up before every edit, verify after writing (and syntax-check when the
//! shell is available), roll back on failure, never add the block twice.
use crate::linux::{backup_dir, now_secs, which, write_atomic};
use std::fs;
use std::path::{Path, PathBuf};

pub const BEGIN: &str = "# >>> SystemMonitor proxy (managed block; remove with: SystemMonitor --cli shell-setup remove) >>>";
pub const END: &str = "# <<< SystemMonitor proxy <<<";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

pub const ALL: [Shell; 3] = [Shell::Bash, Shell::Zsh, Shell::Fish];
pub const PROXY_VARS: [&str; 8] = ["http_proxy", "https_proxy", "all_proxy", "no_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];

impl Shell {
    pub fn parse(s: &str) -> Option<Shell> {
        match s.to_ascii_lowercase().as_str() {
            "bash" | "sh" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }
    pub fn installed(self) -> bool {
        which(self.name()).is_some()
    }
}

/// Where a shell reads its start-up configuration.
pub struct Dirs {
    pub home: PathBuf,
    pub config: PathBuf,
}

impl Dirs {
    pub fn current() -> Dirs {
        Dirs { home: crate::linux::home(), config: crate::linux::config_home() }
    }
    /// File that gets the managed block (bash/zsh) or the whole snippet (fish: its own `conf.d` file, config.fish is never edited).
    pub fn startup_file(&self, s: Shell) -> PathBuf {
        match s {
            Shell::Bash => self.home.join(".bashrc"),
            Shell::Zsh => std::env::var_os("ZDOTDIR").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| self.home.clone()).join(".zshrc"),
            Shell::Fish => self.config.join("fish/conf.d/systemmonitor-proxy.fish"),
        }
    }
    pub fn env_file(&self, s: Shell) -> PathBuf {
        let base = self.config.join("SystemMonitor");
        base.join(if s == Shell::Fish { "proxy.fish" } else { "proxy.env" })
    }
}

fn sh_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

fn fish_quote(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Script that makes a shell's proxy variables match `vars`; with no vars it unsets them all
/// (this is what `eval` in an existing terminal needs after the proxy was disabled).
pub fn export_script(shell: Shell, vars: &[(String, String)]) -> String {
    let vars: Vec<&(String, String)> = vars.iter().filter(|(k, _)| PROXY_VARS.contains(&k.as_str())).collect();
    let mut out = String::new();
    match shell {
        Shell::Bash | Shell::Zsh => {
            for k in PROXY_VARS {
                match vars.iter().find(|(n, _)| n == k) {
                    Some((_, v)) => out += &format!("export {k}={}\n", sh_quote(v)),
                    None => out += &format!("unset {k}\n"),
                }
            }
        }
        Shell::Fish => {
            for k in PROXY_VARS {
                match vars.iter().find(|(n, _)| n == k) {
                    Some((_, v)) => out += &format!("set -gx {k} {}\n", fish_quote(v)),
                    None => out += &format!("set -e {k}\n"),
                }
            }
        }
    }
    out
}

fn block_for(s: Shell, d: &Dirs) -> String {
    let env = d.env_file(s);
    match s {
        Shell::Fish => format!("{BEGIN}\nif test -f {p}\n    source {p}\nend\n{END}\n", p = fish_quote(&env.to_string_lossy())),
        _ => format!("{BEGIN}\n[ -f {p} ] && . {p}\n{END}\n", p = sh_quote(&env.to_string_lossy())),
    }
}

/// Remove every managed block; returns the new text and how many blocks were removed. Everything else is kept byte for byte.
pub fn strip_blocks(text: &str) -> (String, usize) {
    let mut out = String::new();
    let mut n = 0;
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let t = line.trim_end_matches(['\n', '\r']);
        if !inside && t == BEGIN {
            inside = true;
            n += 1;
        } else if inside && t == END {
            inside = false;
        } else if !inside {
            out.push_str(line);
        }
    }
    if inside {
        // unterminated block: refuse to guess, keep the text as it was
        return (text.to_string(), 0);
    }
    (out, n)
}

pub fn with_block(text: &str, block: &str) -> String {
    let (mut base, _) = strip_blocks(text);
    if !base.is_empty() && !base.ends_with('\n') {
        base.push('\n');
    }
    base + block
}

fn syntax_ok(shell: Shell, file: &Path) -> Result<(), String> {
    let (prog, args): (&str, &[&str]) = match shell {
        Shell::Bash => ("bash", &["-n"]),
        Shell::Zsh => ("zsh", &["-n"]),
        Shell::Fish => ("fish", &["--no-execute"]),
    };
    if which(prog).is_none() {
        return Ok(());
    }
    let out = std::process::Command::new(prog).args(args).arg(file).output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

fn backup(file: &Path, shell: Shell) -> Result<Option<PathBuf>, String> {
    let Ok(content) = fs::read(file) else { return Ok(None) };
    let dst = backup_dir("shell").join(format!("{}.{}.{}.bak", shell.name(), now_secs(), file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    fs::write(&dst, content).map_err(|e| format!("backup {}: {e}", dst.display()))?;
    Ok(Some(dst))
}

/// Resolve symlinks so a dotfile manager's link keeps pointing at its repository.
fn real_path(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o644)
}

pub fn is_installed(shell: Shell, d: &Dirs) -> bool {
    fs::read_to_string(d.startup_file(shell)).map(|t| t.lines().any(|l| l == BEGIN)).unwrap_or(false)
}

/// Install (or refresh) the block for one shell. Idempotent.
pub fn install(shell: Shell, d: &Dirs) -> Result<String, String> {
    let file = d.startup_file(shell);
    let target = real_path(&file);
    let old = fs::read_to_string(&target).unwrap_or_default();
    let new = if shell == Shell::Fish { block_for(shell, d) } else { with_block(&old, &block_for(shell, d)) };
    if new == old {
        return Ok(format!("{}: already set up ({})", shell.name(), file.display()));
    }
    let bak = backup(&target, shell)?;
    let mode = if target.exists() { mode_of(&target) } else { 0o644 };
    write_atomic(&target, &new, mode)?;
    let verify = (|| {
        let back = fs::read_to_string(&target).map_err(|e| e.to_string())?;
        if back.lines().filter(|l| *l == BEGIN).count() != 1 {
            return Err("managed block was not written exactly once".to_string());
        }
        // user content outside the block must be unchanged
        if strip_blocks(&back).0.trim_end() != strip_blocks(&old).0.trim_end() {
            return Err("content outside the managed block changed".to_string());
        }
        syntax_ok(shell, &target)
    })();
    if let Err(e) = verify {
        match &bak {
            Some(b) => {
                let _ = fs::read_to_string(b).map(|c| write_atomic(&target, &c, mode));
            }
            None => {
                let _ = fs::remove_file(&target);
            }
        }
        return Err(format!("{}: {e}; {} restored", shell.name(), file.display()));
    }
    Ok(format!("{}: block added to {}{}", shell.name(), file.display(), bak.map(|b| format!(" (backup {})", b.display())).unwrap_or_default()))
}

/// Remove the block for one shell; a fish snippet file we created is deleted, everything else is left alone.
pub fn remove(shell: Shell, d: &Dirs) -> Result<String, String> {
    let file = d.startup_file(shell);
    let target = real_path(&file);
    let Ok(old) = fs::read_to_string(&target) else { return Ok(format!("{}: nothing to remove", shell.name())) };
    let (new, n) = strip_blocks(&old);
    if n == 0 {
        return Ok(format!("{}: no managed block in {}", shell.name(), file.display()));
    }
    backup(&target, shell)?;
    if shell == Shell::Fish && new.trim().is_empty() {
        fs::remove_file(&target).map_err(|e| e.to_string())?;
    } else {
        write_atomic(&target, &new, mode_of(&target))?;
    }
    Ok(format!("{}: removed {n} managed block(s) from {}", shell.name(), file.display()))
}

/// Write the per-shell environment files that the blocks source. Empty `vars` removes them (proxy disabled).
pub fn write_env_files(d: &Dirs, vars: &[(String, String)]) -> Result<(), String> {
    for s in [Shell::Bash, Shell::Fish] {
        let f = d.env_file(s);
        if vars.is_empty() {
            let _ = fs::remove_file(&f);
        } else {
            write_atomic(&f, &export_script(s, vars), 0o600)?;
        }
    }
    Ok(())
}

/// Proxy variables applied to programs this application starts (its own "current environment").
/// Updating it is instant and needs no restart; it cannot reach shells or programs that already run.
fn process_cell() -> &'static std::sync::Mutex<Vec<(String, String)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String)>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

pub fn set_process_proxy(vars: &[(String, String)]) {
    *process_cell().lock().unwrap_or_else(|e| e.into_inner()) = vars.iter().filter(|(k, _)| PROXY_VARS.contains(&k.as_str())).cloned().collect();
}

pub fn apply_to(cmd: &mut std::process::Command) {
    let vars = process_cell().lock().unwrap_or_else(|e| e.into_inner()).clone();
    for k in PROXY_VARS {
        match vars.iter().find(|(n, _)| n == k) {
            Some((_, v)) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
}

pub fn status(d: &Dirs) -> Vec<String> {
    ALL.iter()
        .map(|s| format!("{:<5} {:<9} hook {:<13} {}", s.name(), if s.installed() { "installed" } else { "not found" }, if is_installed(*s, d) { "set up" } else { "not set up" }, d.startup_file(*s).display()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn dirs(tag: &str) -> Dirs {
        let root = std::env::temp_dir().join(format!("sysmon-shell-{tag}-{}-{}", std::process::id(), crate::linux::now_millis()));
        fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", root.join(".config")) };
        Dirs { home: root.clone(), config: root.join(".config") }
    }

    #[test]
    fn export_scripts_quote_and_unset() {
        let v = vec![("http_proxy".to_string(), "http://u:p'w@h:80".to_string())];
        let sh = export_script(Shell::Bash, &v);
        assert!(sh.contains(r#"export http_proxy='http://u:p'\''w@h:80'"#), "{sh}");
        assert!(sh.contains("unset https_proxy"));
        let fish = export_script(Shell::Fish, &v);
        assert!(fish.contains(r"set -gx http_proxy 'http://u:p\'w@h:80'") && fish.contains("set -e NO_PROXY"), "{fish}");
        assert_eq!(export_script(Shell::Zsh, &[]).lines().count(), 8, "all unset when disabled");
        // foreign variables are never exported
        assert!(!export_script(Shell::Bash, &[("LD_PRELOAD".into(), "x".into())]).contains("LD_PRELOAD"));
    }

    #[test]
    fn install_is_idempotent_and_preserves_user_content() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dirs("idem");
        let rc = d.startup_file(Shell::Bash);
        let original = "# my bashrc\nalias ll='ls -l'\nexport PATH=$PATH:~/bin";
        fs::write(&rc, original).unwrap(); // note: no trailing newline
        install(Shell::Bash, &d).unwrap();
        let after = fs::read_to_string(&rc).unwrap();
        assert!(after.starts_with(original) && after.matches(BEGIN).count() == 1, "{after}");
        let again = install(Shell::Bash, &d).unwrap();
        assert!(again.contains("already"), "{again}");
        assert_eq!(fs::read_to_string(&rc).unwrap(), after);
        // a hand-duplicated block collapses to one
        fs::write(&rc, format!("{after}{}", block_for(Shell::Bash, &d))).unwrap();
        install(Shell::Bash, &d).unwrap();
        assert_eq!(fs::read_to_string(&rc).unwrap().matches(BEGIN).count(), 1);
        // remove restores the original exactly (modulo the added newline)
        remove(Shell::Bash, &d).unwrap();
        assert_eq!(fs::read_to_string(&rc).unwrap(), format!("{original}\n"));
        assert!(fs::read_dir(backup_dir("shell")).map(|r| r.count() > 0).unwrap_or(false), "backups were taken");
        let _ = fs::remove_dir_all(&d.home);
    }

    #[test]
    fn fish_uses_its_own_file_and_symlinked_dotfiles_survive() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dirs("fish");
        install(Shell::Fish, &d).unwrap();
        let f = d.startup_file(Shell::Fish);
        assert!(f.ends_with("fish/conf.d/systemmonitor-proxy.fish") && fs::read_to_string(&f).unwrap().contains("source"));
        assert!(!d.config.join("fish/config.fish").exists(), "config.fish is never created or edited");
        remove(Shell::Fish, &d).unwrap();
        assert!(!f.exists());
        // zsh rc is a symlink into a dotfiles repo: the link must stay a link
        let repo = d.home.join("dotfiles");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("zshrc"), "setopt autocd\n").unwrap();
        std::os::unix::fs::symlink(repo.join("zshrc"), d.home.join(".zshrc")).unwrap();
        unsafe { std::env::remove_var("ZDOTDIR") };
        install(Shell::Zsh, &d).unwrap();
        assert!(fs::symlink_metadata(d.home.join(".zshrc")).unwrap().file_type().is_symlink());
        assert!(fs::read_to_string(repo.join("zshrc")).unwrap().contains(BEGIN));
        let _ = fs::remove_dir_all(&d.home);
    }

    #[test]
    fn unterminated_block_is_left_alone() {
        let text = format!("a\n{BEGIN}\nb\n");
        assert_eq!(strip_blocks(&text), (text.clone(), 0));
        let (t, n) = strip_blocks(&format!("a\n{BEGIN}\nx\n{END}\nb\n"));
        assert_eq!((t.as_str(), n), ("a\nb\n", 1));
    }

    #[test]
    fn env_files_written_and_cleaned() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let d = dirs("env");
        let v = vec![("http_proxy".to_string(), "http://h:1".to_string())];
        write_env_files(&d, &v).unwrap();
        assert!(fs::read_to_string(d.env_file(Shell::Bash)).unwrap().contains("export http_proxy='http://h:1'"));
        assert!(fs::read_to_string(d.env_file(Shell::Fish)).unwrap().contains("set -gx http_proxy"));
        write_env_files(&d, &[]).unwrap();
        assert!(!d.env_file(Shell::Bash).exists() && !d.env_file(Shell::Fish).exists());
        let _ = fs::remove_dir_all(&d.home);
    }
}
