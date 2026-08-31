//! Shared child-process safety rails.
//!
//! Every external tool prview spawns must:
//! 1. detach stdin, so it can never sit on an interactive prompt inherited from
//!    the operator's terminal (npm's "Ok to proceed?" — the `--deep` hang class),
//! 2. run under `kill_on_drop`, so a dropped wait-future reaps the direct child,
//! 3. on unix lead its own process group (`process_group(0)`), so one SIGKILL to
//!    `-pgid` takes down the WHOLE tree (cargo → rustc → cc, npx → node → tool),
//!    not just the direct child — `kill_on_drop` alone leaves grandchildren,
//! 4. on Windows own a Job Object, so the tree remains killable after a short-
//!    lived wrapper exits and its PID can no longer be passed to `taskkill /T`.
//!
//! Three call sites (checks, heuristics, mcp) each carried a copy of this logic
//! plus a copy of the grandchild-kill test; commit 8be898a had to close the hang
//! class in all three at once. This module is the single home.

use std::process::Output;
use std::time::Duration;
use tokio::process::Command as TokioCommand;

#[cfg(windows)]
fn system_taskkill_path() -> std::path::PathBuf {
    let windows_dir = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    windows_dir.join("System32").join("taskkill.exe")
}

#[cfg(windows)]
fn taskkill_process_tree_at(
    taskkill: &std::path::Path,
    pid: u32,
) -> std::io::Result<std::process::ExitStatus> {
    std::process::Command::new(taskkill)
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
}

#[cfg(any(windows, test))]
fn terminate_windows_process_tree_with(
    taskkill: &std::path::Path,
    pid: u32,
    executor: impl FnOnce(&std::path::Path, u32) -> std::io::Result<std::process::ExitStatus>,
) -> std::io::Result<()> {
    match executor(taskkill, pid) {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(std::io::Error::other(format!(
            "{} failed to terminate process tree {pid} (status {status})",
            taskkill.display()
        ))),
        Err(err) => Err(std::io::Error::new(
            err.kind(),
            format!(
                "failed to run {} for process tree {pid}: {err}",
                taskkill.display()
            ),
        )),
    }
}

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
        let taskkill = system_taskkill_path();
        if let Err(err) =
            terminate_windows_process_tree_with(&taskkill, pid, taskkill_process_tree_at)
        {
            eprintln!("prview: {err}");
        }
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

pub(crate) fn spawn_owned_std_child(
    cmd: std::process::Command,
) -> std::io::Result<Box<dyn process_wrap::std::ChildWrapper>> {
    use process_wrap::std::CommandWrap;

    let mut wrapped = CommandWrap::from(cmd);
    #[cfg(windows)]
    wrapped.wrap(process_wrap::std::JobObject);
    wrapped.spawn()
}

/// Terminate every process owned by a synchronous child wrapper, including the
/// descendants of a Windows root process that has already exited.
pub fn terminate_owned_std_child(child: &mut dyn process_wrap::std::ChildWrapper) {
    #[cfg(unix)]
    terminate_process_tree(child.id());

    #[cfg(windows)]
    if let Err(err) = child.start_kill() {
        eprintln!("prview: failed to terminate Windows Job Object: {err}");
    }
}

/// Spawn a synchronous child, register it with the current run governor if any,
/// and wait. A cancelled run kills the process group instead of waiting out a
/// `git fetch` or `git archive | tar`.
pub fn spawn_wait_governed(
    mut cmd: std::process::Command,
    label: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    harden_std(&mut cmd);
    if crate::governor::current_run_governor().is_some_and(|governor| governor.is_cancelled()) {
        return Err(crate::governor::Cancelled.into());
    }
    let mut child =
        spawn_owned_std_child(cmd).map_err(|e| anyhow::anyhow!("failed to spawn {label}: {e}"))?;
    let _registration = crate::governor::register_run_child(child.id(), label);
    child
        .wait()
        .map_err(|e| anyhow::anyhow!("failed to wait for {label}: {e}"))
}

fn spawn_owned_tokio_child(
    cmd: TokioCommand,
) -> std::io::Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
    use process_wrap::tokio::{CommandWrap, KillOnDrop};

    let mut wrapped = CommandWrap::from(cmd);
    wrapped.wrap(KillOnDrop);
    #[cfg(windows)]
    wrapped.wrap(process_wrap::tokio::JobObject);
    wrapped.spawn()
}

async fn wait_for_direct_child_exit(
    child: &mut dyn process_wrap::tokio::ChildWrapper,
) -> std::io::Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn terminate_owned_tokio_child(
    child: &mut dyn process_wrap::tokio::ChildWrapper,
    pid: Option<u32>,
) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        sigkill_process_group(pid);
    }

    // On Windows JobObjectChild::start_kill terminates the job even when the
    // direct root has already exited. On Unix the process-group signal above
    // owns descendants; start_kill is still useful as direct-child fallback.
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Spawn `cmd` under the standard rails with piped output, drain stdout+stderr
/// concurrently (a high-output child cannot deadlock on a full pipe buffer),
/// and enforce `timeout`.
///
/// One deadline covers child wait and pipe drain. On timeout the whole process
/// tree is terminated, the root is reaped, and both reader tasks are aborted
/// and awaited before `on_timeout()` is returned. A successful wrapper exit
/// also terminates residual group members before the registration guard is
/// dropped: Unix signals the still-owned process group and Windows terminates
/// the still-owned Job Object, so a background descendant cannot escape just
/// because its root PID exited first or keep inherited pipes alive.
/// `label` names the tool in spawn/wait errors.
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

    // One deadline owns the complete command lifecycle. A child that exits
    // while a descendant keeps an inherited pipe open must not turn a 5-minute
    // command timeout into an unbounded output-drain wait.
    let deadline = tokio::time::Instant::now() + timeout;

    let mut child = spawn_owned_tokio_child(cmd)
        .map_err(|e| anyhow::anyhow!("failed to spawn {label}: {e}"))?;

    // Capture the pid (also the pgid, since the child leads its group) BEFORE
    // waiting; needed to signal the group. Keep the handle until after that
    // signal so `kill_on_drop` cannot reap the root first.
    let pid = child.id();

    after_spawn(child.id());

    // Held for the whole wait: dropping it unregisters, so the success, timeout
    // and wait-error paths all leave the registry clean without saying so.
    let registration = child.id().and_then(crate::governor::register_active_child);

    let stdout_pipe = child.stdout().take();
    let stderr_pipe = child.stderr().take();
    let mut stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
        }
        buf
    });
    let mut stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
        }
        buf
    });

    // `JobObjectChild::wait` deliberately waits for every descendant. Poll the
    // direct root instead, so root-exits-first is observed immediately and the
    // owned job can be terminated rather than consuming the command timeout.
    match tokio::time::timeout_at(deadline, wait_for_direct_child_exit(child.as_mut())).await {
        Ok(Ok(status)) => {
            terminate_owned_tokio_child(child.as_mut(), pid).await;
            // The direct child is reaped. Keeping its pid registered while
            // draining buffered output risks signalling a later pid reuse.
            drop(registration);

            let drained = tokio::time::timeout_at(deadline, async {
                let stdout = (&mut stdout_task).await.unwrap_or_else(|_| Vec::new());
                let stderr = (&mut stderr_task).await.unwrap_or_else(|_| Vec::new());
                (stdout, stderr)
            })
            .await;
            match drained {
                Ok((stdout, stderr)) => Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                }),
                Err(_) => {
                    stdout_task.abort();
                    stderr_task.abort();
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    Err(on_timeout())
                }
            }
        }
        Ok(Err(e)) => {
            terminate_owned_tokio_child(child.as_mut(), pid).await;
            drop(registration);
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            Err(anyhow::anyhow!("failed to run {label}: {e}"))
        }
        Err(_) => {
            terminate_owned_tokio_child(child.as_mut(), pid).await;
            drop(registration);
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            Err(on_timeout())
        }
    }
}

/// A real PowerShell root/child/grandchild tree for Windows-only process tests.
///
/// This lives outside either test module so the direct process primitive and
/// the governor integration test exercise the same fixture and PID probe. Drop
/// is failure cleanup only: successful tests first prove every captured PID is
/// gone through the operation under test, then disarm cleanup.
#[cfg(all(test, windows))]
pub(crate) struct WindowsProcessTree {
    root: std::process::Child,
    pids: Vec<u32>,
    _tempdir: tempfile::TempDir,
    verified_gone: bool,
}

#[cfg(all(test, windows))]
impl WindowsProcessTree {
    pub(crate) fn spawn(label: &str) -> Self {
        use std::thread::sleep;

        let tempdir = tempfile::tempdir().expect("Windows process-tree tempdir");
        let child_pidfile = tempdir.path().join("child.pid");
        let grandchild_pidfile = tempdir.path().join("grandchild.pid");
        let child_script = tempdir.path().join("child.ps1");
        let parent_script = tempdir.path().join("parent.ps1");

        std::fs::write(
            &child_script,
            format!(
                "$g = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60'\nSet-Content -LiteralPath '{}' -Value $g.Id\nWait-Process -Id $g.Id\n",
                grandchild_pidfile.display()
            ),
        )
        .expect("write Windows child script");
        std::fs::write(
            &parent_script,
            format!(
                "$c = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','{}'\nSet-Content -LiteralPath '{}' -Value $c.Id\nWait-Process -Id $c.Id\n",
                child_script.display(),
                child_pidfile.display()
            ),
        )
        .expect("write Windows parent script");

        let root = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&parent_script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn root PowerShell");

        // Establish cleanup ownership before waiting for either descendant to
        // publish its PID. If setup times out or panics, Drop can already kill
        // the root with /T and therefore cannot orphan an unrecorded child.
        let mut tree = Self {
            pids: vec![root.id()],
            root,
            _tempdir: tempdir,
            verified_gone: false,
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let descendants = loop {
            if let (Some(child), Some(grandchild)) = (
                try_read_windows_pid(&child_pidfile),
                try_read_windows_pid(&grandchild_pidfile),
            ) && windows_pid_exists(child)
                && windows_pid_exists(grandchild)
            {
                break [child, grandchild];
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{label} did not publish a live child and grandchild within 10s"
            );
            sleep(Duration::from_millis(25));
        };

        tree.pids.extend(descendants);
        tree.assert_all_running(label);
        tree
    }

    pub(crate) fn root_pid(&self) -> u32 {
        self.pids[0]
    }

    pub(crate) fn pids(&self) -> [u32; 3] {
        [self.pids[0], self.pids[1], self.pids[2]]
    }

    pub(crate) fn assert_all_running(&self, phase: &str) {
        for (role, pid) in ["root", "child", "grandchild"]
            .into_iter()
            .zip(self.pids.iter().copied())
        {
            assert!(
                windows_pid_exists(pid),
                "{phase}: captured {role} PID {pid} is not running"
            );
        }
    }

    pub(crate) fn assert_all_gone(&mut self, phase: &str) {
        use std::thread::sleep;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let _ = self.root.try_wait();
            let live: Vec<_> = ["root", "child", "grandchild"]
                .into_iter()
                .zip(self.pids.iter().copied())
                .filter(|(_, pid)| windows_pid_exists(*pid))
                .collect();
            if live.is_empty() {
                self.verified_gone = true;
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{phase}: Windows process census still live after 5s: {live:?}"
            );
            sleep(Duration::from_millis(25));
        }
    }
}

#[cfg(all(test, windows))]
impl Drop for WindowsProcessTree {
    fn drop(&mut self) {
        if self.verified_gone {
            return;
        }

        // A failing assertion must not leave any known PowerShell descendant on
        // the shared runner. This cleanup is deliberately not part of the PASS
        // oracle: assert_all_gone disarms it only after the tested path succeeds.
        for pid in self.pids.iter().copied().rev() {
            terminate_process_tree(pid);
        }
        let _ = self.root.kill();
        let _ = self.root.wait();
    }
}

#[cfg(all(test, windows))]
fn try_read_windows_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(all(test, windows))]
pub(crate) fn windows_pid_exists(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let output = std::process::Command::new("tasklist.exe")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("tasklist must be available on a Windows runner");
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| line.contains(&format!("\"{pid}\"")))
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

    /// A wrapper can exit successfully while a background descendant keeps its
    /// inherited stdout/stderr pipes open. The command deadline owns that drain
    /// too, and the descendant must not survive a success-shaped root exit.
    #[cfg(unix)]
    #[tokio::test]
    async fn successful_root_exit_reaps_pipe_holding_descendants() {
        use std::io::Read;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("background.pid");
        let script = format!("sleep 30 & echo $! > {}", pidfile.display());
        let mut cmd = TokioCommand::new("sh");
        cmd.arg("-c").arg(script);

        let started = std::time::Instant::now();
        let output =
            run_capture_with_timeout(cmd, Duration::from_secs(2), "background-pipe-tree", || {
                anyhow::anyhow!("background pipe drain timed out")
            })
            .await
            .expect("successful root exit must close descendant-held pipes");
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(2));

        let mut pid = String::new();
        std::fs::File::open(&pidfile)
            .expect("shell records background pid before exit")
            .read_to_string(&mut pid)
            .expect("read background pid");
        assert_grandchild_reaped(pid.trim().parse().expect("background pid")).await;
    }

    /// Windows has no durable process-group id after the root is reaped. The
    /// Job Object must therefore remain the ownership handle and terminate a
    /// descendant that outlives a successful PowerShell wrapper.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_successful_root_exit_reaps_job_descendants() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let child_pidfile = tmp.path().join("background-child.pid");
        let child_script = tmp.path().join("background-child.ps1");
        let parent_script = tmp.path().join("successful-parent.ps1");

        std::fs::write(&child_script, "Start-Sleep -Seconds 60\n")
            .expect("write Windows child script");
        std::fs::write(
            &parent_script,
            format!(
                "$c = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','{}'\nSet-Content -LiteralPath '{}' -Value $c.Id\nexit 0\n",
                child_script.display(),
                child_pidfile.display(),
            ),
        )
        .expect("write Windows parent script");

        let mut cmd = TokioCommand::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&parent_script);
        let output = run_capture_with_timeout(
            cmd,
            Duration::from_secs(10),
            "successful-windows-parent",
            || anyhow::anyhow!("Windows root-exit cleanup timed out"),
        )
        .await
        .expect("successful Windows root exit must terminate its job descendants");
        assert!(output.status.success());

        let child_pid: u32 = std::fs::read_to_string(&child_pidfile)
            .expect("parent records child pid")
            .trim()
            .parse()
            .expect("numeric Windows child pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while super::windows_pid_exists(child_pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "Windows Job Object descendant {child_pid} survived root exit",
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_wait_governed_is_killed_when_the_run_cancels() {
        use std::sync::Arc;
        use std::time::Instant;

        let governor = Arc::new(crate::governor::ResourceGovernor::new());
        let canceller = Arc::clone(&governor);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            canceller.cancel();
        });
        let started = Instant::now();
        let status = crate::governor::with_run_scope(Arc::clone(&governor), async {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30");
            spawn_wait_governed(cmd, "sleep")
        })
        .await
        .expect("spawned sleep must be reaped, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancelled git-shaped std child must not wait out its command"
        );
        assert!(
            !status.success(),
            "a cancelled sleep must not exit 0, got {status:?}"
        );
        assert_eq!(governor.inflight_count(), 0);
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
        let mut tree = WindowsProcessTree::spawn("direct taskkill primitive");

        terminate_process_tree(tree.root_pid());

        tree.assert_all_gone("direct taskkill primitive");
    }

    #[cfg(windows)]
    #[test]
    fn taskkill_uses_an_absolute_system_path_and_exposes_spawn_failure() {
        let taskkill = system_taskkill_path();
        assert!(
            taskkill.is_absolute(),
            "taskkill path must not use PATH lookup"
        );
        assert_eq!(
            taskkill.file_name().and_then(|name| name.to_str()),
            Some("taskkill.exe")
        );

        let missing = std::path::Path::new(r"C:\prview-missing\taskkill.exe");
        assert!(
            taskkill_process_tree_at(missing, u32::MAX).is_err(),
            "spawn failure must remain observable to the caller"
        );
    }

    #[test]
    fn taskkill_nonzero_status_is_observable_through_production_handler() {
        #[cfg(unix)]
        let status = std::process::Command::new("sh")
            .args(["-c", "exit 23"])
            .status()
            .expect("controlled non-zero status");
        #[cfg(windows)]
        let status = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 23"])
            .status()
            .expect("controlled non-zero status");

        let taskkill = std::path::Path::new("controlled-taskkill.exe");
        let err = terminate_windows_process_tree_with(taskkill, 4242, |path, pid| {
            assert_eq!(path, taskkill);
            assert_eq!(pid, 4242);
            Ok(status)
        })
        .expect_err("non-zero taskkill status must remain observable");

        let message = err.to_string();
        assert!(message.contains("controlled-taskkill.exe"));
        assert!(message.contains("process tree 4242"));
        assert!(message.contains("status"));
    }
}
