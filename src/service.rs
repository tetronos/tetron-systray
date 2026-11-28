//! Per-user service install/uninstall: a `systemd --user` unit on Linux, a
//! launchd **LaunchAgent** on macOS. Same shape as `tetron-webui`'s own
//! `src/service.rs` (per-user, not system-wide -- no root needed, only
//! makes sense inside a login session), adapted for a tray app instead of
//! an HTTP server: there's no port to poll for "did it actually come up",
//! so this checks the service manager's own reported state instead
//! (`systemctl --user is-active` on Linux; `launchctl list` on macOS).
//!
//! Also unlike `tetron-webui`, this needs a *graphical* session specifically
//! (a tray icon + gtk event loop), not just a login session -- the Linux
//! unit targets `graphical-session.target`, not `default.target`.

#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::{Command, Stdio};
#[cfg(not(windows))]
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

fn run_cmd(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: `{program}` exited with {status}"),
        Err(e) => eprintln!("warning: failed to run `{program}`: {e}"),
    }
}

/// Used for best-effort teardown before a fresh macOS launchd load; unused
/// on Linux (same `#[allow(dead_code)]` reasoning as tetron/tetron-webui's
/// own `service.rs`).
#[allow(dead_code)]
fn run_cmd_quiet(program: &str, args: &[&str]) {
    let _ = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("could not determine config directory")?
        .join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("tetron-systray.service"))
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("com.tetron.systray.plist"))
}

#[cfg(target_os = "macos")]
fn log_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Library/Logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("tetron-systray.log"))
}

/// `~/Applications/TetronSystray.app`. Not `NSApplicationActivationPolicy`
/// itself but a prerequisite for it: without a real bundle (`CFBundleExecutable`/
/// `CFBundleIdentifier`/`LSUIElement` in `Contents/Info.plist`), the status
/// item never actually appears in the menu bar at all -- found live-testing
/// on a real M1 Mac (macOS 26), see `src/main.rs`'s own top-level doc comment
/// for the full story. A bare `current_exe()` binary (this function's
/// pre-2026-07-24 behavior) is not enough.
#[cfg(target_os = "macos")]
fn bundle_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Applications/TetronSystray.app");
    std::fs::create_dir_all(dir.join("Contents/MacOS"))?;
    Ok(dir)
}

/// `tetron-systray install`: write the unit/plist (substituting the path of
/// the binary currently running, same idempotent-on-every-install pattern
/// tetron's own `ensure_service_installed` and tetron-webui's `install`
/// use), enable it, and wait for the service manager to report it active
/// before declaring success.
///
/// The webui port is injected as `TETRON_WEBUI_PORT` so the tray's "open
/// webui" menu item opens the correct URL when running as a service.
///
/// **Always forces a restart, not just enable/start.** Found live
/// (2026-07-24, testing tetron-webui's addon-install framework): running
/// `install` again to relocate/upgrade an *already-active* instance --
/// e.g. a fresh binary downloaded to a new path -- rewrites the unit's
/// `ExecStart` correctly, but `enable --now` is a no-op on an already-
/// running unit; the live process keeps executing whatever binary it
/// originally loaded, never picking up the new one. `restart` (unlike
/// `start`) unconditionally reloads, whether the unit was previously
/// stopped or running, so `install` now always ends with the actual
/// current on-disk binary in memory. macOS's own `unload`+`load` cycle
/// just below already had this property -- only Linux's `enable --now`
/// needed the fix.
pub fn install(port: u16) -> Result<()> {
    println!("installing tetron-systray {}", crate::FULL_VERSION);
    let exe = std::env::current_exe()
        .context("failed to determine current executable path")?
        .to_string_lossy()
        .into_owned();
    #[cfg(windows)]
    return windows::install(&exe, port);
    #[cfg(not(windows))]
    unix_install(&exe, port)
}

/// Linux and macOS: write and load the unit/plist, then wait for it.
#[cfg(not(windows))]
fn unix_install(exe: &str, port: u16) -> Result<()> {
    let port_str = port.to_string();

    #[cfg(target_os = "linux")]
    {
        let path = unit_path()?;
        let unit = include_str!("../contrib/tetron-systray.service")
            .replace("/usr/local/bin/tetron-systray", exe)
            .replace("__TETRON_WEBUI_PORT__", &port_str);
        std::fs::write(&path, unit)
            .with_context(|| format!("failed to write {}", path.display()))?;
        run_cmd("systemctl", &["--user", "daemon-reload"]);
        run_cmd("systemctl", &["--user", "enable", "tetron-systray"]);
        run_cmd("systemctl", &["--user", "restart", "tetron-systray"]);
    }

    #[cfg(target_os = "macos")]
    {
        // A real app bundle (Contents/MacOS/<exe> + Contents/Info.plist with
        // CFBundleIdentifier/LSUIElement) is a prerequisite for the status
        // item to appear in the menu bar at all, not just an aesthetic
        // nicety -- see bundle_dir()'s own doc comment. Copy (not symlink)
        // the running binary in, so a stale copy can't be swapped out from
        // under the running process's own executable inode.
        let bundle = bundle_dir()?;
        let bundled_exe = bundle.join("Contents/MacOS/tetron-systray");
        std::fs::copy(exe, &bundled_exe)
            .with_context(|| format!("failed to copy binary into {}", bundled_exe.display()))?;
        let info_plist = include_str!("../contrib/TetronSystray-Info.plist")
            .replace("__VERSION__", env!("CARGO_PKG_VERSION"));
        std::fs::write(bundle.join("Contents/Info.plist"), info_plist).with_context(|| {
            format!(
                "failed to write {}",
                bundle.join("Contents/Info.plist").display()
            )
        })?;

        let path = plist_path()?;
        let log = log_path()?.to_string_lossy().into_owned();
        let plist = include_str!("../contrib/com.tetron.systray.plist")
            .replace(
                "/usr/local/bin/tetron-systray",
                &bundled_exe.to_string_lossy(),
            )
            .replace("/tmp/tetron-systray.log", &log)
            .replace("__TETRON_WEBUI_PORT__", &port_str);
        std::fs::write(&path, plist)
            .with_context(|| format!("failed to write {}", path.display()))?;
        run_cmd_quiet("launchctl", &["unload", &path.to_string_lossy()]);
        run_cmd("launchctl", &["load", "-w", &path.to_string_lossy()]);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("per-user service install not supported on this platform");

    eprintln!("waiting for tetron-systray to start…");
    if wait_for_active(Duration::from_secs(10)) {
        println!("tetron-systray service installed and running.");
        Ok(())
    } else {
        anyhow::bail!(
            "service was installed but never reported active.\n\
             Check the service logs (journalctl --user -u tetron-systray on Linux, or the log path in the plist on macOS)."
        );
    }
}

/// `tetron-systray uninstall`: stop, disable, and remove the unit/plist.
pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = unit_path()?;
        if path.exists() {
            run_cmd(
                "systemctl",
                &["--user", "disable", "--now", "tetron-systray"],
            );
            std::fs::remove_file(&path)?;
            run_cmd("systemctl", &["--user", "daemon-reload"]);
            println!("Removed systemd --user service.");
        } else {
            println!("Service not installed.");
        }
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let path = plist_path()?;
        if path.exists() {
            run_cmd("launchctl", &["unload", "-w", &path.to_string_lossy()]);
            std::fs::remove_file(&path)?;
            println!("Removed launchd LaunchAgent.");
        } else {
            println!("Service not installed.");
        }
        let bundle = dirs::home_dir()
            .context("could not determine home directory")?
            .join("Applications/TetronSystray.app");
        if bundle.exists() {
            std::fs::remove_dir_all(&bundle)
                .with_context(|| format!("failed to remove {}", bundle.display()))?;
        }
        return Ok(());
    }

    #[cfg(windows)]
    return windows::uninstall();

    #[allow(unreachable_code)]
    {
        anyhow::bail!("per-user service uninstall not supported on this platform");
    }
}

/// Windows: started by double-click (a console of its own) from anywhere but
/// its installed location, as when a downloaded file is opened from Explorer:
/// install instead of running a second, uninstalled tray, then wait for Enter
/// so the window does not vanish. Returns `false` when this is not such a
/// start (a terminal, or the sign-in autostart of the installed copy).
#[cfg(windows)]
pub fn double_click_install() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut pids = [0u32; 2];
    // SAFETY: `pids` is writable for its length.
    let sharing = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if sharing != 1 {
        return false;
    }
    let installed = std::env::current_exe()
        .ok()
        .zip(windows::install_dir().ok())
        .is_some_and(|(exe, dir)| exe.starts_with(dir));
    if installed {
        return false;
    }
    println!("tetron-systray: installing for your user (no administrator rights needed)...\n");
    let port = std::env::var("TETRON_WEBUI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7870);
    if let Err(e) = install(port) {
        println!("\ninstall failed: {e:#}");
    }
    println!("\nPress Enter to close this window.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    true
}

/// Windows: one tray per sign-in session. A named mutex in the session's
/// `Local\` namespace is held for the life of the tray; a second start finds
/// it and exits instead of adding a second icon.
#[cfg(windows)]
pub fn already_running() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\tetron-systray"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `name` is NUL-terminated; the handle is deliberately kept open
    // (never closed) so the mutex lives as long as this process.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    // SAFETY: reads this thread's last error, set by CreateMutexW.
    !handle.is_null() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS
}

/// Windows: a per-user autostart entry, the counterpart of the systemd user
/// unit and the LaunchAgent. The program is copied to
/// `%LOCALAPPDATA%\Programs\tetron-systray` (so the downloaded file can be
/// deleted) and started at sign-in by a value under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. No administrator
/// rights are needed: everything is in the user's own profile.
#[cfg(windows)]
mod windows {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use anyhow::{Context, Result};

    use super::{run_cmd, run_cmd_quiet};

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "tetron-systray";
    const EXE_NAME: &str = "tetron-systray.exe";
    const DEFAULT_WEBUI_PORT: u16 = 7870;

    pub(crate) fn install_dir() -> Result<PathBuf> {
        Ok(dirs::data_local_dir()
            .context("could not determine %LOCALAPPDATA%")?
            .join("Programs")
            .join("tetron-systray"))
    }

    /// Ends every other running tetron-systray of this user, never this
    /// process (install and uninstall run as tetron-systray too). Matched by
    /// image name prefix, so a copy started from the download
    /// (`tetron-systray-x86_64-windows.exe`) is stopped too; otherwise it
    /// would keep the one-tray lock and the installed tray would exit.
    fn stop_other_instances() {
        let pid = std::process::id().to_string();
        run_cmd_quiet(
            "taskkill",
            &[
                "/F",
                "/FI",
                "IMAGENAME eq tetron-systray*",
                "/FI",
                &format!("PID ne {pid}"),
            ],
        );
        // Let the lock of a stopped tray go before the new one starts.
        std::thread::sleep(Duration::from_millis(500));
    }

    /// The autostart command line: the quoted program, plus the web UI port
    /// when it is not the default (a Run entry can not set environment).
    pub(super) fn run_command(exe: &Path, port: u16) -> String {
        let mut cmd = format!("\"{}\"", exe.display());
        if port != DEFAULT_WEBUI_PORT {
            cmd.push_str(&format!(" --webui-port {port}"));
        }
        cmd
    }

    pub(super) fn install(exe: &str, port: u16) -> Result<()> {
        let dir = install_dir()?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let installed = dir.join(EXE_NAME);
        stop_other_instances();
        if !same_file(Path::new(exe), &installed) {
            // A just-stopped instance may hold the file for a moment.
            let mut copied = std::fs::copy(exe, &installed);
            for _ in 0..10 {
                if copied.is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
                copied = std::fs::copy(exe, &installed);
            }
            copied.with_context(|| {
                format!("failed to copy the program to {}", installed.display())
            })?;
        }
        run_cmd(
            "reg",
            &[
                "add",
                RUN_KEY,
                "/v",
                RUN_VALUE,
                "/t",
                "REG_SZ",
                "/d",
                &run_command(&installed, port),
                "/f",
            ],
        );

        // Start it now, detached from this terminal.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let mut start = Command::new(&installed);
        if port != DEFAULT_WEBUI_PORT {
            start.args(["--webui-port", &port.to_string()]);
        }
        let mut child = start
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .with_context(|| format!("failed to start {}", installed.display()))?;
        eprintln!("waiting for tetron-systray to start…");
        std::thread::sleep(Duration::from_secs(2));
        match child.try_wait() {
            Ok(None) => {
                println!(
                    "tetron-systray installed in {} and running; it starts when you sign in.",
                    dir.display()
                );
                Ok(())
            }
            Ok(Some(status)) => {
                anyhow::bail!("tetron-systray exited right after starting ({status})")
            }
            Err(e) => anyhow::bail!("could not check on tetron-systray: {e}"),
        }
    }

    pub(super) fn uninstall() -> Result<()> {
        let had_entry = Command::new("reg")
            .args(["query", RUN_KEY, "/v", RUN_VALUE])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if had_entry {
            run_cmd("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
            println!("Removed the sign-in autostart entry.");
        } else {
            println!("Autostart entry not installed.");
        }
        stop_other_instances();
        let dir = install_dir()?;
        let running_from_dir = std::env::current_exe()
            .map(|p| p.starts_with(&dir))
            .unwrap_or(false);
        if running_from_dir {
            println!(
                "{} is the running program; delete {} after this command exits.",
                EXE_NAME,
                dir.display()
            );
        } else if dir.exists() {
            std::thread::sleep(Duration::from_millis(500));
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to remove {}", dir.display()))?;
            println!("Removed {}.", dir.display());
        }
        Ok(())
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn run_command_quotes_the_path_and_adds_only_a_custom_port() {
            let exe =
                Path::new(r"C:\Users\me\AppData\Local\Programs\tetron-systray\tetron-systray.exe");
            assert_eq!(
                run_command(exe, 7870),
                r#""C:\Users\me\AppData\Local\Programs\tetron-systray\tetron-systray.exe""#
            );
            assert!(run_command(exe, 8080).ends_with(r#"tetron-systray.exe" --webui-port 8080"#));
        }
    }
}

#[cfg(target_os = "linux")]
fn is_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "tetron-systray"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false)
}

/// macOS equivalent is weaker than the Linux check on purpose, not an
/// oversight: `launchctl list <label>` only confirms the job is *loaded*,
/// not that it's still running a few seconds later (that needs parsing
/// `launchctl print`'s output, which wasn't worth guessing at without real
/// Mac hardware to verify the exact output format against -- see
/// docs/HOWTO.md's "Known gaps"). Good enough to catch "the plist itself
/// was rejected," not to catch "it loaded then immediately crash-looped."
#[cfg(target_os = "macos")]
fn is_active() -> bool {
    Command::new("launchctl")
        .args(["list", "com.tetron.systray"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn wait_for_active(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if is_active() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
