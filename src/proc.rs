//! Shared child-process safety rails.
//!
//! Every external tool prview spawns must:
//! 1. detach stdin, so it can never sit on an interactive prompt inherited from
//!    the operator's terminal (npm's "Ok to proceed?" — the `--deep` hang class),
//! 2. run under `kill_on_drop`, so a dropped wait-future reaps the direct child,
//! 3. on unix lead its own process group (`process_group(0)`), so one SIGKILL to
//!    `-pgid` reaches the normal inherited tree (cargo → rustc → cc, npx → node
//!    → tool); cancellation/timeout additionally freezes and inventories live
//!    PPID descendants that moved into their own group via `setsid`/`setpgid`,
//!    because `kill_on_drop` alone leaves grandchildren,
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

#[cfg(unix)]
const EXTERNAL_CHILD_GROUP_TOKEN_ENV: &str = "PRVIEW_INTERNAL_CHILD_GROUP_TOKEN";
#[cfg(unix)]
const EXTERNAL_CHILD_GROUP_FD_ENV: &str = "PRVIEW_INTERNAL_CHILD_GROUP_FD";
#[cfg(unix)]
const EXTERNAL_CHILD_GROUP_HEADER: &str = "prview-child-groups-v1";

#[cfg(unix)]
struct ExternalChildGroupWriter {
    file: std::sync::Mutex<std::fs::File>,
}

#[cfg(unix)]
enum ExternalChildGroupWriterState {
    Disabled,
    Ready(ExternalChildGroupWriter),
    Broken(String),
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ExternalChildGroupIdentity {
    pid: u32,
    birth_identity: String,
}

pub(crate) enum ExternalChildGroupStart {
    NotMirrored,
    #[cfg(unix)]
    Mirrored(String),
    #[cfg(unix)]
    ExitedBeforeMirror,
}

impl ExternalChildGroupStart {
    pub(crate) fn into_mirrored_identity(self) -> Option<String> {
        match self {
            Self::NotMirrored => None,
            #[cfg(unix)]
            Self::Mirrored(identity) => Some(identity),
            #[cfg(unix)]
            Self::ExitedBeforeMirror => None,
        }
    }
}

#[cfg(unix)]
#[derive(Debug)]
enum ExternalChildBirthIdentity {
    Captured(String),
    ChildExited,
}

#[cfg(unix)]
fn classify_external_child_birth_identity(
    birth_identity: std::io::Result<String>,
    child_exited: impl FnOnce() -> std::io::Result<bool>,
) -> std::io::Result<ExternalChildBirthIdentity> {
    match birth_identity {
        Ok(identity) => Ok(ExternalChildBirthIdentity::Captured(identity)),
        Err(error) => match child_exited() {
            Ok(true) => Ok(ExternalChildBirthIdentity::ChildExited),
            Ok(false) | Err(_) => Err(error),
        },
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unix_child_exited_without_reaping(pid: u32) -> std::io::Result<bool> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info is writable siginfo storage. P_PID scopes the query to
        // the direct child just spawned by this process; WNOWAIT observes exit
        // without releasing that child/PID for reuse before registration.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: successful waitid initialized the supplied siginfo. POSIX
            // reports si_pid == 0 when WNOHANG found no waitable state.
            return Ok(unsafe { info.assume_init().si_pid() } != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn unix_child_exited_without_reaping(_pid: u32) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "non-reaping child-exit identity is unsupported on this platform",
    ))
}

#[cfg(unix)]
static EXTERNAL_CHILD_GROUP_WRITER: std::sync::OnceLock<ExternalChildGroupWriterState> =
    std::sync::OnceLock::new();
#[cfg(unix)]
static EXTERNAL_CHILD_GROUP_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(unix)]
static PROCESS_TABLE_CAPTURE_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(unix)]
const PROCESS_TABLE_MAX_BYTES: u64 = 16 * 1024 * 1024;
#[cfg(unix)]
static PROCESS_TABLE_REAPER_QUARANTINE: std::sync::OnceLock<
    std::sync::Mutex<Vec<std::process::Child>>,
> = std::sync::OnceLock::new();

#[cfg(unix)]
fn external_child_group_writer() -> std::io::Result<Option<&'static ExternalChildGroupWriter>> {
    use std::io::{Read as _, Seek as _};

    let state = EXTERNAL_CHILD_GROUP_WRITER.get_or_init(|| {
        let (token, ledger_fd) = match (
            std::env::var(EXTERNAL_CHILD_GROUP_TOKEN_ENV).ok(),
            std::env::var(EXTERNAL_CHILD_GROUP_FD_ENV).ok(),
        ) {
            (None, None) => return ExternalChildGroupWriterState::Disabled,
            (Some(token), Some(ledger_fd)) => (token, ledger_fd),
            _ => {
                return ExternalChildGroupWriterState::Broken(
                    "incomplete parent-owned child-group capability".to_string(),
                );
            }
        };
        let open = || -> std::io::Result<ExternalChildGroupWriter> {
            use std::os::fd::FromRawFd as _;

            let ledger_fd = ledger_fd.parse::<libc::c_int>().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid child-group ledger descriptor",
                )
            })?;
            validate_external_child_group_fd(ledger_fd)?;
            set_close_on_exec(ledger_fd)?;
            // SAFETY: the MCP parent transfers one inherited, locked ledger
            // descriptor to the quick-review root. This process is its sole
            // Rust owner after the parent closes its copy post-spawn.
            let mut file = unsafe { std::fs::File::from_raw_fd(ledger_fd) };
            let expected = format!("{EXTERNAL_CHILD_GROUP_HEADER} {token}\n");
            file.seek(std::io::SeekFrom::Start(0))?;
            let mut header = vec![0_u8; expected.len()];
            file.read_exact(&mut header)?;
            if header != expected.as_bytes() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "parent-owned child-group ledger header mismatch",
                ));
            }
            Ok(ExternalChildGroupWriter {
                file: std::sync::Mutex::new(file),
            })
        };
        match open() {
            Ok(writer) => ExternalChildGroupWriterState::Ready(writer),
            Err(error) => ExternalChildGroupWriterState::Broken(error.to_string()),
        }
    });
    match state {
        ExternalChildGroupWriterState::Disabled => Ok(None),
        ExternalChildGroupWriterState::Ready(writer) => Ok(Some(writer)),
        ExternalChildGroupWriterState::Broken(error) => Err(std::io::Error::other(error.clone())),
    }
}

/// Validate and adopt an MCP parent's child-group capability before this
/// process can launch any startup helper. The inherited descriptor is made
/// close-on-exec by `external_child_group_writer`, so later un-hardened probes
/// cannot accidentally extend the parent's spawn barrier.
pub(crate) fn initialize_external_child_group_capability() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let _ = external_child_group_writer()?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_close_on_exec(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fcntl reads and updates descriptor flags for an owned, non-negative
    // descriptor. No pointer or borrowed memory crosses the syscall boundary.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor and integer-only flag update as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn validate_external_child_group_fd(fd: libc::c_int) -> std::io::Result<()> {
    if fd <= libc::STDERR_FILENO {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "child-group ledger aliases a standard descriptor",
        ));
    }
    // SAFETY: `stat` is valid writable storage and fstat only inspects `fd`.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "child-group ledger descriptor is not a regular file",
        ));
    }
    // SAFETY: integer-only descriptor flag query for the validated pipe.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let access_mode = flags & libc::O_ACCMODE;
    if !matches!(access_mode, libc::O_WRONLY | libc::O_RDWR) || flags & libc::O_APPEND == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "child-group ledger descriptor is not appendable",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn clear_close_on_exec(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fcntl reads and updates descriptor flags for an owned descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: integer-only descriptor flag update for the same descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn lock_external_child_group_ledger(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: flock operates on the owned regular-file descriptor only.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn try_lock_external_child_group_ledger(fd: libc::c_int) -> std::io::Result<bool> {
    // SAFETY: same non-blocking advisory-lock operation as above.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    ) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn external_child_group_pre_exec_fd() -> std::io::Result<Option<libc::c_int>> {
    use std::os::fd::AsRawFd as _;

    Ok(external_child_group_writer()?.map(|writer| {
        writer
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_raw_fd()
    }))
}

/// Async-signal-safe provisional registration for the forked child. It runs
/// after the child has entered its dedicated process group and before `exec`.
#[cfg(unix)]
fn write_external_child_group_provisional(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: setpgid(0, 0) moves only this pre-exec child into the group whose
    // id is its own pid. Repeating CommandExt::process_group(0) is idempotent and
    // makes the registration ordering explicit rather than stdlib-dependent.
    if unsafe { libc::setpgid(0, 0) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // A decimal u32 plus the event byte and newline fits comfortably.
    let mut row = [0_u8; 16];
    row[0] = b'?';
    // SAFETY: getpid has no arguments or memory effects visible to Rust.
    let pid = unsafe { libc::getpid() as u32 };
    let mut digits = [0_u8; 10];
    let mut value = pid;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..count {
        row[1 + index] = digits[count - index - 1];
    }
    row[1 + count] = b'\n';
    let length = count + 2;
    // SAFETY: `row[..length]` is initialized stack memory and `fd` is the
    // inherited O_APPEND ledger descriptor. One write keeps the provisional
    // record contiguous with concurrent registrations.
    let written = unsafe { libc::write(fd, row.as_ptr().cast(), length) };
    if written == length as isize {
        Ok(())
    } else if written == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::from_raw_os_error(libc::EIO))
    }
}

#[cfg(unix)]
fn report_external_child_group(
    writer: &ExternalChildGroupWriter,
    event: char,
    pid: u32,
    birth_identity: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = writer
        .file
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let row = format!("{event}{pid}\t{birth_identity}\n");
    file.write_all(row.as_bytes())
}

pub(crate) fn report_external_child_group_started(
    pid: u32,
) -> std::io::Result<ExternalChildGroupStart> {
    #[cfg(unix)]
    {
        let Some(writer) = external_child_group_writer()? else {
            return Ok(ExternalChildGroupStart::NotMirrored);
        };
        let birth_identity = match classify_external_child_birth_identity(
            crate::storage::process_birth_identity(pid),
            || unix_child_exited_without_reaping(pid),
        )? {
            ExternalChildBirthIdentity::Captured(identity) => identity,
            ExternalChildBirthIdentity::ChildExited => {
                // The owning process has observed this direct child exit without
                // reaping it, so its PID/PGID cannot be reused. Registration
                // must use that narrow window to terminate any surviving group
                // members; afterwards the pre-exec provisional row can settle
                // without ever becoming signal authority in the MCP parent.
                return Ok(ExternalChildGroupStart::ExitedBeforeMirror);
            }
        };
        report_external_child_group(writer, '+', pid, &birth_identity)?;
        Ok(ExternalChildGroupStart::Mirrored(birth_identity))
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(ExternalChildGroupStart::NotMirrored)
    }
}

pub(crate) fn report_external_child_group_finished(pid: u32, birth_identity: &str) {
    #[cfg(unix)]
    {
        let result = external_child_group_writer().and_then(|writer| {
            let Some(writer) = writer else {
                return Ok(());
            };
            report_external_child_group(writer, '-', pid, birth_identity)
        });
        if let Err(error) = result {
            eprintln!("prview: failed to update the parent-owned child-group registry: {error}");
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (pid, birth_identity);
    }
}

/// Parent-side mirror of the separately-grouped tools owned by an MCP review.
/// Windows already has recursive `taskkill /T`; Unix needs this explicit
/// registry because a process group does not contain another process group.
pub(crate) struct ExternalChildGroupTracker {
    root_pid: Option<u32>,
    #[cfg(unix)]
    path: std::path::PathBuf,
    #[cfg(unix)]
    reader: std::fs::File,
    #[cfg(unix)]
    pending: Vec<u8>,
    #[cfg(unix)]
    active: std::collections::BTreeMap<ExternalChildGroupIdentity, usize>,
    #[cfg(unix)]
    settled_by_parent: std::collections::BTreeMap<ExternalChildGroupIdentity, usize>,
    #[cfg(unix)]
    provisional: std::collections::BTreeMap<u32, usize>,
    #[cfg(unix)]
    settled_provisional: std::collections::BTreeMap<u32, usize>,
    #[cfg(unix)]
    proven_terminated_groups: std::collections::BTreeSet<u32>,
    #[cfg(unix)]
    ledger_writer: Option<std::fs::File>,
}

impl ExternalChildGroupTracker {
    #[cfg(unix)]
    fn create(owner_dir: &std::path::Path) -> std::io::Result<(Self, String, libc::c_int)> {
        use std::io::{Seek as _, Write as _};
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let sequence = EXTERNAL_CHILD_GROUP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let token = format!("{}-{nonce}-{sequence}", std::process::id());
        let path = owner_dir.join(format!(".mcp-child-groups-{token}"));
        let header = format!("{EXTERNAL_CHILD_GROUP_HEADER} {token}\n");
        let mut writer = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        if let Err(error) = writer
            .write_all(header.as_bytes())
            .and_then(|()| writer.sync_data())
        {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        if let Err(error) = set_close_on_exec(writer.as_raw_fd())
            .and_then(|()| lock_external_child_group_ledger(writer.as_raw_fd()))
        {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }

        let mut reader = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
        };
        if let Err(error) = reader.seek(std::io::SeekFrom::Start(header.len() as u64)) {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        if let Err(error) = set_close_on_exec(reader.as_raw_fd()) {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let writer_fd = writer.as_raw_fd();
        Ok((
            Self {
                root_pid: None,
                path,
                reader,
                pending: Vec::new(),
                active: std::collections::BTreeMap::new(),
                settled_by_parent: std::collections::BTreeMap::new(),
                provisional: std::collections::BTreeMap::new(),
                settled_provisional: std::collections::BTreeMap::new(),
                proven_terminated_groups: std::collections::BTreeSet::new(),
                ledger_writer: Some(writer),
            },
            token,
            writer_fd,
        ))
    }

    pub(crate) fn attach(
        cmd: &mut TokioCommand,
        owner_dir: &std::path::Path,
    ) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let (tracker, token, writer_fd) = Self::create(owner_dir)?;
            cmd.env(EXTERNAL_CHILD_GROUP_TOKEN_ENV, &token)
                .env(EXTERNAL_CHILD_GROUP_FD_ENV, writer_fd.to_string());
            // Keep the descriptor CLOEXEC in the multi-threaded MCP parent.
            // Only the already-forked review root may clear the bit immediately
            // before its exec; unrelated concurrent spawns can never inherit it.
            // SAFETY: fcntl on one inherited integer descriptor is
            // async-signal-safe and the closure allocates no state.
            unsafe {
                cmd.pre_exec(move || clear_close_on_exec(writer_fd));
            }
            Ok(tracker)
        }

        #[cfg(not(unix))]
        {
            let _ = (cmd, owner_dir);
            Ok(Self { root_pid: None })
        }
    }

    pub(crate) fn attach_std(
        cmd: &mut std::process::Command,
        owner_dir: &std::path::Path,
    ) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;

            let (tracker, token, writer_fd) = Self::create(owner_dir)?;
            cmd.env(EXTERNAL_CHILD_GROUP_TOKEN_ENV, &token)
                .env(EXTERNAL_CHILD_GROUP_FD_ENV, writer_fd.to_string());
            // Same capability transfer as the Tokio adapter above. The parent
            // descriptor remains CLOEXEC; only this already-forked review root
            // clears it immediately before exec.
            unsafe {
                cmd.pre_exec(move || clear_close_on_exec(writer_fd));
            }
            Ok(tracker)
        }

        #[cfg(not(unix))]
        {
            let _ = (cmd, owner_dir);
            Ok(Self { root_pid: None })
        }
    }

    /// Release the MCP parent's copy of the locked ledger once the review root
    /// has inherited it. Later lock acquisition proves that the root and every
    /// fork caught in pre-exec have closed their copies.
    pub(crate) fn child_spawned(&mut self, root_pid: Option<u32>) {
        self.root_pid = root_pid;
        #[cfg(unix)]
        {
            drop(self.ledger_writer.take());
        }
    }

    async fn wait_for_spawn_barrier(&mut self, timeout: Duration) -> bool {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;

            drop(self.ledger_writer.take());
            let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
                return false;
            };
            loop {
                match try_lock_external_child_group_ledger(self.reader.as_raw_fd()) {
                    Ok(true) => return true,
                    Ok(false) => {
                        if std::time::Instant::now() >= deadline {
                            return false;
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = timeout;
            true
        }
    }

    #[cfg(unix)]
    fn wait_for_spawn_barrier_blocking(&mut self, timeout: Duration) -> bool {
        use std::os::fd::AsRawFd as _;

        drop(self.ledger_writer.take());
        let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
            return false;
        };
        loop {
            match try_lock_external_child_group_ledger(self.reader.as_raw_fd()) {
                Ok(true) => return true,
                Ok(false) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }

    pub(crate) async fn finalize_after_root_exit(&mut self, timeout: Duration) -> bool {
        let barrier_closed = self.wait_for_spawn_barrier(timeout).await;
        let groups_terminated = self.terminate_active_groups(true);
        barrier_closed && groups_terminated
    }

    /// Synchronous twin of [`Self::finalize_after_root_exit`] for the dedicated
    /// detached MCP reaper thread, which deliberately owns no Tokio runtime.
    pub(crate) fn finalize_after_root_exit_blocking(&mut self, timeout: Duration) -> bool {
        #[cfg(unix)]
        {
            let barrier_closed = self.wait_for_spawn_barrier_blocking(timeout);
            let groups_terminated = self.terminate_active_groups(true);
            barrier_closed && groups_terminated
        }

        #[cfg(not(unix))]
        {
            let _ = timeout;
            self.terminate_active_groups(true)
        }
    }

    pub(crate) fn root_reaped(&mut self) {
        self.root_pid = None;
    }

    #[cfg(unix)]
    fn drain(&mut self, final_read: bool) -> bool {
        use std::io::Read as _;

        let mut complete = true;
        let mut buffer = Vec::new();
        if self.reader.read_to_end(&mut buffer).is_err() {
            complete = false;
        }
        self.pending.extend_from_slice(&buffer);

        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            let Some((&event, identity)) = line.split_first() else {
                complete = false;
                continue;
            };
            let payload = identity.strip_suffix(b"\n").unwrap_or(identity);
            let Ok(text) = std::str::from_utf8(payload) else {
                complete = false;
                continue;
            };
            match event {
                b'?' => {
                    let Ok(pid) = text.parse::<u32>() else {
                        complete = false;
                        continue;
                    };
                    *self.provisional.entry(pid).or_default() += 1;
                }
                b'+' | b'-' => {
                    let Some((pid, birth_identity)) = text.split_once('\t') else {
                        complete = false;
                        continue;
                    };
                    let Ok(pid) = pid.parse::<u32>() else {
                        complete = false;
                        continue;
                    };
                    let identity = ExternalChildGroupIdentity {
                        pid,
                        birth_identity: birth_identity.to_string(),
                    };
                    if event == b'+' {
                        self.consume_provisional(pid);
                        *self.active.entry(identity).or_default() += 1;
                    } else if let Some(count) = self.active.get_mut(&identity) {
                        *count -= 1;
                        if *count == 0 {
                            let _ = self.active.remove(&identity);
                        }
                    } else if let Some(count) = self.settled_by_parent.get_mut(&identity) {
                        *count -= 1;
                        if *count == 0 {
                            let _ = self.settled_by_parent.remove(&identity);
                        }
                    } else {
                        complete = false;
                    }
                }
                _ => complete = false,
            }
        }
        complete && (!final_read || self.pending.is_empty())
    }

    pub(crate) fn terminate_active_groups(&mut self, final_read: bool) -> bool {
        #[cfg(unix)]
        {
            let mut complete = self.drain(final_read);
            let provisional: Vec<u32> = self.provisional.keys().copied().collect();
            for pid in provisional {
                if self.proven_terminated_groups.contains(&pid) || !unix_process_group_exists(pid) {
                    self.settle_provisional(pid);
                } else {
                    // A provisional row proves only what the forked child
                    // claimed before exec. Never signal a possibly recycled
                    // PGID unless the stopped-root descendant census proved it.
                    complete = false;
                }
            }
            let active: Vec<ExternalChildGroupIdentity> = self.active.keys().cloned().collect();
            for identity in active {
                let exact_leader = crate::storage::process_birth_identity_matches(
                    identity.pid,
                    &identity.birth_identity,
                );
                let leader_alive = crate::storage::is_process_alive(identity.pid);
                let group_exists = unix_process_group_exists(identity.pid);
                if exact_leader || (!leader_alive && group_exists) {
                    // If the leader is gone while its group remains, POSIX keeps
                    // that PGID attached to the original surviving members; it
                    // cannot name a newly-created group until the old one is
                    // empty. A live different-birth leader is the reuse case and
                    // deliberately does not enter this branch.
                    if terminate_hardened_process_tree(identity.pid) {
                        self.settle_identity(identity);
                    } else {
                        complete = false;
                    }
                } else if !leader_alive && !group_exists {
                    // The exact leader is gone and no member retains its group;
                    // ownership is complete without risking a recycled PID.
                    self.settle_identity(identity);
                } else {
                    // A group still exists but its leader no longer matches the
                    // recorded incarnation. Never signal a possibly reused
                    // target; report containment as unconfirmed instead.
                    complete = false;
                }
            }
            complete
        }

        #[cfg(not(unix))]
        {
            let _ = final_read;
            true
        }
    }

    #[cfg(unix)]
    fn settle_identity(&mut self, identity: ExternalChildGroupIdentity) {
        if let Some(count) = self.active.remove(&identity) {
            *self.settled_by_parent.entry(identity).or_default() += count;
        }
    }

    #[cfg(unix)]
    fn consume_provisional(&mut self, pid: u32) {
        if let Some(count) = self.provisional.get_mut(&pid) {
            *count -= 1;
            if *count == 0 {
                let _ = self.provisional.remove(&pid);
            }
        } else if let Some(count) = self.settled_provisional.get_mut(&pid) {
            *count -= 1;
            if *count == 0 {
                let _ = self.settled_provisional.remove(&pid);
            }
        }
    }

    #[cfg(unix)]
    fn settle_provisional(&mut self, pid: u32) {
        if let Some(count) = self.provisional.remove(&pid) {
            *self.settled_provisional.entry(pid).or_default() += count;
        }
    }

    #[cfg(unix)]
    fn terminate_proven_descendant_groups(&mut self, groups: &[u32]) -> bool {
        let mut complete = true;
        for &pgid in groups {
            if terminate_hardened_process_tree(pgid) {
                self.proven_terminated_groups.insert(pgid);
            } else {
                complete = false;
            }
        }
        complete
    }
}

impl Drop for ExternalChildGroupTracker {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let mut containment_confirmed = true;
            if let Some(pid) = self.root_pid.take() {
                let root_suspended = suspend_process_group(pid);
                let descendants_terminated = root_suspended
                    && match unix_stopped_descendant_process_groups(pid, Duration::from_secs(1)) {
                        Ok(groups) => self.terminate_proven_descendant_groups(&groups),
                        Err(error) => {
                            eprintln!(
                                "prview: could not prove MCP review descendant groups during cleanup: {error}"
                            );
                            false
                        }
                    };
                let registered_terminated = self.terminate_active_groups(false);
                let root_terminated = terminate_process_tree(pid);
                containment_confirmed &= root_suspended
                    && descendants_terminated
                    && registered_terminated
                    && root_terminated;
            }

            // Drop is the ownership fallback for an aborted MCP future. It may
            // block briefly, but it must not abandon a forked pre-exec writer
            // after an arbitrary scheduler-dependent 100 ms window.
            containment_confirmed &= self.finalize_after_root_exit_blocking(Duration::from_secs(5));
            if containment_confirmed {
                let _ = std::fs::remove_file(&self.path);
            } else {
                eprintln!(
                    "prview: MCP review cleanup remains unconfirmed; retained ownership sidecar {}",
                    self.path.display()
                );
            }
        }

        #[cfg(not(unix))]
        if let Some(pid) = self.root_pid.take() {
            let _ = terminate_process_tree(pid);
        }
    }
}

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
    sigkill_process_group_result(pid).is_ok()
}

#[cfg(unix)]
fn sigkill_process_group_result(pid: u32) -> std::io::Result<()> {
    // SAFETY: plain kill(2) syscall against the process group created by
    // `harden[_std]`. ESRCH means the group is already gone; every other errno
    // (especially EPERM) means tree termination was not confirmed.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if unix_group_kill_succeeded(result, std::io::Error::last_os_error().raw_os_error()) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Close a group whose direct leader is known waitable but remains unreaped.
///
/// A failed group signal is not itself evidence that a descendant survived.
/// In particular, macOS may reject a signal after the short-lived leader has
/// become a zombie even though no live member retains the PGID. The unreaped
/// leader prevents PGID reuse, so one bounded process-table snapshot can settle
/// that exact group safely. A live member or an unreadable census remains a
/// fail-closed error.
#[cfg(unix)]
pub(crate) fn close_exited_child_process_group(pid: u32) -> std::io::Result<()> {
    close_exited_child_process_group_with(
        pid,
        || sigkill_process_group_result(pid),
        || bounded_unix_process_table(Duration::from_millis(250)),
    )
}

#[cfg(unix)]
fn close_exited_child_process_group_with(
    pid: u32,
    terminate: impl FnOnce() -> std::io::Result<()>,
    census: impl FnOnce() -> std::io::Result<Vec<UnixProcessRow>>,
) -> std::io::Result<()> {
    let signal_error = match terminate() {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let rows = census().map_err(|census_error| {
        std::io::Error::other(format!(
            "group signal failed ({signal_error}); unable to verify process group {pid}: {census_error}"
        ))
    })?;
    let live_members = rows
        .iter()
        .filter(|row| row.pgid == pid && !row.state.starts_with('Z'))
        .map(|row| row.pid)
        .collect::<Vec<_>>();
    if live_members.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "group signal failed ({signal_error}); process group {pid} still has live members {live_members:?}"
        )))
    }
}

#[cfg(unix)]
fn unix_process_group_exists(pid: u32) -> bool {
    // SAFETY: signal 0 is a read-only existence/permission probe for the
    // recorded process-group id.
    let result = unsafe { libc::kill(-(pid as i32), 0) };
    if result == 0 {
        true
    } else {
        !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        )
    }
}

/// Ask a Unix process group to enter prview's ordinary Ctrl-C unwind.
#[cfg(unix)]
fn interrupt_process_group(pid: u32) -> bool {
    // SAFETY: plain kill(2) against the dedicated review process group. SIGINT
    // is handled by prview's supervisor and does not target the MCP server.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGINT) };
    unix_group_kill_succeeded(result, std::io::Error::last_os_error().raw_os_error())
}

/// Freeze the review root group before the hard fallback inventories its
/// separately-grouped descendants. Once stopped, the root cannot begin or reap
/// another spawn while the parent consumes provisional registrations.
#[cfg(unix)]
fn suspend_process_group(pid: u32) -> bool {
    // SAFETY: SIGSTOP targets only the dedicated review process group. ESRCH is
    // success-shaped because there is no root left to create another child.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGSTOP) };
    unix_group_kill_succeeded(result, std::io::Error::last_os_error().raw_os_error())
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct UnixProcessRow {
    pid: u32,
    ppid: u32,
    pgid: u32,
    state: String,
}

/// Parse the deliberately header-free `ps` shape used by timeout containment.
/// A malformed non-empty row invalidates the census instead of silently
/// weakening the ownership proof.
#[cfg(unix)]
fn parse_unix_process_table(stdout: &[u8]) -> std::io::Result<Vec<UnixProcessRow>> {
    let text = std::str::from_utf8(stdout).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("process table is not UTF-8: {error}"),
        )
    })?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let parse = |field: Option<&str>, name: &str| -> std::io::Result<u32> {
                field
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("process table row lacks {name}: {line:?}"),
                        )
                    })?
                    .parse::<u32>()
                    .map_err(|error| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid {name} in process table row {line:?}: {error}"),
                        )
                    })
            };
            let row = UnixProcessRow {
                pid: parse(fields.next(), "pid")?,
                ppid: parse(fields.next(), "ppid")?,
                pgid: parse(fields.next(), "pgid")?,
                state: fields
                    .next()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("process table row lacks state: {line:?}"),
                        )
                    })?
                    .to_string(),
            };
            if fields.next().is_some() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("process table row has extra fields: {line:?}"),
                ));
            }
            Ok(row)
        })
        .collect()
}

/// Return only process groups led by direct children of the stopped review
/// root. Those are exactly prview's hardened tool roots. A transitive child may
/// still be reaped by its running parent between snapshot and signal; a direct
/// child cannot be reaped while the review root itself remains stopped.
#[cfg(unix)]
fn direct_child_process_groups(rows: &[UnixProcessRow], root_pid: u32) -> Vec<u32> {
    rows.iter()
        .filter(|row| {
            row.pid == row.pgid
                && row.ppid == root_pid
                && row.pgid != root_pid
                && row.pgid > 0
                && row.pgid <= i32::MAX as u32
        })
        .map(|row| row.pgid)
        .collect()
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct UnixOwnedDescendantGroup {
    pgid: u32,
    birth_identity: String,
    depth: usize,
}

#[cfg(unix)]
#[derive(Debug)]
enum HardenedDescendantFreezeError {
    RootGone,
    Other(std::io::Error),
}

#[cfg(unix)]
fn unix_process_state_is_quiescent(state: &str) -> bool {
    state.starts_with('T') || state.starts_with('Z')
}

#[cfg(unix)]
fn unix_process_group_is_quiescent(rows: &[UnixProcessRow], pgid: u32) -> bool {
    let mut found = false;
    for row in rows.iter().filter(|row| row.pgid == pgid) {
        found = true;
        if !unix_process_state_is_quiescent(&row.state) {
            return false;
        }
    }
    found
}

#[cfg(unix)]
fn stable_census_step(consecutive: &mut u8, stable: bool) -> bool {
    if stable {
        *consecutive = consecutive.saturating_add(1);
    } else {
        *consecutive = 0;
    }
    *consecutive >= 2
}

#[cfg(unix)]
fn terminate_exact_descendant_groups(mut groups: Vec<UnixOwnedDescendantGroup>) -> bool {
    groups.sort_by_key(|group| std::cmp::Reverse(group.depth));
    let mut confirmed = true;
    for group in groups {
        let exact =
            crate::storage::process_birth_identity_matches(group.pgid, &group.birth_identity);
        let terminated = exact && sigkill_process_group(group.pgid);
        confirmed &= terminated;
    }
    confirmed
}

#[cfg(unix)]
fn cleanup_frozen_descendant_groups(
    groups: &std::collections::BTreeMap<u32, UnixOwnedDescendantGroup>,
    error: std::io::Error,
) -> std::io::Error {
    if terminate_exact_descendant_groups(groups.values().cloned().collect()) {
        error
    } else {
        std::io::Error::new(
            error.kind(),
            format!("{error}; cleanup of already-frozen descendant groups was unconfirmed"),
        )
    }
}

/// Find every process-group leader still connected to `root_pid` by live PPID
/// ancestry, including nested groups created with `setsid` or `setpgid`.
#[cfg(unix)]
fn transitive_descendant_group_leaders(
    rows: &[UnixProcessRow],
    root_pid: u32,
) -> Vec<(u32, usize)> {
    let mut depths = std::collections::BTreeMap::from([(root_pid, 0_usize)]);
    loop {
        let mut changed = false;
        for row in rows {
            let Some(parent_depth) = depths.get(&row.ppid).copied() else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) = depths.entry(row.pid) {
                entry.insert(parent_depth + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    depths
        .into_iter()
        .filter(|(pid, _)| *pid != root_pid)
        .filter_map(|(pid, depth)| {
            rows.iter()
                .any(|row| row.pid == pid && row.pgid == pid)
                .then_some((pid, depth))
        })
        .collect()
}

#[cfg(unix)]
fn unlinked_process_table_capture() -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let sequence = PROCESS_TABLE_CAPTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        ".prview-process-table-{}-{nonce}-{sequence}",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    if let Err(error) = std::fs::remove_file(&path) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(file)
}

#[cfg(unix)]
fn reap_quarantined_process_table_helpers() {
    let Some(quarantine) = PROCESS_TABLE_REAPER_QUARANTINE.get() else {
        return;
    };
    quarantine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain_mut(|child| {
            let pid = child.id();
            let _ = terminate_process_tree(pid);
            let _ = child.kill();
            !matches!(child.try_wait(), Ok(Some(_)))
        });
}

#[cfg(unix)]
fn quarantine_process_table_helper(child: std::process::Child) {
    PROCESS_TABLE_REAPER_QUARANTINE
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(child);
}

#[cfg(unix)]
fn terminate_bounded_process_table_helper(
    mut child: std::process::Child,
    pid: u32,
    timeout: Duration,
) {
    let _ = terminate_process_tree(pid);
    let _ = child.kill();
    let deadline = std::time::Instant::now().checked_add(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if deadline.is_some_and(|deadline| std::time::Instant::now() < deadline) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(None) | Err(_) => break,
        }
    }

    // The caller's ordinary timeout remains finite even under a pathological
    // wait(2) delay. The shared slot keeps ownership recoverable until the
    // reaper thread has actually started.
    let owned = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    let reaper_owned = std::sync::Arc::clone(&owned);
    let spawn = std::thread::Builder::new()
        .name("prview-ps-reaper".to_string())
        .spawn(move || {
            let child = reaper_owned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let Some(mut child) = child else {
                return;
            };
            let _ = terminate_process_tree(pid);
            let _ = child.kill();
            let _ = child.wait();
        });
    match spawn {
        Ok(handle) => drop(handle),
        Err(error) => {
            eprintln!("prview: failed to launch process-table helper reaper: {error}");
            let child = owned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(child) = child {
                // Thread creation failure is already a system-level resource
                // failure. Preserve non-blocking ownership in a process-wide
                // quarantine; each later census retries kill + try_wait.
                quarantine_process_table_helper(child);
            }
        }
    }
}

/// Take one finite local process-table snapshot. The `ps` helper owns its own
/// group and writes to an already-unlinked mode-0600 file, so output volume
/// cannot fill a pipe. Timeout transfers any slow reap to a dedicated owner.
#[cfg(unix)]
fn bounded_unix_process_table(timeout: Duration) -> std::io::Result<Vec<UnixProcessRow>> {
    use std::io::{Read as _, Seek as _};
    use std::os::unix::process::CommandExt as _;

    reap_quarantined_process_table_helpers();
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::other("process-table timeout overflow"))?;
    let mut capture = unlinked_process_table_capture()?;
    let stdout = capture.try_clone()?;
    let mut command = std::process::Command::new("/bin/ps");
    command
        .args(["-axo", "pid=,ppid=,pgid=,state="])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::null())
        .process_group(0);
    let mut child = command.spawn()?;
    drop(command);
    let pid = child.id();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(None) => {
                terminate_bounded_process_table_helper(child, pid, Duration::from_millis(100));
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "/bin/ps exceeded the process-table snapshot budget",
                ));
            }
            Err(error) => {
                terminate_bounded_process_table_helper(child, pid, Duration::from_millis(100));
                return Err(error);
            }
        }
    };
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "/bin/ps exited with {} during timeout containment",
            status
        )));
    }
    let captured_len = capture.metadata()?.len();
    if captured_len > PROCESS_TABLE_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "/bin/ps process table exceeds the {} byte safety cap",
                PROCESS_TABLE_MAX_BYTES
            ),
        ));
    }
    capture.seek(std::io::SeekFrom::Start(0))?;
    let mut stdout = Vec::new();
    capture.read_to_end(&mut stdout)?;
    parse_unix_process_table(&stdout)
}

/// Freeze every live descendant group of a stopped hardened tool until two
/// consecutive censuses reach a stable fixed point.
///
/// The original tool group is already stopped by the caller. Each newly found
/// group is bound to its native birth identity before and after SIGSTOP, so a
/// later kill never relies on a recyclable PID/PGID alone.
#[cfg(unix)]
fn freeze_hardened_descendant_groups(
    root_pid: u32,
    timeout: Duration,
) -> Result<Vec<UnixOwnedDescendantGroup>, HardenedDescendantFreezeError> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| {
            HardenedDescendantFreezeError::Other(std::io::Error::other(
                "descendant census timeout overflow",
            ))
        })?;
    let mut owned = std::collections::BTreeMap::<u32, UnixOwnedDescendantGroup>::new();
    let mut consecutive_stable_censuses = 0_u8;

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(HardenedDescendantFreezeError::Other(
                cleanup_frozen_descendant_groups(
                    &owned,
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "hardened process tree did not reach a stable stopped census",
                    ),
                ),
            ));
        }
        let rows = match bounded_unix_process_table(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(250)),
        ) {
            Ok(rows) => rows,
            Err(error) => {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(&owned, error),
                ));
            }
        };
        let Some(root) = rows.iter().find(|row| row.pid == root_pid) else {
            if terminate_exact_descendant_groups(owned.values().cloned().collect()) {
                return Err(HardenedDescendantFreezeError::RootGone);
            }
            return Err(HardenedDescendantFreezeError::Other(std::io::Error::other(
                "hardened root disappeared and cleanup of already-frozen descendant groups was unconfirmed",
            )));
        };
        if !unix_process_state_is_quiescent(&root.state)
            || !unix_process_group_is_quiescent(&rows, root_pid)
        {
            if !suspend_process_group(root_pid) {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(
                        &owned,
                        std::io::Error::other(format!(
                            "could not re-stop active member of hardened root process group {root_pid}"
                        )),
                    ),
                ));
            }
            stable_census_step(&mut consecutive_stable_censuses, false);
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }

        let mut added = false;
        for (pgid, depth) in transitive_descendant_group_leaders(&rows, root_pid) {
            if let Some(group) = owned.get(&pgid) {
                if !crate::storage::process_birth_identity_matches(
                    group.pgid,
                    &group.birth_identity,
                ) {
                    return Err(HardenedDescendantFreezeError::Other(
                        cleanup_frozen_descendant_groups(
                            &owned,
                            std::io::Error::other(format!(
                                "descendant process group {pgid} changed identity during containment"
                            )),
                        ),
                    ));
                }
                continue;
            }

            let birth_identity = match crate::storage::process_birth_identity(pgid) {
                Ok(identity) => identity,
                Err(error) => {
                    return Err(HardenedDescendantFreezeError::Other(
                        cleanup_frozen_descendant_groups(&owned, error),
                    ));
                }
            };
            if !crate::storage::process_birth_identity_matches(pgid, &birth_identity) {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(
                        &owned,
                        std::io::Error::other(format!(
                            "descendant process group {pgid} changed before containment"
                        )),
                    ),
                ));
            }
            if !suspend_process_group(pgid) {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(
                        &owned,
                        std::io::Error::other(format!(
                            "could not stop descendant process group {pgid}"
                        )),
                    ),
                ));
            }
            owned.insert(
                pgid,
                UnixOwnedDescendantGroup {
                    pgid,
                    birth_identity: birth_identity.clone(),
                    depth,
                },
            );
            if !crate::storage::process_birth_identity_matches(pgid, &birth_identity) {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(
                        &owned,
                        std::io::Error::other(format!(
                            "descendant process group {pgid} changed while being stopped"
                        )),
                    ),
                ));
            }
            added = true;
        }

        let mut restopped = false;
        for group in owned.values() {
            if unix_process_group_is_quiescent(&rows, group.pgid) {
                continue;
            }
            if !crate::storage::process_birth_identity_matches(group.pgid, &group.birth_identity) {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(
                        &owned,
                        std::io::Error::other(format!(
                            "descendant process group {} changed before re-stop",
                            group.pgid
                        )),
                    ),
                ));
            }
            if !suspend_process_group(group.pgid) {
                return Err(HardenedDescendantFreezeError::Other(
                    cleanup_frozen_descendant_groups(
                        &owned,
                        std::io::Error::other(format!(
                            "could not re-stop active member of descendant process group {}",
                            group.pgid
                        )),
                    ),
                ));
            }
            restopped = true;
        }

        let all_stopped = owned.values().all(|group| {
            unix_process_group_is_quiescent(&rows, group.pgid)
                && rows.iter().any(|row| row.pid == group.pgid)
                && crate::storage::process_birth_identity_matches(group.pgid, &group.birth_identity)
        });
        if stable_census_step(
            &mut consecutive_stable_censuses,
            !added && !restopped && all_stopped,
        ) {
            return Ok(owned.into_values().collect());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Terminate a live hardened tool plus nested groups that escaped its original
/// PGID while retaining PPID ancestry.
///
/// This is the cancellation/timeout primitive for children spawned by prview.
/// A root that has already disappeared falls back to the original PGID kill;
/// no Unix census can recover a descendant that double-forked and was already
/// reparented, so documentation deliberately limits the claim to live ancestry.
#[cfg(unix)]
pub fn terminate_hardened_process_tree(pid: u32) -> bool {
    if !unix_process_group_exists(pid) {
        return true;
    }

    let root_suspended = suspend_process_group(pid);
    let descendants = if root_suspended {
        freeze_hardened_descendant_groups(pid, Duration::from_secs(1))
    } else {
        Err(HardenedDescendantFreezeError::Other(std::io::Error::other(
            format!("could not suspend hardened root process group {pid}"),
        )))
    };

    let descendants_terminated = match descendants {
        Ok(groups) => terminate_exact_descendant_groups(groups),
        Err(HardenedDescendantFreezeError::RootGone) => {
            // A completed wrapper may already have been reaped. Preserve the
            // established same-PGID cleanup without claiming a live-ancestry
            // census that is no longer possible.
            true
        }
        Err(HardenedDescendantFreezeError::Other(error)) => {
            eprintln!("prview: could not prove hardened descendant containment for {pid}: {error}");
            false
        }
    };
    let root_terminated = sigkill_process_group(pid);
    root_suspended && descendants_terminated && root_terminated
}

#[cfg(not(unix))]
pub fn terminate_hardened_process_tree(pid: u32) -> bool {
    terminate_process_tree(pid)
}

/// Wait for one process-table snapshot that both proves the review root has
/// reached stopped state and inventories its descendants. The same snapshot
/// supplies both facts, so the root cannot create or reap between proof and
/// census. Failure is fail-closed and never authorizes a provisional-PID kill.
#[cfg(unix)]
fn unix_stopped_descendant_process_groups(
    root_pid: u32,
    timeout: Duration,
) -> std::io::Result<Vec<u32>> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::other("stopped-root census timeout overflow"))?;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "review root did not reach stopped state before census deadline",
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let rows = match bounded_unix_process_table(remaining.min(Duration::from_millis(250))) {
            Ok(rows) => rows,
            Err(error)
                if error.kind() == std::io::ErrorKind::TimedOut
                    && std::time::Instant::now() < deadline =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let root = rows.iter().find(|row| row.pid == root_pid).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "review root disappeared before stopped-state census",
            )
        })?;
        if root.state.starts_with('T') {
            return Ok(direct_child_process_groups(&rows, root_pid));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(unix)]
fn unix_group_kill_succeeded(result: i32, errno: Option<i32>) -> bool {
    result == 0 || (result == -1 && errno == Some(libc::ESRCH))
}

/// Terminate the primary process group/tree led by `pid` on every platform.
///
/// Unix uses one negative-pgid SIGKILL. Call
/// [`terminate_hardened_process_tree`] for a prview-spawned tool when live
/// descendants may have moved to another group. Windows has no inherited
/// Unix-style process group contract; `taskkill /T /F` walks the tree.
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

/// Stop an MCP-owned quick review whose tools may lead process groups distinct
/// from the review root's group.
///
/// Unix first delivers Ctrl-C so the in-process governor can drain its precise
/// child registry. If that bounded unwind does not finish, the MCP-side sidecar
/// supplies committed and pre-exec provisional tool groups to the hard fallback.
/// Its inherited lock cannot be acquired until every in-flight pre-exec child
/// has either published its PGID or failed the spawn. Windows uses its native
/// recursive tree termination and needs no side ledger.
pub(crate) async fn terminate_supervised_tokio_child(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
    tracker: &mut ExternalChildGroupTracker,
    cooperative_grace: Duration,
    reap_timeout: Duration,
) -> bool {
    #[cfg(not(unix))]
    let _ = cooperative_grace;

    #[cfg(unix)]
    if let Some(pid) = pid {
        let _ = interrupt_process_group(pid);
        if matches!(
            tokio::time::timeout(cooperative_grace, child.wait()).await,
            Ok(Ok(_))
        ) {
            tracker.root_reaped();
            return tracker.finalize_after_root_exit(reap_timeout).await;
        }
    }

    #[cfg(unix)]
    let root_suspended = pid.is_none_or(suspend_process_group);
    #[cfg(not(unix))]
    let root_suspended = true;
    #[cfg(unix)]
    let descendant_groups = match pid {
        Some(pid) if root_suspended => {
            let census = tokio::task::spawn_blocking(move || {
                unix_stopped_descendant_process_groups(pid, Duration::from_secs(1))
            });
            match tokio::time::timeout(Duration::from_millis(1_250), census).await {
                Ok(Ok(Ok(groups))) => Some(groups),
                Ok(Ok(Err(error))) => {
                    eprintln!(
                        "prview: could not prove quick-review descendant groups during cleanup: {error}"
                    );
                    None
                }
                Ok(Err(error)) => {
                    eprintln!("prview: stopped-root census worker failed: {error}");
                    None
                }
                Err(_) => {
                    eprintln!("prview: stopped-root census exceeded its outer scheduling budget");
                    None
                }
            }
        }
        Some(_) => None,
        None => Some(Vec::new()),
    };
    #[cfg(unix)]
    let descendant_groups_terminated = descendant_groups
        .as_deref()
        .is_some_and(|groups| tracker.terminate_proven_descendant_groups(groups));
    #[cfg(not(unix))]
    let descendant_groups_terminated = true;
    let nested_before_root = tracker.terminate_active_groups(false);
    let root_reaped = terminate_and_reap_tokio_child(child, pid, reap_timeout).await;
    if root_reaped {
        tracker.root_reaped();
    }
    // Root reap closes its locked ledger writer. Acquiring that lock proves
    // every child already forked into pre-exec has written a provisional PGID
    // or failed its spawn.
    let finalized_after_root = tracker.finalize_after_root_exit(reap_timeout).await;
    root_suspended
        && descendant_groups_terminated
        && nested_before_root
        && root_reaped
        && finalized_after_root
}

/// Apply the standard rails to `cmd`: detached stdin, `kill_on_drop`, and (unix)
/// its own process group. An MCP review child inherits the ledger only for
/// its async-signal-safe provisional write; the descriptor and capability env
/// are closed/removed at exec. The tool's whole group is then represented by
/// that one registration. Stdout/stderr are left to the caller — piped for
/// captured runs, redirected to files for detached packs.
pub fn harden(cmd: &mut TokioCommand) {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    // unix: own process group so one signal to -pgid reaches the whole tree.
    #[cfg(unix)]
    {
        match external_child_group_pre_exec_fd() {
            Ok(Some(fd)) => {
                // SAFETY: the closure performs only getpid/write on inherited
                // descriptors before exec; it captures one integer by value.
                unsafe {
                    cmd.pre_exec(move || write_external_child_group_provisional(fd));
                }
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("prview: child-group pre-exec ownership is unavailable: {error}");
                // SAFETY: a configured but broken ownership channel must make
                // spawn fail before an untracked process can exec.
                unsafe {
                    cmd.pre_exec(|| Err(std::io::Error::from_raw_os_error(libc::EIO)));
                }
            }
        }
        cmd.process_group(0)
            .env_remove(EXTERNAL_CHILD_GROUP_TOKEN_ENV)
            .env_remove(EXTERNAL_CHILD_GROUP_FD_ENV);
    }
}

/// The half of [`harden`] that a synchronous [`std::process::Command`] can take:
/// detached stdin, the same capability boundary, and, on unix, its own process
/// group.
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
        match external_child_group_pre_exec_fd() {
            Ok(Some(fd)) => {
                // SAFETY: same bounded pre-exec registration as [`harden`].
                unsafe {
                    cmd.pre_exec(move || write_external_child_group_provisional(fd));
                }
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("prview: child-group pre-exec ownership is unavailable: {error}");
                // SAFETY: fail the spawn before exec rather than create an
                // untracked process under a broken ownership capability.
                unsafe {
                    cmd.pre_exec(|| Err(std::io::Error::from_raw_os_error(libc::EIO)));
                }
            }
        }
        cmd.process_group(0)
            .env_remove(EXTERNAL_CHILD_GROUP_TOKEN_ENV)
            .env_remove(EXTERNAL_CHILD_GROUP_FD_ENV);
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
        terminate_hardened_process_tree(child.id())
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
    let group_terminated = pid.is_some_and(terminate_hardened_process_tree);

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
#[derive(Clone, Debug)]
struct OwnedWindowsPid {
    pid: u32,
    birth_identity: String,
}

#[cfg(all(test, windows))]
impl OwnedWindowsPid {
    fn capture(pid: u32) -> std::io::Result<Self> {
        Ok(Self {
            pid,
            birth_identity: crate::storage::process_birth_identity(pid)?,
        })
    }

    fn still_owned(&self) -> bool {
        crate::storage::process_birth_identity_matches(self.pid, &self.birth_identity)
    }
}

#[cfg(all(test, windows))]
pub(crate) struct WindowsProcessTree {
    root: std::process::Child,
    pids: Vec<OwnedWindowsPid>,
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
        let child_pid_tmp = tempdir.path().join("child.pid.tmp");
        let grandchild_pid_tmp = tempdir.path().join("grandchild.pid.tmp");
        let child_script = tempdir.path().join("child.ps1");
        let parent_script = tempdir.path().join("parent.ps1");
        let root_stdout = tempdir.path().join("root.stdout.log");
        let root_stderr = tempdir.path().join("root.stderr.log");

        std::fs::write(
            &child_script,
            format!(
                "$g = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60'\nSet-Content -LiteralPath '{}' -Value $g.Id -Encoding ascii\nMove-Item -LiteralPath '{}' -Destination '{}' -Force\nWait-Process -Id $g.Id\n",
                grandchild_pid_tmp.display(),
                grandchild_pid_tmp.display(),
                grandchild_pidfile.display()
            ),
        )
        .expect("write Windows child script");
        std::fs::write(
            &parent_script,
            format!(
                "$c = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','{}'\nSet-Content -LiteralPath '{}' -Value $c.Id -Encoding ascii\nMove-Item -LiteralPath '{}' -Destination '{}' -Force\nWait-Process -Id $c.Id\n",
                child_script.display(),
                child_pid_tmp.display(),
                child_pid_tmp.display(),
                child_pidfile.display()
            ),
        )
        .expect("write Windows parent script");

        let mut root = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&parent_script)
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(&root_stdout).expect("root stdout log"))
            .stderr(std::fs::File::create(&root_stderr).expect("root stderr log"))
            .spawn()
            .expect("spawn root PowerShell");
        let root_pid = root.id();
        let root_owner = OwnedWindowsPid::capture(root_pid).unwrap_or_else(|error| {
            let _ = root.kill();
            let _ = root.wait();
            panic!("capture root PowerShell birth identity for PID {root_pid}: {error}");
        });

        // Establish cleanup ownership before waiting for either descendant to
        // publish its PID. If setup times out or panics, Drop can already kill
        // the root with /T and therefore cannot orphan an unrecorded child.
        let mut tree = Self {
            pids: vec![root_owner],
            root,
            _tempdir: tempdir,
            verified_gone: false,
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut child_owner = None;
        let mut grandchild_owner = None;
        let descendants = loop {
            if child_owner.is_none()
                && let Some(pid) = try_read_windows_pid(&child_pidfile)
                && let Ok(owner) = OwnedWindowsPid::capture(pid)
            {
                tree.pids.push(owner.clone());
                child_owner = Some(owner);
            }
            if grandchild_owner.is_none()
                && let Some(pid) = try_read_windows_pid(&grandchild_pidfile)
                && let Ok(owner) = OwnedWindowsPid::capture(pid)
            {
                tree.pids.push(owner.clone());
                grandchild_owner = Some(owner);
            }
            let child_alive = child_owner
                .as_ref()
                .is_some_and(OwnedWindowsPid::still_owned);
            let grandchild_alive = grandchild_owner
                .as_ref()
                .is_some_and(OwnedWindowsPid::still_owned);
            if child_alive && grandchild_alive {
                break [
                    child_owner.expect("live child PID"),
                    grandchild_owner.expect("live grandchild PID"),
                ];
            }
            let root_status = tree.root.try_wait().expect("probe root PowerShell status");
            let diagnostics = || {
                format!(
                    "root_status={root_status:?}; child_owner={child_owner:?}; child_alive={child_alive}; grandchild_owner={grandchild_owner:?}; grandchild_alive={grandchild_alive}; root_stdout={:?}; root_stderr={:?}",
                    std::fs::read_to_string(&root_stdout).unwrap_or_default(),
                    std::fs::read_to_string(&root_stderr).unwrap_or_default(),
                )
            };
            assert!(
                root_status.is_none(),
                "{label} root PowerShell exited before publishing a live tree: {}",
                diagnostics()
            );
            if std::time::Instant::now() >= deadline {
                panic!(
                    "{label} did not publish a live child and grandchild within 30s: {}",
                    diagnostics()
                );
            }
            sleep(Duration::from_millis(25));
        };

        tree.pids = vec![
            tree.pids[0].clone(),
            descendants[0].clone(),
            descendants[1].clone(),
        ];
        tree.assert_all_running(label);
        tree
    }

    pub(crate) fn root_pid(&self) -> u32 {
        self.pids[0].pid
    }

    pub(crate) fn pids(&self) -> [u32; 3] {
        [self.pids[0].pid, self.pids[1].pid, self.pids[2].pid]
    }

    pub(crate) fn assert_all_running(&self, phase: &str) {
        for (role, owner) in ["root", "child", "grandchild"].into_iter().zip(&self.pids) {
            assert!(
                owner.still_owned(),
                "{phase}: captured {role} PID {} is not the recorded live process",
                owner.pid
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
                .zip(&self.pids)
                .filter(|(_, owner)| owner.still_owned())
                .map(|(role, owner)| (role, owner.pid))
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
        for owner in self.pids.iter().rev() {
            if owner.still_owned() {
                terminate_process_tree(owner.pid);
            }
        }
        let _ = self.root.kill();
        let _ = self.root.wait();
    }
}

#[cfg(all(test, windows))]
fn try_read_windows_pid(path: &std::path::Path) -> Option<u32> {
    let first = std::fs::read_to_string(path).ok()?;
    let second = std::fs::read_to_string(path).ok()?;
    stable_windows_pid_from_reads(&first, &second)
}

#[cfg(all(test, windows))]
fn stable_windows_pid_from_reads(first: &str, second: &str) -> Option<u32> {
    if first != second || !first.ends_with('\n') {
        return None;
    }
    let mut tokens = first.split_whitespace();
    let pid = tokens.next()?.parse().ok()?;
    if pid == 0 || tokens.next().is_some() {
        return None;
    }
    Some(pid)
}

#[cfg(all(test, windows))]
pub(crate) fn windows_pid_exists(pid: u32) -> bool {
    crate::storage::is_process_alive(pid)
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
    const BROKEN_CHILD_GROUP_FIXTURE_ENV: &str = "PRVIEW_BROKEN_CHILD_GROUP_FIXTURE";
    #[cfg(unix)]
    const CHILD_GROUP_FD_FIXTURE_ENV: &str = "PRVIEW_CHILD_GROUP_FD_FIXTURE";
    #[cfg(unix)]
    const CHILD_GROUP_FD_EXPECT_ENV: &str = "PRVIEW_CHILD_GROUP_FD_EXPECT";
    #[cfg(unix)]
    const CHILD_GROUP_FD_IDENTITY_ENV: &str = "PRVIEW_CHILD_GROUP_FD_IDENTITY";

    #[cfg(unix)]
    #[test]
    fn waitable_exit_after_birth_identity_failure_is_completed() {
        let completed = classify_external_child_birth_identity(
            Err(std::io::Error::from_raw_os_error(libc::ESRCH)),
            || Ok(true),
        )
        .expect("an unreaped direct-child exit is definitive completion");
        assert!(matches!(completed, ExternalChildBirthIdentity::ChildExited));

        let ambiguous = classify_external_child_birth_identity(
            Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            || Ok(false),
        )
        .expect_err("a child not observed exited must fail closed");
        assert_eq!(ambiguous.raw_os_error(), Some(libc::EPERM));

        let captured = classify_external_child_birth_identity(Ok("birth".to_string()), || {
            panic!("a successful native identity must not probe child status")
        })
        .expect("captured identity");
        assert!(matches!(
            captured,
            ExternalChildBirthIdentity::Captured(identity) if identity == "birth"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn external_child_group_fd_fixture() {
        let Some(fd) = std::env::var_os(CHILD_GROUP_FD_FIXTURE_ENV) else {
            return;
        };
        let fd = fd
            .to_string_lossy()
            .parse::<libc::c_int>()
            .expect("numeric fixture fd");
        let expected = std::env::var(CHILD_GROUP_FD_EXPECT_ENV).expect("fd expectation");
        let expected_identity =
            std::env::var(CHILD_GROUP_FD_IDENTITY_ENV).expect("ledger identity expectation");
        let descriptor_identity = || -> Option<String> {
            // SAFETY: `stat` is writable storage and fstat only inspects the
            // numeric fixture descriptor.
            let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
            (unsafe { libc::fstat(fd, &mut stat) } == 0)
                .then(|| format!("{}:{}", stat.st_dev, stat.st_ino))
        };
        // SAFETY: F_GETFD only inspects the numeric fixture descriptor.
        let before = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        match expected.as_str() {
            "closed" => {
                assert_ne!(
                    descriptor_identity().as_deref(),
                    Some(expected_identity.as_str()),
                    "unrelated exec inherited the ledger descriptor"
                );
            }
            "open" => {
                assert_ne!(before, -1, "quick-review root lost the ledger fd");
                assert_eq!(
                    descriptor_identity().as_deref(),
                    Some(expected_identity.as_str()),
                    "quick-review root inherited a different descriptor at the reused fd number"
                );
                initialize_external_child_group_capability()
                    .expect("quick-review root adopts inherited capability");
                // SAFETY: same descriptor after initialization.
                let after = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert_ne!(after, -1);
                assert_ne!(after & libc::FD_CLOEXEC, 0, "root restores CLOEXEC");
            }
            other => panic!("unexpected fd expectation {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_child_group_fd_is_inherited_only_by_the_attached_root() {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::MetadataExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut root = TokioCommand::new(std::env::current_exe().unwrap());
        root.args([
            "--exact",
            "proc::tests::external_child_group_fd_fixture",
            "--nocapture",
        ])
        .env(CHILD_GROUP_FD_EXPECT_ENV, "open");
        harden(&mut root);
        let tracker_setup = ExternalChildGroupTracker::attach(&mut root, tmp.path())
            .expect("attach child-group tracker");
        let ledger_fd = tracker_setup
            .ledger_writer
            .as_ref()
            .expect("parent writer")
            .as_raw_fd();
        let metadata = std::fs::metadata(&tracker_setup.path).expect("ledger metadata");
        let ledger_identity = format!("{}:{}", metadata.dev(), metadata.ino());

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proc::tests::external_child_group_fd_fixture",
                "--nocapture",
            ])
            .env(CHILD_GROUP_FD_FIXTURE_ENV, ledger_fd.to_string())
            .env(CHILD_GROUP_FD_EXPECT_ENV, "closed")
            .env(CHILD_GROUP_FD_IDENTITY_ENV, &ledger_identity)
            .status()
            .expect("spawn unrelated sentinel");
        assert!(status.success(), "unrelated sentinel validates CLOEXEC");

        root.env(CHILD_GROUP_FD_FIXTURE_ENV, ledger_fd.to_string())
            .env(CHILD_GROUP_FD_IDENTITY_ENV, &ledger_identity);
        let mut child = root.spawn().expect("spawn attached quick root");
        let mut tracker = tracker_setup;
        tracker.child_spawned(child.id());
        let status = child.wait().await.expect("wait attached quick root");
        assert!(status.success());
        tracker.root_reaped();
        assert!(
            tracker
                .finalize_after_root_exit(Duration::from_secs(2))
                .await,
            "root closes its inherited writer after adopting the capability"
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_external_child_group_capability_fixture() {
        if std::env::var_os(BROKEN_CHILD_GROUP_FIXTURE_ENV).is_none() {
            return;
        }

        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        harden_std(&mut command);
        let error = command
            .spawn()
            .expect_err("a broken parent ledger must fail before child exec");
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }

    #[cfg(unix)]
    #[test]
    fn broken_external_child_group_capability_fails_closed() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "proc::tests::broken_external_child_group_capability_fixture",
            "--nocapture",
        ]);
        harden_std(&mut command);
        command
            .env(BROKEN_CHILD_GROUP_FIXTURE_ENV, "1")
            .env(EXTERNAL_CHILD_GROUP_TOKEN_ENV, "test-token")
            .env(EXTERNAL_CHILD_GROUP_FD_ENV, "999999");
        let status = command.status().expect("run broken-capability fixture");
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn external_child_group_tracker_rejects_a_truncated_final_record() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let mut command = TokioCommand::new("true");
        let mut tracker = ExternalChildGroupTracker::attach(&mut command, tmp.path())
            .expect("attach child-group tracker");
        let mode = std::fs::metadata(&tracker.path)
            .expect("child-group sidecar metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "sidecar must not grant group/other access");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&tracker.path)
            .expect("open child-group sidecar")
            .write_all(b"+123\ttruncated")
            .expect("write truncated child-group record");

        assert!(
            !tracker.terminate_active_groups(true),
            "a partial final registration must keep containment unconfirmed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_child_group_tracker_accepts_late_finish_after_parent_cleanup() {
        use std::io::Write as _;

        const ABSENT_PID: u32 = i32::MAX as u32;
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut command = TokioCommand::new("true");
        let mut tracker = ExternalChildGroupTracker::attach(&mut command, tmp.path())
            .expect("attach child-group tracker");
        let mut sidecar = std::fs::OpenOptions::new()
            .append(true)
            .open(&tracker.path)
            .expect("open child-group sidecar");
        sidecar
            .write_all(format!("+{ABSENT_PID}\ttest-birth\n").as_bytes())
            .expect("write registration");
        assert!(tracker.terminate_active_groups(false));

        sidecar
            .write_all(format!("-{ABSENT_PID}\ttest-birth\n").as_bytes())
            .expect("write late completion");
        assert!(
            tracker.terminate_active_groups(true),
            "a late child unwind after parent cleanup is coherent, not an unknown finish"
        );
    }

    #[cfg(unix)]
    #[test]
    fn descendant_group_census_requires_a_direct_child_group_leader() {
        let rows = parse_unix_process_table(
            b"100 1 100 T\n250 100 250 S\n300 200 300 S\n200 100 100 S\n301 300 300 S\n400 1 400 S\n500 200 400 S\n",
        )
        .expect("parse process table");
        assert_eq!(direct_child_process_groups(&rows, 100), vec![250]);
    }

    #[cfg(unix)]
    #[test]
    fn transitive_census_includes_nested_detached_group_leaders() {
        let rows = parse_unix_process_table(
            b"100 1 100 T\n200 100 100 T\n300 200 300 S\n301 300 300 S\n400 301 400 S\n500 1 500 S\n",
        )
        .expect("parse process table");
        assert_eq!(
            transitive_descendant_group_leaders(&rows, 100),
            vec![(300, 2), (400, 4)],
        );
    }

    #[cfg(unix)]
    #[test]
    fn stopped_and_zombie_roots_are_quiescent_for_descendant_census() {
        assert!(unix_process_state_is_quiescent("T"));
        assert!(unix_process_state_is_quiescent("T+"));
        assert!(unix_process_state_is_quiescent("Z"));
        assert!(unix_process_state_is_quiescent("Z+"));
        assert!(!unix_process_state_is_quiescent("S"));
        assert!(!unix_process_state_is_quiescent("R+"));
    }

    #[cfg(unix)]
    #[test]
    fn process_group_census_requires_every_visible_member_to_be_quiescent() {
        let mixed = parse_unix_process_table(b"100 1 100 T\n101 100 100 S\n102 100 100 Z\n")
            .expect("parse mixed process group");
        assert!(!unix_process_group_is_quiescent(&mixed, 100));

        let stopped = parse_unix_process_table(b"100 1 100 T\n101 100 100 T+\n102 100 100 Z\n")
            .expect("parse quiescent process group");
        assert!(unix_process_group_is_quiescent(&stopped, 100));
        assert!(!unix_process_group_is_quiescent(&stopped, 999));
    }

    #[cfg(unix)]
    #[test]
    fn stable_census_requires_two_quiescent_observations_after_a_restop() {
        let mut consecutive = 1;
        assert!(!stable_census_step(&mut consecutive, false));
        assert_eq!(consecutive, 0);
        assert!(!stable_census_step(&mut consecutive, true));
        assert_eq!(consecutive, 1);
        assert!(stable_census_step(&mut consecutive, true));
        assert_eq!(consecutive, 2);
    }

    #[cfg(unix)]
    #[test]
    fn process_table_parser_rejects_partial_and_extra_rows() {
        assert!(parse_unix_process_table(b"100 1\n").is_err());
        assert!(parse_unix_process_table(b"100 1 100 S extra\n").is_err());
        assert!(parse_unix_process_table(&[0xff, b'\n']).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_table_snapshot_contains_the_current_process() {
        let rows = bounded_unix_process_table(Duration::from_secs(2))
            .expect("read bounded local process table");
        assert!(
            rows.iter().any(|row| row.pid == std::process::id()),
            "process-table snapshot must contain its caller"
        );
    }

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

    #[cfg(windows)]
    #[test]
    fn published_windows_pid_reader_rejects_partial_or_extra_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("child.pid");

        assert_eq!(try_read_windows_pid(&pidfile), None);
        std::fs::write(&pidfile, "").expect("empty pid payload");
        assert_eq!(try_read_windows_pid(&pidfile), None);
        std::fs::write(&pidfile, "987").expect("partial pid payload");
        assert_eq!(try_read_windows_pid(&pidfile), None);

        std::fs::write(&pidfile, "987\r\n").expect("complete pid payload");
        assert_eq!(try_read_windows_pid(&pidfile), Some(987));

        std::fs::write(&pidfile, "987 32145\r\n").expect("extra pid payload");
        assert_eq!(try_read_windows_pid(&pidfile), None);
        assert_eq!(stable_windows_pid_from_reads("0\r\n", "0\r\n"), None);
        assert_eq!(stable_windows_pid_from_reads("987\r\n", "9873\r\n"), None);
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

    /// Helper process for the two cross-PGID containment regressions below.
    /// It is inert in the ordinary suite and becomes a long-lived descendant
    /// only when a parent test invokes this exact test in a subprocess.
    #[cfg(unix)]
    #[test]
    fn unix_detached_descendant_helper() {
        let Ok(pidfile) = std::env::var("PRVIEW_TEST_DETACHED_PIDFILE") else {
            return;
        };
        let mode = std::env::var("PRVIEW_TEST_DETACHED_MODE").expect("detachment mode");
        // A non-interactive shell normally keeps its background child in the
        // shell's group. Make that precondition explicit so the fixture never
        // becomes a group leader merely because one shell changes job-control
        // policy, which would make setsid fail with EPERM for the wrong reason.
        // SAFETY: both queries and setpgid target this dedicated helper and its
        // live parent; no unrelated process is changed.
        let parent_group = unsafe { libc::getpgid(libc::getppid()) };
        assert!(parent_group > 0, "read parent process group");
        if unsafe { libc::getpgrp() } == unsafe { libc::getpid() } {
            assert_eq!(
                unsafe { libc::setpgid(0, parent_group) },
                0,
                "join wrapper process group before detaching: {}",
                std::io::Error::last_os_error(),
            );
        }
        let detached = match mode.as_str() {
            "setsid" => {
                // SAFETY: the helper is a dedicated subprocess and requests a
                // new session only for itself before publishing its PID.
                unsafe { libc::setsid() != -1 }
            }
            "setpgid" => {
                // SAFETY: pid/pgid zero means this helper process only; it
                // creates a group named by its own PID.
                unsafe { libc::setpgid(0, 0) != -1 }
            }
            other => panic!("unknown detachment mode {other}"),
        };
        assert!(
            detached,
            "{mode} failed: {}",
            std::io::Error::last_os_error()
        );
        std::fs::write(pidfile, format!("{}\n", std::process::id())).expect("publish helper pid");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[cfg(unix)]
    async fn assert_timeout_reaps_detached_group(mode: &str) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicI32, Ordering};

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join(format!("{mode}.pid"));
        let helper = std::env::current_exe().expect("current test binary");
        let mut cmd = TokioCommand::new("sh");
        cmd.args([
            "-c",
            "\"$PRVIEW_TEST_HELPER\" --exact proc::tests::unix_detached_descendant_helper --nocapture & wait",
        ])
        .env("PRVIEW_TEST_HELPER", helper)
        .env("PRVIEW_TEST_DETACHED_PIDFILE", &pidfile)
        .env("PRVIEW_TEST_DETACHED_MODE", mode);

        let published_pid = Arc::new(AtomicI32::new(0));
        let captured_pid = Arc::clone(&published_pid);
        let captured_pidfile = pidfile.clone();
        let error = run_capture_with_timeout_after_spawn(
            cmd,
            Duration::from_secs(3),
            "detached-group-tree",
            || anyhow::anyhow!("detached group tree timed out"),
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
        .expect_err("the live detached descendant must reach the timeout path");
        assert!(error.to_string().contains("detached group tree timed out"));

        let descendant = published_pid.load(Ordering::Acquire);
        assert_ne!(descendant, 0, "helper must publish its detached PID");
        assert_grandchild_reaped(descendant).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reaps_a_setsid_descendant() {
        assert_timeout_reaps_detached_group("setsid").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reaps_a_setpgid_descendant() {
        assert_timeout_reaps_detached_group("setpgid").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn governor_cancel_reaps_a_setsid_descendant() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("cancel-setsid.pid");
        let helper = std::env::current_exe().expect("current test binary");
        let mut cmd = TokioCommand::new("sh");
        cmd.args([
            "-c",
            "\"$PRVIEW_TEST_HELPER\" --exact proc::tests::unix_detached_descendant_helper --nocapture & wait",
        ])
        .env("PRVIEW_TEST_HELPER", helper)
        .env("PRVIEW_TEST_DETACHED_PIDFILE", &pidfile)
        .env("PRVIEW_TEST_DETACHED_MODE", "setsid");

        let governor = std::sync::Arc::new(crate::governor::ResourceGovernor::new());
        let canceller = std::sync::Arc::clone(&governor);
        let marker = pidfile.clone();
        let cancel_task = tokio::spawn(async move {
            for _ in 0..400 {
                if read_published_unix_pid(&marker).is_some() {
                    canceller.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("detached helper did not publish before cancellation");
        });

        let _ = crate::governor::with_child_scope(
            std::sync::Arc::clone(&governor),
            "detached-cancel-tree",
            run_capture_with_timeout(cmd, Duration::from_secs(30), "detached-cancel-tree", || {
                anyhow::anyhow!("detached cancel tree timed out")
            }),
        )
        .await;
        cancel_task.await.expect("canceller task");

        let descendant = read_published_unix_pid(&pidfile).expect("published detached PID");
        assert_grandchild_reaped(descendant).await;
        assert_eq!(governor.inflight_count(), 0);
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

    #[cfg(unix)]
    #[test]
    fn exited_group_signal_failure_requires_a_no_live_members_census() {
        let zombie_only = vec![UnixProcessRow {
            pid: 41,
            ppid: 1,
            pgid: 41,
            state: "Z".to_string(),
        }];
        close_exited_child_process_group_with(
            41,
            || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            || Ok(zombie_only),
        )
        .expect("a zombie leader is not a live surviving group member");

        let with_survivor = vec![
            UnixProcessRow {
                pid: 41,
                ppid: 1,
                pgid: 41,
                state: "Z".to_string(),
            },
            UnixProcessRow {
                pid: 42,
                ppid: 41,
                pgid: 41,
                state: "S".to_string(),
            },
        ];
        let error = close_exited_child_process_group_with(
            41,
            || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            || Ok(with_survivor),
        )
        .expect_err("a live survivor must keep the closure fail-closed");
        assert!(
            error.to_string().contains("live members [42]"),
            "unexpected error: {error}"
        );

        let error = close_exited_child_process_group_with(
            41,
            || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
            || Err(std::io::Error::other("fixture census failure")),
        )
        .expect_err("an unreadable census cannot certify closure");
        assert!(
            error.to_string().contains("unable to verify"),
            "unexpected error: {error}"
        );
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
