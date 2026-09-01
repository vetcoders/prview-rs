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

const OWNED_DIRECT_ROOT_REAP_TIMEOUT: Duration = Duration::from_secs(2);

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
pub fn sigkill_process_group(pid: u32) -> bool {
    // SAFETY: plain kill(2) syscall against the process group created by
    // `harden[_std]`. ESRCH means the group is already gone; every other errno
    // (especially EPERM) means tree termination was not confirmed.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    unix_group_kill_succeeded(result, std::io::Error::last_os_error().raw_os_error())
}

#[cfg(unix)]
fn unix_group_kill_succeeded(result: i32, errno: Option<i32>) -> bool {
    result == 0 || (result == -1 && errno == Some(libc::ESRCH))
}

/// Terminate the full process tree led by `pid` on every supported platform.
///
/// Unix children lead their own process group, so one negative-pgid SIGKILL is
/// sufficient. Windows has no inherited Unix-style process group contract;
/// the built-in `taskkill /T /F` primitive walks and force-terminates the tree.
pub fn terminate_process_tree(pid: u32) -> bool {
    #[cfg(unix)]
    {
        sigkill_process_group(pid)
    }

    #[cfg(windows)]
    {
        let taskkill = system_taskkill_path();
        match terminate_windows_process_tree_with(&taskkill, pid, taskkill_process_tree_at) {
            Ok(()) => true,
            Err(err) => {
                eprintln!("prview: {err}");
                false
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("prview process-tree cancellation is unsupported on this platform");
        false
    }
}

/// Terminate a hardened Tokio child tree and reap its direct root within a
/// finite budget. Group/tree termination and direct-root reaping are separate
/// obligations: killing the group can leave the direct child as a zombie in a
/// long-lived MCP process unless `wait()` is still driven afterwards.
pub async fn terminate_and_reap_tokio_child(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
    reap_timeout: Duration,
) -> bool {
    let tree_terminated = pid.is_some_and(terminate_process_tree);
    let direct_kill_started = child.start_kill().is_ok();
    let root_reaped = matches!(
        tokio::time::timeout(reap_timeout, child.wait()).await,
        Ok(Ok(_))
    );
    (tree_terminated || direct_kill_started) && root_reaped
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

/// Poll only the direct process beneath the ownership wrapper.
///
/// On Windows, `JobObjectChild::try_wait` also consumes one completion-port
/// notification before it polls the root. If that notification is the final
/// job-completion signal, a later blocking `JobObjectChild::wait` can wait for
/// an event that was already consumed. The owner needs the wrapper intact for
/// tree termination, but root-exit detection must bypass it.
fn try_wait_direct_std_child(
    child: &mut dyn process_wrap::std::ChildWrapper,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    child.inner_mut().try_wait()
}

fn reap_direct_std_child_within(
    child: &mut dyn process_wrap::std::ChildWrapper,
    timeout: Duration,
) -> bool {
    let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
        return false;
    };
    loop {
        match try_wait_direct_std_child(child) {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => return false,
        }
    }
}

/// Terminate every process owned by a synchronous child wrapper, including the
/// descendants of a Windows root process that has already exited.
pub fn terminate_owned_std_child(child: &mut dyn process_wrap::std::ChildWrapper) -> bool {
    #[cfg(unix)]
    {
        terminate_process_tree(child.id())
    }

    #[cfg(windows)]
    {
        match child.start_kill() {
            Ok(()) => true,
            Err(job_error) => {
                let taskkill = system_taskkill_path();
                match terminate_windows_process_tree_with(
                    &taskkill,
                    child.id(),
                    taskkill_process_tree_at,
                ) {
                    Ok(()) => true,
                    Err(fallback_error) => {
                        eprintln!(
                            "prview: failed to terminate Windows Job Object ({job_error}); fallback also failed: {fallback_error}"
                        );
                        false
                    }
                }
            }
        }
    }
}

/// Terminate and reap a synchronous owned child without an unbounded wrapper
/// wait. Windows uses the Job Object only as the tree-termination handle, then
/// reaps the direct root through the raw child layer within a finite budget;
/// completion-port notifications are not treated as a durable completion
/// ledger.
pub fn terminate_and_reap_owned_std_child(child: &mut dyn process_wrap::std::ChildWrapper) -> bool {
    if terminate_owned_std_child(child) {
        #[cfg(windows)]
        {
            reap_direct_std_child_within(child, OWNED_DIRECT_ROOT_REAP_TIMEOUT)
        }
        #[cfg(not(windows))]
        {
            child.wait().is_ok()
        }
    } else {
        // Cancellation may have won through the governor milliseconds before
        // this owner reached its own cleanup path. A second group kill can then
        // report no live group before the direct root becomes waitable. Poll
        // for a short, finite interval so the owned root gets a bounded chance
        // to be reaped instead of being abandoned immediately. The false return
        // still preserves the distinction between a confirmed tree termination
        // and this best-effort direct-root reap; a root that exits only after
        // the deadline is not claimed as reaped.
        let _ = reap_direct_std_child_within(child, Duration::from_millis(250));
        false
    }
}

/// Run a synchronous command with captured output under the run governor.
///
/// Unlike `Command::output`, this registers the process tree before waiting and
/// observes run cancellation while the direct root is alive. Reader threads
/// drain both pipes concurrently so stderr cannot deadlock a command whose
/// stdout is also full.
pub fn output_governed(
    cmd: std::process::Command,
    label: &str,
) -> anyhow::Result<std::process::Output> {
    output_governed_inner(cmd, label, None, None)
}

/// Governed captured output with an absolute lifecycle deadline.
pub fn output_governed_with_timeout(
    cmd: std::process::Command,
    label: &str,
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    output_governed_inner(cmd, label, None, Some(timeout))
}

/// Captured governed output with a finite byte payload written to child stdin.
///
/// The writer runs beside both pipe readers, so a command that produces output
/// before consuming its input cannot deadlock the parent. Cancellation closes
/// the owned process tree and all three pipes before any thread is joined.
pub fn output_governed_with_input(
    cmd: std::process::Command,
    label: &str,
    input: &[u8],
) -> anyhow::Result<std::process::Output> {
    output_governed_inner(cmd, label, Some(input.to_vec()), None)
}

/// Governed captured output with stdin and one deadline for input, execution,
/// process-tree cleanup and output drain.
pub fn output_governed_with_input_timeout(
    cmd: std::process::Command,
    label: &str,
    input: &[u8],
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    output_governed_inner(cmd, label, Some(input.to_vec()), Some(timeout))
}

fn output_governed_inner(
    mut cmd: std::process::Command,
    label: &str,
    input: Option<Vec<u8>>,
    timeout: Option<Duration>,
) -> anyhow::Result<std::process::Output> {
    use std::io::{Read, Write};

    harden_std(&mut cmd);
    if input.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let governor = crate::governor::current_run_governor();
    if governor
        .as_ref()
        .is_some_and(|governor| governor.is_cancelled())
    {
        return Err(crate::governor::Cancelled.into());
    }

    let mut child =
        spawn_owned_std_child(cmd).map_err(|e| anyhow::anyhow!("failed to spawn {label}: {e}"))?;
    let mut registration = crate::governor::register_run_child(child.id(), label);
    if governor.is_some() && registration.is_none() {
        terminate_and_reap_owned_std_child(child.as_mut());
        return Err(crate::governor::Cancelled.into());
    }
    let pipes = (child.stdout().take(), child.stderr().take());
    let (Some(stdout), Some(stderr)) = pipes else {
        terminate_and_reap_owned_std_child(child.as_mut());
        drop(registration.take());
        return Err(anyhow::anyhow!(
            "failed to capture both stdout and stderr for {label}"
        ));
    };
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut pipe = stdout;
        pipe.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut pipe = stderr;
        pipe.read_to_end(&mut bytes).map(|_| bytes)
    });
    let mut stdin_writer = match input {
        Some(input) => {
            let Some(mut stdin) = child.stdin().take() else {
                terminate_and_reap_owned_std_child(child.as_mut());
                drop(registration.take());
                return Err(anyhow::anyhow!("failed to pipe stdin for {label}"));
            };
            Some(std::thread::spawn(move || stdin.write_all(&input)))
        }
        None => None,
    };

    let deadline = timeout.and_then(|timeout| std::time::Instant::now().checked_add(timeout));
    enum StopReason {
        Completed(std::process::ExitStatus),
        Cancelled,
        TimedOut,
    }
    let reason = loop {
        if governor
            .as_ref()
            .is_some_and(|governor| governor.is_cancelled())
        {
            if terminate_and_reap_owned_std_child(child.as_mut()) {
                drop(registration.take());
                // Successful tree termination closes the inherited pipes, so
                // the reader threads below can be joined deterministically.
                break StopReason::Cancelled;
            }
            // Both Windows termination mechanisms failed. Do not turn that
            // exceptional cleanup failure into an unbounded join/wait; dropping
            // JoinHandle detaches the readers and preserves prompt exit 130.
            drop(stdout_reader);
            drop(stderr_reader);
            drop(stdin_writer.take());
            drop(registration.take());
            return Err(crate::governor::Cancelled.into());
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            if terminate_and_reap_owned_std_child(child.as_mut()) {
                drop(registration.take());
                break StopReason::TimedOut;
            }
            drop(stdout_reader);
            drop(stderr_reader);
            drop(stdin_writer.take());
            drop(registration.take());
            return Err(anyhow::anyhow!(
                "{label} timed out and its process tree could not be terminated"
            ));
        }
        match try_wait_direct_std_child(child.as_mut()) {
            Ok(Some(status)) => {
                // The direct wrapper can exit before a descendant. Tear down
                // the still-owned group/job before joining pipe readers.
                if !terminate_owned_std_child(child.as_mut()) {
                    drop(stdout_reader);
                    drop(stderr_reader);
                    drop(stdin_writer.take());
                    drop(registration.take());
                    return Err(anyhow::anyhow!(
                        "failed to terminate descendants of completed {label}"
                    ));
                }
                if !reap_direct_std_child_within(child.as_mut(), OWNED_DIRECT_ROOT_REAP_TIMEOUT) {
                    drop(stdout_reader);
                    drop(stderr_reader);
                    drop(stdin_writer.take());
                    drop(registration.take());
                    return Err(anyhow::anyhow!(
                        "failed to reap direct root of completed {label}"
                    ));
                }
                drop(registration.take());
                break StopReason::Completed(status);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                terminate_and_reap_owned_std_child(child.as_mut());
                drop(registration.take());
                return Err(anyhow::anyhow!("failed to wait for {label}: {error}"));
            }
        }
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stderr reader panicked"))??;
    if let Some(stdin_writer) = stdin_writer {
        stdin_writer
            .join()
            .map_err(|_| anyhow::anyhow!("{label} stdin writer panicked"))??;
    }
    let status = match reason {
        StopReason::Completed(status) => status,
        StopReason::Cancelled => return Err(crate::governor::Cancelled.into()),
        StopReason::TimedOut => return Err(anyhow::anyhow!("{label} timed out")),
    };
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn a synchronous child, register it with the current run governor if any,
/// and wait. A cancelled run kills the process group instead of waiting out a
/// `git fetch` or `git archive | tar`.
pub fn spawn_wait_governed(
    mut cmd: std::process::Command,
    label: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    harden_std(&mut cmd);
    let governor = crate::governor::current_run_governor();
    if governor
        .as_ref()
        .is_some_and(|governor| governor.is_cancelled())
    {
        return Err(crate::governor::Cancelled.into());
    }
    let mut child =
        spawn_owned_std_child(cmd).map_err(|e| anyhow::anyhow!("failed to spawn {label}: {e}"))?;
    let registration = crate::governor::register_run_child(child.id(), label);
    if governor.is_some() && registration.is_none() {
        // Cancellation won the spawn/register race. `register_child` attempted
        // PID-tree cleanup; the owned wrapper is the authoritative Windows
        // fallback when that direct root already exited.
        terminate_and_reap_owned_std_child(child.as_mut());
        return Err(crate::governor::Cancelled.into());
    }
    let status = loop {
        if governor
            .as_ref()
            .is_some_and(|governor| governor.is_cancelled())
        {
            terminate_and_reap_owned_std_child(child.as_mut());
            return Err(crate::governor::Cancelled.into());
        }
        match try_wait_direct_std_child(child.as_mut()) {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                terminate_and_reap_owned_std_child(child.as_mut());
                return Err(anyhow::anyhow!("failed to wait for {label}: {error}"));
            }
        }
    };
    // Polling the direct root above lets Windows terminate the still-owned Job
    // Object without relying on its completion-port wait; Unix applies the
    // equivalent process-group cleanup.
    if !terminate_and_reap_owned_std_child(child.as_mut()) {
        return Err(anyhow::anyhow!(
            "failed to terminate descendants of completed {label}"
        ));
    }
    Ok(status)
}

/// Run a synchronous producer → consumer pipeline under the same owned-process
/// contract as individual governed commands. Both direct roots are polled so a
/// Windows Job Object can be terminated when a wrapper exits before one of its
/// descendants; waiting on the job itself at that point would hang forever.
pub fn run_pipeline_governed(
    mut producer_cmd: std::process::Command,
    mut consumer_cmd: std::process::Command,
    producer_label: &str,
    consumer_label: &str,
) -> anyhow::Result<(std::process::ExitStatus, std::process::ExitStatus)> {
    harden_std(&mut producer_cmd);
    harden_std(&mut consumer_cmd);
    let governor = crate::governor::current_run_governor();
    if governor
        .as_ref()
        .is_some_and(|governor| governor.is_cancelled())
    {
        return Err(crate::governor::Cancelled.into());
    }

    producer_cmd.stdout(std::process::Stdio::piped());
    let mut producer = spawn_owned_std_child(producer_cmd)
        .map_err(|error| anyhow::anyhow!("failed to spawn {producer_label}: {error}"))?;
    let mut producer_registration =
        crate::governor::register_run_child(producer.id(), producer_label);
    if governor.is_some() && producer_registration.is_none() {
        terminate_and_reap_owned_std_child(producer.as_mut());
        return Err(crate::governor::Cancelled.into());
    }
    let producer_stdout = match producer.stdout().take() {
        Some(stdout) => stdout,
        None => {
            terminate_and_reap_owned_std_child(producer.as_mut());
            return Err(anyhow::anyhow!("failed to capture {producer_label} stdout"));
        }
    };

    consumer_cmd.stdin(producer_stdout);
    let mut consumer = match spawn_owned_std_child(consumer_cmd) {
        Ok(consumer) => consumer,
        Err(error) => {
            terminate_and_reap_owned_std_child(producer.as_mut());
            return Err(anyhow::anyhow!("failed to spawn {consumer_label}: {error}"));
        }
    };
    let mut consumer_registration =
        crate::governor::register_run_child(consumer.id(), consumer_label);
    if governor.is_some() && consumer_registration.is_none() {
        terminate_and_reap_owned_std_child(producer.as_mut());
        terminate_and_reap_owned_std_child(consumer.as_mut());
        return Err(crate::governor::Cancelled.into());
    }
    let mut producer_status = None;
    let mut consumer_status = None;

    while producer_status.is_none() || consumer_status.is_none() {
        if governor
            .as_ref()
            .is_some_and(|governor| governor.is_cancelled())
        {
            terminate_and_reap_owned_std_child(producer.as_mut());
            terminate_and_reap_owned_std_child(consumer.as_mut());
            return Err(crate::governor::Cancelled.into());
        }

        if producer_status.is_none() {
            match try_wait_direct_std_child(producer.as_mut()) {
                Ok(Some(status)) => {
                    if !terminate_and_reap_owned_std_child(producer.as_mut()) {
                        terminate_and_reap_owned_std_child(consumer.as_mut());
                        return Err(anyhow::anyhow!(
                            "failed to terminate descendants of completed {producer_label}"
                        ));
                    }
                    drop(producer_registration.take());
                    producer_status = Some(status);
                }
                Ok(None) => {}
                Err(error) => {
                    terminate_and_reap_owned_std_child(producer.as_mut());
                    terminate_and_reap_owned_std_child(consumer.as_mut());
                    return Err(anyhow::anyhow!(
                        "failed to wait for {producer_label}: {error}"
                    ));
                }
            }
        }

        if consumer_status.is_none() {
            match try_wait_direct_std_child(consumer.as_mut()) {
                Ok(Some(status)) => {
                    if !terminate_and_reap_owned_std_child(consumer.as_mut()) {
                        terminate_and_reap_owned_std_child(producer.as_mut());
                        return Err(anyhow::anyhow!(
                            "failed to terminate descendants of completed {consumer_label}"
                        ));
                    }
                    drop(consumer_registration.take());
                    consumer_status = Some(status);
                }
                Ok(None) => {}
                Err(error) => {
                    terminate_and_reap_owned_std_child(producer.as_mut());
                    terminate_and_reap_owned_std_child(consumer.as_mut());
                    return Err(anyhow::anyhow!(
                        "failed to wait for {consumer_label}: {error}"
                    ));
                }
            }
        }

        if producer_status.is_none() || consumer_status.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    Ok((producer_status.unwrap(), consumer_status.unwrap()))
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
        // Keep the Windows Job Object completion stream untouched until the
        // owner terminates and reaps the complete tree.
        if let Some(status) = child.inner_mut().try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(windows)]
async fn reap_direct_tokio_child_within(
    child: &mut dyn process_wrap::tokio::ChildWrapper,
    timeout: Duration,
) -> bool {
    matches!(
        tokio::time::timeout(timeout, child.inner_mut().wait()).await,
        Ok(Ok(_))
    )
}

async fn terminate_owned_tokio_child(
    child: &mut dyn process_wrap::tokio::ChildWrapper,
    pid: Option<u32>,
) -> bool {
    #[cfg(unix)]
    let group_terminated = pid.is_some_and(sigkill_process_group);

    // On Windows JobObjectChild::start_kill terminates the job even when the
    // direct root has already exited. If that owned primitive fails while the
    // root is still live, taskkill remains a best-effort fallback. On Unix the
    // process-group signal above owns descendants; start_kill is still useful
    // as direct-child fallback.
    #[cfg(windows)]
    let terminated = match child.start_kill() {
        Ok(()) => true,
        Err(job_error) => match pid {
            Some(pid) => {
                let taskkill = system_taskkill_path();
                match terminate_windows_process_tree_with(&taskkill, pid, taskkill_process_tree_at)
                {
                    Ok(()) => true,
                    Err(fallback_error) => {
                        eprintln!(
                            "prview: failed to terminate Windows Job Object ({job_error}); fallback also failed: {fallback_error}"
                        );
                        false
                    }
                }
            }
            None => {
                eprintln!("prview: failed to terminate Windows Job Object: {job_error}");
                false
            }
        },
    };
    #[cfg(windows)]
    return terminated
        && reap_direct_tokio_child_within(child, OWNED_DIRECT_ROOT_REAP_TIMEOUT).await;
    #[cfg(not(windows))]
    {
        let direct_terminated = child.start_kill().is_ok();
        let _ = child.wait().await;
        if pid.is_some() {
            group_terminated
        } else {
            direct_terminated
        }
    }
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

    // Capture the scope separately from the registration result. `None` means
    // either "not governed" or "registration refused after cancellation";
    // only the latter must terminate and return typed `Cancelled` immediately.
    let scoped_governor = crate::governor::current_child_governor();

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

    if scoped_governor.is_some() && registration.is_none() {
        let _ = terminate_owned_tokio_child(child.as_mut(), pid).await;
        stdout_task.abort();
        stderr_task.abort();
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        return Err(crate::governor::Cancelled.into());
    }

    // `JobObjectChild::wait` deliberately waits for every descendant. Poll the
    // direct root instead, so root-exits-first is observed immediately and the
    // owned job can be terminated rather than consuming the command timeout.
    match tokio::time::timeout_at(deadline, wait_for_direct_child_exit(child.as_mut())).await {
        Ok(Ok(status)) => {
            if !terminate_owned_tokio_child(child.as_mut(), pid).await {
                drop(registration);
                stdout_task.abort();
                stderr_task.abort();
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(anyhow::anyhow!(
                    "failed to terminate descendants of completed {label}"
                ));
            }
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
            let tree_reaped = terminate_owned_tokio_child(child.as_mut(), pid).await;
            drop(registration);
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            if !tree_reaped {
                return Err(anyhow::anyhow!(
                    "failed to run {label}: {e}; owned process tree could not be terminated"
                ));
            }
            Err(anyhow::anyhow!("failed to run {label}: {e}"))
        }
        Err(_) => {
            let tree_reaped = terminate_owned_tokio_child(child.as_mut(), pid).await;
            drop(registration);
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let timeout_error = on_timeout();
            if tree_reaped {
                Err(timeout_error)
            } else {
                Err(timeout_error.context(format!(
                    "{label} timed out and its owned process tree could not be terminated"
                )))
            }
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

/// Read a test-owned Unix pidfile only after the producer has completed the
/// write. `echo > file` creates an empty file before writing its digits, so
/// existence alone is a racy readiness oracle under concurrent test load.
#[cfg(all(test, unix))]
pub(crate) fn read_published_unix_pid(path: &std::path::Path) -> Option<i32> {
    read_published_unix_pids(path, 1)?.into_iter().next()
}

/// Require the producer's newline terminator, an exact token count, and two
/// identical reads. A parseable prefix such as `"987 3"` must not be mistaken
/// for a completed `"987 32145\n"` while the shell is still writing it.
#[cfg(all(test, unix))]
pub(crate) fn read_published_unix_pids(
    path: &std::path::Path,
    expected: usize,
) -> Option<Vec<i32>> {
    let contents = std::fs::read_to_string(path).ok()?;
    if !contents.ends_with('\n') {
        return None;
    }
    let tokens = contents.split_whitespace().collect::<Vec<_>>();
    if tokens.len() != expected {
        return None;
    }
    let pids = tokens
        .into_iter()
        .map(str::parse::<i32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (std::fs::read_to_string(path).ok()? == contents).then_some(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn published_unix_pid_reader_rejects_partial_or_extra_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("tree.pids");

        std::fs::write(&pidfile, "987 3").expect("partial pid payload");
        assert_eq!(read_published_unix_pids(&pidfile, 2), None);

        std::fs::write(&pidfile, "987 32145\n").expect("complete pid payload");
        assert_eq!(
            read_published_unix_pids(&pidfile, 2),
            Some(vec![987, 32145])
        );

        std::fs::write(&pidfile, "987 32145 6\n").expect("extra pid payload");
        assert_eq!(read_published_unix_pids(&pidfile, 2), None);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn governed_sync_output_writes_stdin_without_deadlock() {
        #[cfg(unix)]
        let command = std::process::Command::new("cat");
        #[cfg(windows)]
        let command = {
            let mut command = std::process::Command::new("findstr.exe");
            command.arg("^");
            command
        };
        let output = output_governed_with_input_timeout(
            command,
            "sync stdin echo",
            b"payload\n",
            Duration::from_secs(2),
        )
        .expect("the platform stdin echo should consume governed stdin");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim_end(),
            "payload"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_tokio_tree_termination_reaps_each_direct_root() {
        for iteration in 0..2 {
            let tmp = tempfile::tempdir().expect("tempdir");
            let pidfile = tmp.path().join(format!("mcp-timeout-{iteration}.pids"));
            let mut command = TokioCommand::new("sh");
            command.args([
                "-c",
                &format!(
                    "sleep 30 & printf '%s %s\\n' \"$$\" \"$!\" > '{}'; wait",
                    pidfile.display()
                ),
            ]);
            harden(&mut command);
            let mut child = command.spawn().expect("spawn hardened timeout tree");
            let root_pid = child.id();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while read_published_unix_pids(&pidfile, 2).is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timeout fixture never published its process tree"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            assert!(
                terminate_and_reap_tokio_child(&mut child, root_pid, Duration::from_secs(2)).await,
                "timeout cleanup must terminate the tree and reap its root"
            );
            let recorded = std::fs::read_to_string(&pidfile).unwrap();
            for pid in recorded
                .split_whitespace()
                .map(|pid| pid.parse::<i32>().unwrap())
            {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    // SAFETY: signal 0 only probes a PID created by this fixture.
                    if unsafe { libc::kill(pid, 0) } == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                    {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "MCP-style timeout process {pid} survived cleanup"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn governed_sync_output_deadline_reaps_process_group() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("governed-sync-grandchild.pid");
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            &format!("sleep 30 & echo $! > {} ; wait", pidfile.display()),
        ]);
        let error =
            output_governed_with_timeout(command, "sync timeout tree", Duration::from_millis(500))
                .expect_err("long-lived tree must hit the sync deadline");
        assert!(error.to_string().contains("timed out"));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let grandchild = loop {
            if let Some(pid) = read_published_unix_pid(&pidfile) {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shell never published its grandchild pid"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        loop {
            // SAFETY: signal 0 only probes the test-owned pid recorded above.
            let exists = unsafe { libc::kill(grandchild, 0) } == 0;
            if !exists && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild {grandchild} survived the governed sync timeout"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, Ordering};

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());
        let published_pid = Arc::new(AtomicI32::new(0));
        let captured_pid = Arc::clone(&published_pid);
        let captured_pidfile = pidfile.clone();

        let mut cmd = TokioCommand::new("sh");
        cmd.arg("-c").arg(&script);
        let err = run_capture_with_timeout_after_spawn(
            cmd,
            Duration::from_secs(3),
            "sh-tree",
            || anyhow::anyhow!("sh-tree timed out"),
            move |_| {
                for _ in 0..200 {
                    if let Some(pid) = read_published_unix_pid(&captured_pidfile) {
                        captured_pid.store(pid, Ordering::Release);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
        )
        .await
        .expect_err("sh tree with a 3s budget must time out");
        assert!(err.to_string().contains("sh-tree timed out"));

        let grandchild = published_pid.load(Ordering::Acquire);
        assert_ne!(grandchild, 0, "sh should publish a complete grandchild pid");

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
                    if super::read_published_unix_pid(&marker).is_some() {
                        break;
                    }
                    sleep(Duration::from_millis(10));
                }
                assert!(
                    super::read_published_unix_pid(&marker).is_some(),
                    "child must publish its complete grandchild pid"
                );
                canceller.cancel();
            },
        );

        let error =
            crate::governor::with_child_scope(Arc::clone(&governor), "late-async-tree", run)
                .await
                .expect_err("late registration must return typed cancellation");

        let grandchild =
            super::read_published_unix_pid(&pidfile).expect("complete numeric grandchild pid");
        assert_grandchild_reaped(grandchild).await;
        assert_eq!(governor.inflight_count(), 0);
        assert!(
            crate::governor::is_cancellation(&error),
            "the refused late registration must preserve cancellation identity: {error:#}",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sync_successful_root_exit_reaps_process_group_descendants() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("sync-background.pid");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", &format!("sleep 30 & echo $! > {}", pidfile.display())]);

        let started = std::time::Instant::now();
        let status = spawn_wait_governed(cmd, "sync-background-tree")
            .expect("successful root exit must not wait for its descendant");
        assert!(status.success());
        assert!(started.elapsed() < Duration::from_secs(2));

        let descendant: i32 = std::fs::read_to_string(&pidfile)
            .expect("root records descendant pid")
            .trim()
            .parse()
            .expect("numeric descendant pid");
        assert_grandchild_reaped(descendant).await;
    }

    #[cfg(windows)]
    #[test]
    fn windows_sync_root_exit_reaps_job_descendants() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let child_pidfile = tmp.path().join("sync-background-child.pid");
        let child_script = tmp.path().join("sync-background-child.ps1");
        let parent_script = tmp.path().join("sync-successful-parent.ps1");
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

        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&parent_script);
        let status = spawn_wait_governed(cmd, "sync-successful-windows-parent")
            .expect("sync root exit must terminate its job descendants");
        assert!(status.success());

        let child_pid: u32 = std::fs::read_to_string(&child_pidfile)
            .expect("parent records child pid")
            .trim()
            .parse()
            .expect("numeric Windows child pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while super::windows_pid_exists(child_pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "Windows Job Object descendant {child_pid} survived sync root exit",
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn governed_pipeline_reaps_a_root_exits_first_producer_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("pipeline-background.pid");
        let mut producer = std::process::Command::new("sh");
        producer.args([
            "-c",
            &format!("sleep 30 & echo $! > {}; printf payload", pidfile.display()),
        ]);
        let mut consumer = std::process::Command::new("cat");
        consumer.stdout(std::process::Stdio::null());

        let (producer_status, consumer_status) =
            run_pipeline_governed(producer, consumer, "producer", "consumer")
                .expect("pipeline roots finish without descendant hang");
        assert!(producer_status.success());
        assert!(consumer_status.success());
        let descendant: i32 = std::fs::read_to_string(&pidfile)
            .expect("producer records descendant pid")
            .trim()
            .parse()
            .expect("numeric descendant pid");
        assert_grandchild_reaped(descendant).await;
    }

    #[cfg(windows)]
    #[test]
    fn windows_governed_pipeline_reaps_root_exits_first_descendants() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let child_pidfile = tmp.path().join("pipeline-background-child.pid");
        let child_script = tmp.path().join("pipeline-background-child.ps1");
        let producer_script = tmp.path().join("pipeline-producer.ps1");
        std::fs::write(&child_script, "Start-Sleep -Seconds 60\n")
            .expect("write Windows child script");
        std::fs::write(
            &producer_script,
            format!(
                "$c = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','{}'\nSet-Content -LiteralPath '{}' -Value $c.Id\nWrite-Output payload\nexit 0\n",
                child_script.display(),
                child_pidfile.display(),
            ),
        )
        .expect("write Windows producer script");

        let mut producer = std::process::Command::new("powershell.exe");
        producer
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&producer_script);
        let mut consumer = std::process::Command::new("powershell.exe");
        consumer
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$input | Out-Null",
            ])
            .stdout(std::process::Stdio::null());

        let (producer_status, consumer_status) =
            run_pipeline_governed(producer, consumer, "producer", "consumer")
                .expect("Windows pipeline roots finish without descendant hang");
        assert!(producer_status.success());
        assert!(consumer_status.success());
        let child_pid: u32 = std::fs::read_to_string(&child_pidfile)
            .expect("producer records child pid")
            .trim()
            .parse()
            .expect("numeric Windows child pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while super::windows_pid_exists(child_pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "Windows pipeline descendant {child_pid} survived producer root exit",
            );
            std::thread::sleep(Duration::from_millis(25));
        }
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
        let error = crate::governor::with_run_scope(Arc::clone(&governor), async {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30");
            spawn_wait_governed(cmd, "sleep")
        })
        .await
        .expect_err("cancelled governed child returns typed cancellation");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancelled git-shaped std child must not wait out its command"
        );
        assert!(crate::governor::is_cancellation(&error), "{error:#}");
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

        assert!(sigkill_process_group(leader));
        let _ = child.wait();

        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 is a read-only existence/permission probe, and
            // `grandchild` came from the process tree created by this test.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) {
                    gone = true;
                    break;
                }
            }
            sleep(Duration::from_millis(20));
        }
        assert!(gone, "grandchild {grandchild} survived the group kill");
    }

    #[cfg(unix)]
    #[test]
    fn unix_group_kill_only_confirms_success_or_absence() {
        assert!(unix_group_kill_succeeded(0, None));
        assert!(unix_group_kill_succeeded(-1, Some(libc::ESRCH)));
        assert!(!unix_group_kill_succeeded(-1, Some(libc::EPERM)));
        assert!(!unix_group_kill_succeeded(-1, Some(libc::EINVAL)));
    }

    /// Poll until `grandchild` no longer exists. EPERM is not absence: treating
    /// a permission failure as reaped would let a live orphan satisfy the test.
    #[cfg(unix)]
    async fn assert_grandchild_reaped(grandchild: i32) {
        let mut gone = false;
        for _ in 0..60 {
            // SAFETY: signal 0 is a read-only existence/permission probe, and
            // `grandchild` came from the process tree created by this test.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) {
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
