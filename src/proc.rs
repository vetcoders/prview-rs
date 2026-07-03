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

/// Apply the standard rails to `cmd`: detached stdin, `kill_on_drop`, and (unix)
/// its own process group. Stdout/stderr are left to the caller — piped for
/// captured runs, redirected to files for detached packs.
pub fn harden(cmd: &mut TokioCommand) {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    // unix: own process group so one signal to -pgid reaches the whole tree.
    #[cfg(unix)]
    cmd.process_group(0);
}

/// Spawn `cmd` under the standard rails with piped output, drain stdout+stderr
/// concurrently (a high-output child cannot deadlock on a full pipe buffer),
/// and enforce `timeout`.
///
/// On timeout the wait-future is dropped (so `kill_on_drop` reaps the direct
/// child) and, on unix, the whole process group is SIGKILLed so grandchildren
/// die too; the returned error is `on_timeout()`. `label` names the tool in the
/// spawn/wait error messages.
pub async fn run_capture_with_timeout(
    mut cmd: TokioCommand,
    timeout: Duration,
    label: &str,
    on_timeout: impl FnOnce() -> anyhow::Error,
) -> anyhow::Result<Output> {
    harden(&mut cmd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {label}: {e}"))?;

    // Capture the pid (also the pgid, since the child leads its group) BEFORE
    // the handle moves into wait_with_output(); needed to signal the group.
    #[cfg(unix)]
    let pid = child.id();

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| anyhow::anyhow!("failed to run {label}: {e}")),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                sigkill_process_group(pid);
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
}
