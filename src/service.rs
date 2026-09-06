//! Daemon lifecycle on a real machine: start/stop/status, and login autostart
//! via launchd (macOS) or a systemd user unit (Linux).
//!
//! `fore start` works without any service manager: it double-forks the daemon with
//! stdout/stderr → ~/.local/state/fore/daemon.log. The service files just call
//! `fore daemon` in the foreground and let launchd/systemd own the process.

use crate::config;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn is_running() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(config::pid_path()).ok()?.trim().parse().ok()?;
    // Signal 0 = existence check.
    let alive = unsafe { libc_kill(pid as i32, 0) } == 0;
    if alive { Some(pid) } else { None }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

pub fn socket_alive() -> bool {
    std::os::unix::net::UnixStream::connect(config::socket_path()).is_ok()
}

/// Start the daemon detached. Returns once the socket answers (or after 3 s).
/// If a launchd/systemd unit is installed, the manager is asked instead, so the
/// process stays under its supervision.
pub fn start(exe: &PathBuf) -> Result<u32, String> {
    if socket_alive() {
        return Err("already running".into());
    }
    if autostart_installed() {
        let ok = match manager() {
            Manager::Launchd => launchctl(&["kickstart", &format!("gui/{}/dev.fore.daemon", uid())]),
            Manager::Systemd => systemctl(&["start", "fore.service"]),
            Manager::None => false,
        };
        if ok {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(3) {
                if socket_alive() { return Ok(is_running().unwrap_or(0)); }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        // fall through: manager missing/failed → plain detached start
    }
    let log = config::log_path();
    if let Some(p) = log.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
    rotate_log(&log);
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log).map_err(|e| format!("open log {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;

    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(out).stderr(err);
    // New session so a closing terminal's SIGHUP doesn't take the daemon with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
    let pid = child.id();
    // Don't wait() — we want it orphaned/reparented, not zombified when we exit.
    std::mem::forget(child);

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if socket_alive() { return Ok(pid); }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("daemon did not answer within 3s — see {}", log.display()))
}

pub fn stop() -> Result<(), String> {
    if autostart_installed() && is_running().is_some() {
        let ok = match manager() {
            Manager::Launchd => launchctl(&["kill", "SIGTERM", &format!("gui/{}/dev.fore.daemon", uid())]),
            Manager::Systemd => systemctl(&["stop", "fore.service"]),
            Manager::None => false,
        };
        if ok {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(3) && is_running().is_some() { std::thread::sleep(Duration::from_millis(50)); }
        }
    }
    if let Some(pid) = is_running() {
        unsafe { libc_kill(pid as i32, 15) }; // SIGTERM
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            if is_running().is_none() { break; }
            std::thread::sleep(Duration::from_millis(50));
        }
        if is_running().is_some() { unsafe { libc_kill(pid as i32, 9) }; }
        let _ = std::fs::remove_file(config::pid_path());
        let _ = std::fs::remove_file(config::socket_path());
        Ok(())
    } else if socket_alive() {
        // Started some other way (no pid file): ask it nicely over the socket.
        Err("daemon is running but was not started by `fore start`; use `fore shutdown`".into())
    } else {
        Err("not running".into())
    }
}

fn rotate_log(log: &PathBuf) {
    if let Ok(m) = std::fs::metadata(log)
        && m.len() > 5 * 1024 * 1024 {
            let _ = std::fs::rename(log, log.with_extension("log.1"));
        }
}

// ---------------------------------------------------------------------------
// Login autostart
// ---------------------------------------------------------------------------

fn uid() -> u32 { unsafe { libc_getuid() } }
unsafe extern "C" { #[link_name = "getuid"] fn libc_getuid() -> u32; }

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}
fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl").arg("--user").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

pub enum Manager { Launchd, Systemd, None }

pub fn manager() -> Manager {
    if cfg!(target_os = "macos") { return Manager::Launchd; }
    if Command::new("systemctl").args(["--user", "--version"]).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false) {
        return Manager::Systemd;
    }
    Manager::None
}

pub fn unit_path() -> PathBuf {
    match manager() {
        Manager::Launchd => config::home().join("Library/LaunchAgents/dev.fore.daemon.plist"),
        Manager::Systemd => config::config_dir().parent().unwrap_or(&config::home().join(".config")).join("systemd/user/fore.service"),
        Manager::None => config::config_dir().join("no-service-manager"),
    }
}

pub fn install_autostart(exe: &std::path::Path) -> Result<String, String> {
    let path = unit_path();
    if let Some(p) = path.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
    let log = config::log_path();
    let _ = std::fs::create_dir_all(log.parent().unwrap());
    match manager() {
        Manager::Launchd => {
            let plist = launchd_plist(exe);
            std::fs::write(&path, plist).map_err(|e| e.to_string())?;
            let domain = format!("gui/{}", uid());
            let _ = launchctl(&["bootout", &format!("{domain}/dev.fore.daemon")]);
            // bootstrap is the modern verb (10.10+); fall back to load for very old systems.
            if !launchctl(&["bootstrap", &domain, path.to_str().unwrap()]) && !launchctl(&["load", "-w", path.to_str().unwrap()]) {
                return Err("launchctl bootstrap failed".into());
            }
            Ok(format!("launchd agent installed: {}", path.display()))
        }
        Manager::Systemd => {
            std::fs::write(&path, systemd_unit(exe)).map_err(|e| e.to_string())?;
            systemctl(&["daemon-reload"]);
            if !systemctl(&["enable", "--now", "fore.service"]) {
                let _ = std::fs::remove_file(&path);
                return Err("systemctl --user enable --now fore.service failed (no user session bus? try `loginctl enable-linger $USER`)".into());
            }
            Ok(format!("systemd user service installed and started: {}", path.display()))
        }
        Manager::None => Err("no launchd/systemd user session found — the zsh plugin auto-starts the daemon instead".into()),
    }
}

pub fn launchd_plist(exe: &std::path::Path) -> String {
    let log = config::log_path();
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.fore.daemon</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
  </dict>
</dict></plist>
"#, exe = exe.display(), log = log.display(), home = config::home().display())
}

pub fn systemd_unit(exe: &std::path::Path) -> String {
    let log = config::log_path();
    format!(r#"[Unit]
Description=fore shell copilot daemon
After=default.target

[Service]
Type=simple
ExecStart={exe} daemon
Restart=on-failure
RestartSec=2
Environment=HOME={home}
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
"#, exe = exe.display(), home = config::home().display(), log = log.display())
}

pub fn uninstall_autostart() -> Result<String, String> {
    let path = unit_path();
    match manager() {
        Manager::Launchd => { let _ = launchctl(&["bootout", &format!("gui/{}/dev.fore.daemon", uid())]); }
        Manager::Systemd => { let _ = systemctl(&["disable", "--now", "fore.service"]); systemctl(&["daemon-reload"]); }
        Manager::None => {}
    }
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        Ok(format!("removed autostart {}", path.display()))
    } else {
        Ok("no autostart was installed".into())
    }
}

pub fn autostart_installed() -> bool {
    !matches!(manager(), Manager::None) && unit_path().exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_files_reference_binary_and_log() {
        let exe = PathBuf::from("/usr/local/bin/fore");
        let u = systemd_unit(&exe);
        assert!(u.contains("ExecStart=/usr/local/bin/fore daemon"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
        let p = launchd_plist(&exe);
        assert!(p.contains("<string>/usr/local/bin/fore</string><string>daemon</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert_eq!(p.matches("<dict>").count(), p.matches("</dict>").count());
    }
}
