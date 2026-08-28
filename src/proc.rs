//! Shared child-process safety rails.
//!
//! Every external tool prview spawns must:
//! 1. detach stdin, so it can never sit on an interactive prompt inherited from
//!    the operator's terminal (npm's "Ok to proceed?" — the `--deep` hang class),
//! 2. run under `kill_on_drop`, so a dropped wait-future reaps the direct child,
//! 3. on unix lead its own process group (`process_group(0)`), so one SIGKILL to
//!    `-pgid` takes down the WHOLE tree (cargo → rustc → cc, npx → node → tool),
//!    not just the direct child — `kill_on_drop` alone leaves grandchildren.
//!
//! Three call sites (checks, heuristics, mcp) each carried a copy of this logic
//! plus a copy of the grandchild-kill test; commit 8be898a had to close the hang
//! class in all three at once. This module is the single home.

use std::process::Output;
use std::time::Duration;
use tokio::process::Command as TokioCommand;

/// SIGKILL an entire unix process group by its leader pid.
///
/// The child must have been spawned with `process_group(0)` so `pid` is also
/// the pgid; signalling `-pid` then reaches the wrapper AND its grandchildren.
#[cfg(unix)]
pub fn sigkill_process_group(pid: u32) {
    // SAFETY: plain kill(2) syscall; ESRCH (already gone) / EPERM are ignored.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Terminate the full process tree led by `pid` on every supported platform.
///
/// Unix children lead their own process group, so one negative-pgid SIGKILL is
/// sufficient. Windows has no inherited Unix-style process group contract;
/// the built-in `taskkill /T /F` primitive walks and force-terminates the tree.
pub fn terminate_process_tree(pid: u32) {
    #[cfg(unix)]
    sigkill_process_group(pid);

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(not(any(unix, windows)))]
    compile_error!("prview process-tree cancellation is unsupported on this platform");
}

/// Apply the standard rails to `cmd`: detached stdin, `kill_on_drop`, and (unix)
/// its own process group. Stdout/stderr are left to the caller — piped for
/// captured runs, redirected to files for detached packs.
pub fn harden(cmd: &mut TokioCommand) {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    // unix: own process group so one signal to -pgid reaches the whole tree.
    #[cfg(unix)]
    cmd.process_group(0);
}

/// The half of [`harden`] that a synchronous [`std::process::Command`] can take:
/// detached stdin and, on unix, its own process group.
///
/// `kill_on_drop` has no std equivalent — a caller here owns the `Child` and
/// reaps it itself. The process group is the half that matters anyway: the
/// context stage spawns `sh -c 'pnpm exec …'` wrappers whose grandchildren
/// outlive a plain `Child::kill`, and it is the pgid that the run's resource
/// governor signals on cancellation.
pub fn harden_std(cmd: &mut std::process::Command) {
    cmd.stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
}

/// Spawn `cmd` under the standard rails with piped output, drain stdout+stderr
/// concurrently (a high-output child cannot deadlock on a full pipe buffer),
/// and enforce `timeout`.
///
/// On timeout the wait-future is dropped (so `kill_on_drop` reaps the direct
/// child) and, on unix, the whole process group is SIGKILLed so grandchildren
/// die too; the returned error is `on_timeout()`. `label` names the tool in the
/// spawn/wait error messages.
///
/// This is the one place a check's external tool is spawned, so it is also where
/// the pid is handed to the run's resource governor
/// ([`crate::governor::with_child_scope`]) — that is what lets one Ctrl-C reach a
/// `cargo` → `rustc` → `cc` tree instead of leaving it behind. Outside a
/// governed scope the registration is a no-op.
pub async fn run_capture_with_timeout(
    cmd: TokioCommand,
    timeout: Duration,
    label: &str,
    on_timeout: impl FnOnce() -> anyhow::Error,
) -> anyhow::Result<Output> {
    run_capture_with_timeout_after_spawn(cmd, timeout, label, on_timeout, |_| {}).await
}

async fn run_capture_with_timeout_after_spawn(
    mut cmd: TokioCommand,
    timeout: Duration,
    label: &str,
    on_timeout: impl FnOnce() -> anyhow::Error,
    after_spawn: impl FnOnce(Option<u32>),
) -> anyhow::Result<Output> {
    harden(&mut cmd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {label}: {e}"))?;

    // Capture the pid (also the pgid, since the child leads its group) BEFORE
    // the handle moves into wait_with_output(); needed to signal the group.
    let pid = child.id();

    after_spawn(child.id());

    // Held for the whole wait: dropping it unregisters, so the success, timeout
    // and wait-error paths all leave the registry clean without saying so.
    let _registration = child.id().and_then(crate::governor::register_active_child);

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| anyhow::anyhow!("failed to run {label}: {e}")),
        Err(_) => {
            if let Some(pid) = pid {
                terminate_process_tree(pid);
            }
            Err(on_timeout())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_capture_with_timeout_returns_output_on_success() {
        let mut cmd = TokioCommand::new("echo");
        cmd.arg("hello");
        let output = run_capture_with_timeout(cmd, Duration::from_secs(10), "echo", || {
            anyhow::anyhow!("unexpected timeout")
        })
        .await
        .expect("echo should succeed");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[tokio::test]
    async fn run_capture_with_timeout_errors_via_on_timeout() {
        let mut cmd = TokioCommand::new("sleep");
        cmd.arg("30");
        let err = run_capture_with_timeout(cmd, Duration::from_secs(1), "sleep", || {
            anyhow::anyhow!("slept too long")
        })
        .await
        .expect_err("sleep 30 with a 1s budget must time out");
        assert!(err.to_string().contains("slept too long"));
    }

    #[tokio::test]
    async fn run_capture_with_timeout_detaches_stdin() {
        // `cat` with an inherited terminal stdin would block forever; with stdin
        // detached it sees EOF and exits at once. Guards the npm prompt scenario.
        let cmd = TokioCommand::new("cat");
        let output = run_capture_with_timeout(cmd, Duration::from_secs(5), "cat", || {
            anyhow::anyhow!("cat hung")
        })
        .await
        .expect("cat with null stdin exits immediately");
        assert!(output.status.success());
    }

    /// Canonical grandchild kill via the capture helper: a timed-out command
    /// must take its whole process group down, not just the direct child. `sh`
    /// leads the group and records a distinct grandchild `sleep` pid; after the
    /// timeout-kill that grandchild must be gone. Without `process_group(0)`,
    /// kill_on_drop reaps only `sh` and the grandchild survives.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_capture_with_timeout_kills_process_group() {
        use std::io::Read;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());

        let mut cmd = TokioCommand::new("sh");
        cmd.arg("-c").arg(&script);
        let err = run_capture_with_timeout(cmd, Duration::from_secs(1), "sh-tree", || {
            anyhow::anyhow!("sh-tree timed out")
        })
        .await
        .expect_err("sh tree with a 1s budget must time out");
        assert!(err.to_string().contains("sh-tree timed out"));

        let mut s = String::new();
        std::fs::File::open(&pidfile)
            .expect("sh should have recorded the grandchild pid")
            .read_to_string(&mut s)
            .expect("read pidfile");
        let grandchild: i32 = s.trim().parse().expect("grandchild pid");

        assert_grandchild_reaped(grandchild).await;
    }

    /// Deterministic version of the spawn/register race: cancellation drains
    /// an empty registry after the process group exists but before its pid is
    /// registered. Registration must close that window instead of making the
    /// command wait for its ordinary timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn registration_after_cancellation_kills_the_async_process_group() {
        use std::io::Read;
        use std::sync::Arc;
        use std::thread::sleep;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("late-grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());
        let mut cmd = TokioCommand::new("sh");
        cmd.arg("-c").arg(&script);

        let governor = Arc::new(crate::governor::ResourceGovernor::new());
        let canceller = Arc::clone(&governor);
        let marker = pidfile.clone();
        let run = run_capture_with_timeout_after_spawn(
            cmd,
            Duration::from_secs(2),
            "late-async-tree",
            || anyhow::anyhow!("late async tree timed out"),
            move |_| {
                for _ in 0..100 {
                    if marker.exists() {
                        break;
                    }
                    sleep(Duration::from_millis(10));
                }
                assert!(marker.exists(), "child must publish its grandchild pid");
                canceller.cancel();
            },
        );

        let output =
            crate::governor::with_child_scope(Arc::clone(&governor), "late-async-tree", run)
                .await
                .expect("late registration must kill before the timeout branch wins");

        let mut contents = String::new();
        std::fs::File::open(&pidfile)
            .expect("grandchild pidfile")
            .read_to_string(&mut contents)
            .expect("read grandchild pid");
        let grandchild = contents.trim().parse().expect("numeric grandchild pid");
        assert_grandchild_reaped(grandchild).await;
        assert_eq!(governor.inflight_count(), 0);
        assert!(
            !output.status.success(),
            "the cancelled process group must not report a successful command",
        );
    }

    /// Direct primitive test: `sigkill_process_group` reaps a grandchild through
    /// a process-group signal (the path the mcp quick-timeout uses directly).
    #[cfg(unix)]
    #[test]
    fn sigkill_process_group_reaps_grandchild() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        use std::thread::sleep;

        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(&script);
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sh tree");
        let leader = child.id();

        let mut grandchild = None;
        for _ in 0..100 {
            if let Ok(mut f) = std::fs::File::open(&pidfile) {
                let mut s = String::new();
                f.read_to_string(&mut s).ok();
                if let Ok(pid) = s.trim().parse::<i32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            sleep(Duration::from_millis(20));
        }
        let grandchild = grandchild.expect("sh should record the grandchild pid");

        sigkill_process_group(leader);
        let _ = child.wait();

        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 is a read-only existence/permission probe, and
            // `grandchild` came from the process tree created by this test.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) || errno == Some(libc::EPERM) {
                    gone = true;
                    break;
                }
            }
            sleep(Duration::from_millis(20));
        }
        assert!(gone, "grandchild {grandchild} survived the group kill");
    }

    /// Poll until `grandchild` is no longer a signalable live process of ours.
    /// Tolerate ESRCH (gone) or EPERM (pid reused / sandbox signal limits) —
    /// both mean it is reaped (CLAUDE.md #14 signal flake class).
    #[cfg(unix)]
    async fn assert_grandchild_reaped(grandchild: i32) {
        let mut gone = false;
        for _ in 0..60 {
            // SAFETY: signal 0 is a read-only existence/permission probe, and
            // `grandchild` came from the process tree created by this test.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) || errno == Some(libc::EPERM) {
                    gone = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            gone,
            "grandchild {grandchild} survived; group kill did not reach it"
        );
    }

    /// Real Windows tree proof: root PowerShell -> child PowerShell -> sleeping
    /// grandchild. `taskkill /T /F` must remove both descendants.
    #[cfg(windows)]
    #[test]
    fn terminate_process_tree_reaps_windows_child_and_grandchild() {
        use std::thread::sleep;

        let tmp = tempfile::tempdir().expect("tempdir");
        let child_pidfile = tmp.path().join("child.pid");
        let grandchild_pidfile = tmp.path().join("grandchild.pid");
        let child_script = tmp.path().join("child.ps1");
        let parent_script = tmp.path().join("parent.ps1");

        std::fs::write(
            &child_script,
            format!(
                "$g = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60'\nSet-Content -LiteralPath '{}' -Value $g.Id\nWait-Process -Id $g.Id\n",
                grandchild_pidfile.display()
            ),
        )
        .expect("write child script");
        std::fs::write(
            &parent_script,
            format!(
                "$c = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','{}'\nSet-Content -LiteralPath '{}' -Value $c.Id\nWait-Process -Id $c.Id\n",
                child_script.display(),
                child_pidfile.display()
            ),
        )
        .expect("write parent script");

        let mut root = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&parent_script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn root powershell");

        for _ in 0..200 {
            if child_pidfile.exists() && grandchild_pidfile.exists() {
                break;
            }
            sleep(Duration::from_millis(25));
        }
        let child_pid = read_windows_pid(&child_pidfile);
        let grandchild_pid = read_windows_pid(&grandchild_pidfile);

        terminate_process_tree(root.id());
        let _ = root.wait();

        for pid in [child_pid, grandchild_pid] {
            let mut gone = false;
            for _ in 0..100 {
                if !windows_pid_exists(pid) {
                    gone = true;
                    break;
                }
                sleep(Duration::from_millis(25));
            }
            assert!(gone, "Windows descendant {pid} survived taskkill /T /F");
        }
    }

    #[cfg(windows)]
    fn read_windows_pid(path: &std::path::Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    #[cfg(windows)]
    fn windows_pid_exists(pid: u32) -> bool {
        let filter = format!("PID eq {pid}");
        let output = std::process::Command::new("tasklist.exe")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist must be available on a Windows runner");
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .any(|line| line.contains(&format!("\"{pid}\"")))
    }
}
