//! Bounded execution: ONE budget for everything the run puts on the machine.
//!
//! A run's concurrency is currently decided per stage — the checks stage picks
//! its own fan-out, the context stage picks another — so nothing holds the
//! machine-wide number. Two stages each behaving reasonably still oversubscribe
//! a laptop, and the tools that hurt are not equal: `cargo clippy` and `cargo
//! test` each want the whole box, while reading a manifest or shelling out to
//! `git` costs almost nothing.
//!
//! The governor is that missing number. Work declares a [`Weight`], the governor
//! decides what a weight COSTS in permits, and a task holds its slice of the
//! budget for as long as it runs. It also owns the other half of "bounded": the
//! registry of live child processes, so one [`ResourceGovernor::cancel`] takes
//! the whole run down instead of leaving orphaned toolchains behind.
//!
//! The checks stage acquires from it per check ([`crate::checks::run_all`]) and
//! every child it spawns registers itself here through [`with_child_scope`], so the
//! budget shapes when work starts and cancellation reaches what already did.

mod supervisor;

pub use supervisor::{CtrlC, Interrupts, blocking_stage, with_cancellation};

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// The budget assumed when the machine will not say how many cores it has.
///
/// Not 1: a governor that serialises the whole run because one syscall failed
/// turns an unknown into a stall. Not the core count of any particular machine
/// either — four is small enough to be safe on a container with two cores and
/// large enough to keep a real box busy.
const FALLBACK_BUDGET: u32 = 4;

/// How much of the machine a task is expected to want.
///
/// Deliberately two words and no number. What a weight costs is the governor's
/// call, because it depends on the budget the governor is working with — a
/// `Heavy` task on a 16-core box and on a 2-core box are the same DECLARATION
/// and very different permit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Reads a file, shells out briefly, parses something. Cheap enough that
    /// running several is free.
    Light,
    /// Wants the machine: a compiler, a test suite, a whole-project linter.
    Heavy,
}

/// Exit code for a run the operator cancelled.
///
/// 128 + SIGINT, the shell convention for "terminated by an interrupt". It is
/// deliberately outside prview's own map (0 accept, 1 reject/block/quality
/// failure, 3 gate execution error): a cancelled run produced no verdict, and
/// reporting one of those would claim it did.
pub const CANCELLED_EXIT_CODE: i32 = 130;

/// The governor refused because the run was cancelled.
///
/// Returned instead of a permit both to a caller arriving after
/// [`ResourceGovernor::cancel`] and to one already waiting when it fired — a
/// cancelled run must not start work, and a task parked on the budget is work
/// that has not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("run cancelled: the resource governor is no longer granting work")
    }
}

impl std::error::Error for Cancelled {}

/// Whether `err` reports a run the operator cancelled.
///
/// One reader for the whole codebase: [`Cancelled`] travels as an
/// [`anyhow::Error`] through `?` and picks up context layers on the way
/// (`gate review run failed`), so every consumer has to downcast rather than
/// match on a message. A cancelled run is not a failed run — it produced no
/// verdict — and each place that mistakes one for the other reports a verdict
/// the run never reached.
#[must_use]
pub fn is_cancellation(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Cancelled>().is_some()
}

/// A slice of the run's budget, held for exactly as long as the task runs.
///
/// Dropping it returns the permits. There is no `release` — the budget must not
/// depend on a caller remembering to give it back on the error path.
#[derive(Debug)]
pub struct GovernorPermit(OwnedSemaphorePermit);

impl GovernorPermit {
    /// How many permits this task is holding.
    #[must_use]
    pub fn cost(&self) -> u32 {
        self.0.num_permits() as u32
    }
}

/// The run's single budget, plus the registry of processes spawned under it.
pub struct ResourceGovernor {
    semaphore: Arc<Semaphore>,
    total_budget: u32,
    heavy_cost: u32,
    /// Live children, keyed by a caller-chosen label. The value is the pid,
    /// which is also the pgid — every child prview spawns leads its own group
    /// (see [`crate::proc::harden`]), so one signal reaches its grandchildren.
    inflight: Mutex<HashMap<String, u32>>,
    cancelled: Arc<AtomicBool>,
    /// Cancellation as something a dispatcher loop can `select!` on. The atomic
    /// answers the cheap question ("is it over?"); this answers the blocking one
    /// ("tell me when").
    cancel_tx: watch::Sender<bool>,
}

impl Default for ResourceGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceGovernor {
    /// A governor sized to this machine.
    ///
    /// The budget is the core count; a `Heavy` task costs half of it (rounded
    /// up), so eight cores run at most two heavy tasks at once while still
    /// admitting light work beside them.
    #[must_use]
    pub fn new() -> Self {
        let total = available_budget();
        Self::with_budget(total, heavy_cost_for(total))
    }

    /// A governor with an explicit budget — for tests, and for the operator knob
    /// this will eventually be wired to.
    ///
    /// Both arguments are clamped rather than trusted. A zero budget is a
    /// deadlock, and a `heavy_cost` above the total is a task that can never be
    /// admitted at all: tokio would park it forever on permits the semaphore
    /// will never hold. Clamping turns both into "as much of the machine as
    /// there is", which is what the caller meant.
    #[must_use]
    pub fn with_budget(total_budget: u32, heavy_cost: u32) -> Self {
        let total_budget = total_budget.max(1);
        let heavy_cost = heavy_cost.clamp(1, total_budget);
        let (cancel_tx, _) = watch::channel(false);
        Self {
            semaphore: Arc::new(Semaphore::new(total_budget as usize)),
            total_budget,
            heavy_cost,
            inflight: Mutex::new(HashMap::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_tx,
        }
    }

    /// The whole budget, in permits.
    #[must_use]
    pub fn total_budget(&self) -> u32 {
        self.total_budget
    }

    /// What a [`Weight`] costs against this governor's budget.
    #[must_use]
    pub fn cost(&self, weight: Weight) -> u32 {
        match weight {
            Weight::Light => 1,
            Weight::Heavy => self.heavy_cost,
        }
    }

    /// Wait for `weight`'s share of the budget.
    ///
    /// Resolves to a permit that must be held for the duration of the work, or
    /// to [`Cancelled`] if the run was cancelled — including while this call was
    /// already waiting, because cancelling closes the semaphore and every waiter
    /// wakes refused.
    pub async fn acquire(&self, weight: Weight) -> Result<GovernorPermit, Cancelled> {
        self.semaphore
            .clone()
            .acquire_many_owned(self.cost(weight))
            .await
            .map(GovernorPermit)
            .map_err(|_| Cancelled)
    }

    /// Take `weight`'s share of the budget if it is free right now.
    ///
    /// The synchronous counterpart of [`ResourceGovernor::acquire`], for the
    /// artifact stage: `artifacts::generate` is a blocking pipeline with a
    /// poll loop, so it has nothing to `.await` on. `None` means "not now" —
    /// either the budget is spoken for or the run was cancelled, which the
    /// caller separates with [`ResourceGovernor::is_cancelled`] because only
    /// one of the two is worth waiting out.
    #[must_use]
    pub fn try_acquire(&self, weight: Weight) -> Option<GovernorPermit> {
        Arc::clone(&self.semaphore)
            .try_acquire_many_owned(self.cost(weight))
            .ok()
            .map(GovernorPermit)
    }

    /// Record a spawned child so [`ResourceGovernor::cancel`] can reach it.
    ///
    /// `pid` must be the pid of a child spawned through [`crate::proc::harden`],
    /// which makes it the leader of its own process group — that is what lets
    /// one signal take down a `cargo` → `rustc` → `cc` tree rather than just its
    /// root.
    pub fn register_child(&self, key: impl Into<String>, pid: u32) {
        self.lock_inflight().insert(key.into(), pid);
    }

    /// Forget a child that has exited. A pid the governor still believes in is a
    /// pid it may signal, and pids are reused.
    pub fn unregister_child(&self, key: &str) {
        self.lock_inflight().remove(key);
    }

    /// How many children the governor currently believes are alive.
    #[must_use]
    pub fn inflight_count(&self) -> usize {
        self.lock_inflight().len()
    }

    /// End the run: refuse all further work and SIGKILL every registered child's
    /// process group.
    ///
    /// Idempotent, and idempotent in the strong sense — the registry is DRAINED,
    /// not read. A second cancel therefore signals nothing, which matters
    /// because a pid whose process died between the two calls may by then belong
    /// to somebody else.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // Closing wakes every task parked on the budget with an error, so a
        // waiter is refused exactly like a newcomer.
        self.semaphore.close();
        // A watch send fails only when nobody is listening, which is a perfectly
        // ordinary state for a run nothing is supervising.
        let _ = self.cancel_tx.send(true);

        let children = std::mem::take(&mut *self.lock_inflight());
        for pid in children.into_values() {
            #[cfg(unix)]
            crate::proc::sigkill_process_group(pid);
            #[cfg(not(unix))]
            let _ = pid;
        }
    }

    /// Whether the run has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// A receiver that resolves when the run is cancelled, for a dispatcher loop
    /// that has to `select!` between "the next task finished" and "we are done".
    ///
    /// The receiver starts at the CURRENT state, so a loop that subscribes after
    /// cancellation sees `true` immediately rather than waiting for a change
    /// that already happened.
    #[must_use]
    pub fn cancelled_signal(&self) -> watch::Receiver<bool> {
        self.cancel_tx.subscribe()
    }

    /// Resolves when the run is cancelled — and never otherwise.
    ///
    /// [`cancelled_signal`](Self::cancelled_signal) as a plain future, which is
    /// what a `select!` arm actually wants. It exists because the raw receiver
    /// has two edges a caller must not get wrong: `changed()` waits for the NEXT
    /// transition, so a receiver created after the cancel would wait forever for
    /// one that already happened; and a dropped sender means nothing can ever
    /// cancel, which must read as "never", not as "cancelled now".
    pub async fn cancelled(&self) {
        let mut signal = self.cancelled_signal();
        if *signal.borrow() {
            return;
        }
        while signal.changed().await.is_ok() {
            if *signal.borrow() {
                return;
            }
        }
        std::future::pending().await
    }

    /// The registry, recovering from a poisoned lock rather than propagating it:
    /// a panicking task must not turn the run's kill-switch into an error.
    fn lock_inflight(&self) -> std::sync::MutexGuard<'_, HashMap<String, u32>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// This machine's core count, or [`FALLBACK_BUDGET`] when it will not say.
fn available_budget() -> u32 {
    std::thread::available_parallelism().map_or(FALLBACK_BUDGET, |cores| cores.get() as u32)
}

/// Half the budget, rounded up, and never zero: on eight cores a `Heavy` task
/// costs four permits, so two run at once; on one core it costs the whole
/// machine, which is the honest answer rather than a deadlock.
fn heavy_cost_for(total_budget: u32) -> u32 {
    total_budget.div_ceil(2).max(1)
}

tokio::task_local! {
    /// The governor the currently-running task's children belong to.
    ///
    /// A task-local rather than an argument because of where the two halves of
    /// this fact live. The governor is known at the DISPATCHER — one per run,
    /// held by [`crate::App`]. The pid is known at the single spawn point,
    /// [`crate::proc::run_capture_with_timeout`], five frames below it behind
    /// `Check::run(&self, config)`: a trait method the checks stage does not get
    /// to add parameters to without every check and all twenty-odd
    /// `run_command_*` call sites growing a governor argument they never read.
    ///
    /// The scope is established per check future and the checks are polled by
    /// one task, so each sees its own value and no other's — a `tokio::spawn`
    /// inside a check would NOT inherit it, which is why the checks stage keeps
    /// its concurrency in `FuturesUnordered` rather than spawned tasks.
    static CHILD_SCOPE: ChildScope;
}

/// Which run, and under whose name, a child spawned by the current task belongs.
#[derive(Clone)]
struct ChildScope {
    governor: Arc<ResourceGovernor>,
    label: Arc<str>,
}

/// Distinguishes concurrent children of ONE labelled task.
///
/// The registry is a map, so two children registered under the same key would
/// collide: the second would evict the first from the kill list, and the first
/// to exit would then unregister the second. A check that runs `cargo metadata`
/// beside its main command is exactly that case.
static CHILD_SEQ: AtomicU64 = AtomicU64::new(0);

/// Run `future` with its spawned children attributed to `governor` under `label`.
///
/// Outside such a scope [`register_active_child`] is a no-op, which is the
/// correct behaviour for the process-spawning helpers the run also calls outside
/// the checks stage (the `uv sync` pre-step, the MCP adapter): they are not part
/// of a governed run and must not be killed as if they were.
pub async fn with_child_scope<F>(
    governor: Arc<ResourceGovernor>,
    label: &str,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    CHILD_SCOPE
        .scope(
            ChildScope {
                governor,
                label: Arc::from(label),
            },
            future,
        )
        .await
}

/// Register `pid` with the governor of the enclosing [`with_child_scope`], if any.
///
/// The returned guard unregisters on drop, so the success, timeout and spawn-error
/// paths all forget the pid without any of them remembering to — a pid the
/// governor still believes in is a pid it may signal, and pids are reused.
#[must_use]
pub fn register_active_child(pid: u32) -> Option<ChildRegistration> {
    CHILD_SCOPE
        .try_with(|scope| {
            let key = format!(
                "{}#{}",
                scope.label,
                CHILD_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            scope.governor.register_child(key.clone(), pid);
            ChildRegistration {
                governor: Arc::clone(&scope.governor),
                key,
            }
        })
        .ok()
}

/// A live registration in the governor's child registry, released on drop.
pub struct ChildRegistration {
    governor: Arc<ResourceGovernor>,
    key: String,
}

impl Drop for ChildRegistration {
    fn drop(&mut self) {
        self.governor.unregister_child(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// The budget is the point: however the weights are mixed, the permits
    /// actually held at any instant never exceed it.
    #[tokio::test]
    async fn weighted_acquisition_never_oversubscribes_the_budget() {
        let governor = Arc::new(ResourceGovernor::with_budget(8, 4));
        let in_flight = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        for i in 0..24 {
            let governor = Arc::clone(&governor);
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            let weight = if i % 3 == 0 {
                Weight::Heavy
            } else {
                Weight::Light
            };
            tasks.push(tokio::spawn(async move {
                let permit = governor.acquire(weight).await.expect("not cancelled");
                let held = in_flight.fetch_add(permit.cost(), Ordering::SeqCst) + permit.cost();
                peak.fetch_max(held, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                in_flight.fetch_sub(permit.cost(), Ordering::SeqCst);
                held
            }));
        }

        for task in tasks {
            let held = task.await.expect("task must not panic");
            assert!(held <= 8, "{held} permits held against a budget of 8");
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 8,
            "peak {} exceeded the budget",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(
            in_flight.load(Ordering::SeqCst),
            0,
            "permits must come back"
        );
    }

    /// Two heavy tasks on eight cores, not three: the cost of `Heavy` is what
    /// turns a budget into a concurrency limit for the tools that hurt.
    #[tokio::test]
    async fn a_heavy_task_costs_half_the_budget() {
        let governor = ResourceGovernor::with_budget(8, heavy_cost_for(8));
        assert_eq!(governor.cost(Weight::Heavy), 4);
        assert_eq!(governor.cost(Weight::Light), 1);

        let first = governor.acquire(Weight::Heavy).await.expect("first heavy");
        let second = governor.acquire(Weight::Heavy).await.expect("second heavy");
        assert_eq!(first.cost() + second.cost(), 8);

        // The third one has to wait for a permit that is not there.
        let third =
            tokio::time::timeout(Duration::from_millis(50), governor.acquire(Weight::Heavy)).await;
        assert!(third.is_err(), "a third heavy task must not be admitted");

        drop(first);
        governor
            .acquire(Weight::Heavy)
            .await
            .expect("a released permit admits the next heavy task");
    }

    #[test]
    fn the_default_budget_follows_the_core_count() {
        assert_eq!(heavy_cost_for(8), 4, "eight cores run two heavy tasks");
        assert_eq!(heavy_cost_for(4), 2);
        assert_eq!(heavy_cost_for(3), 2, "rounded UP, never below one task");
        assert_eq!(heavy_cost_for(2), 1);
        assert_eq!(heavy_cost_for(1), 1, "one core still admits heavy work");

        let governor = ResourceGovernor::new();
        assert_eq!(governor.total_budget(), available_budget());
        assert_eq!(
            governor.cost(Weight::Heavy),
            heavy_cost_for(available_budget())
        );
        assert_eq!(governor.cost(Weight::Light), 1);
    }

    /// Both clamps exist to prevent a hang, not to be tidy: a zero budget admits
    /// nothing, and a heavy cost above the budget parks that task forever on
    /// permits the semaphore will never hold.
    #[tokio::test]
    async fn an_impossible_budget_is_clamped_rather_than_deadlocked() {
        let governor = ResourceGovernor::with_budget(0, 0);
        assert_eq!(governor.total_budget(), 1);
        assert_eq!(governor.cost(Weight::Heavy), 1);
        governor
            .acquire(Weight::Heavy)
            .await
            .expect("a clamped budget still admits work");

        let governor = ResourceGovernor::with_budget(2, 9);
        assert_eq!(governor.cost(Weight::Heavy), 2);
        governor
            .acquire(Weight::Heavy)
            .await
            .expect("an over-large heavy cost is capped at the whole budget");
    }

    #[tokio::test]
    async fn a_cancelled_governor_grants_nothing_further() {
        let governor = ResourceGovernor::with_budget(4, 2);
        assert!(!governor.is_cancelled());

        governor.cancel();

        assert!(governor.is_cancelled());
        assert_eq!(
            governor.acquire(Weight::Light).await.unwrap_err(),
            Cancelled
        );
        assert_eq!(
            governor.acquire(Weight::Heavy).await.unwrap_err(),
            Cancelled
        );
    }

    /// The case a plain flag check would miss: the task was ALREADY waiting when
    /// cancellation fired, so nothing re-reads the flag on its behalf.
    #[tokio::test]
    async fn cancelling_refuses_a_task_already_waiting_for_the_budget() {
        let governor = Arc::new(ResourceGovernor::with_budget(1, 1));
        let held = governor.acquire(Weight::Light).await.expect("first permit");

        let waiter = {
            let governor = Arc::clone(&governor);
            tokio::spawn(async move { governor.acquire(Weight::Heavy).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        governor.cancel();

        assert_eq!(
            waiter
                .await
                .expect("waiter must not panic")
                .expect_err("a task parked on the budget is work that has not started"),
            Cancelled
        );
        drop(held);
    }

    #[tokio::test]
    async fn the_cancellation_signal_wakes_a_waiting_loop() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let mut signal = governor.cancelled_signal();
        assert!(!*signal.borrow(), "a live run is not cancelled");

        let watcher = {
            let governor = Arc::clone(&governor);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                governor.cancel();
            })
        };

        signal.changed().await.expect("the sender outlives the run");
        assert!(*signal.borrow());
        watcher.await.expect("canceller must not panic");

        // Subscribing afterwards must not wait for a change that already
        // happened.
        assert!(*governor.cancelled_signal().borrow());
    }

    #[test]
    fn the_child_registry_forgets_what_it_is_told_to_forget() {
        let governor = ResourceGovernor::with_budget(2, 1);
        assert_eq!(governor.inflight_count(), 0);

        governor.register_child("clippy", 4242);
        governor.register_child("cargo test", 4243);
        assert_eq!(governor.inflight_count(), 2);

        governor.unregister_child("clippy");
        assert_eq!(governor.inflight_count(), 1);
        governor.unregister_child("nobody spawned this");
        assert_eq!(governor.inflight_count(), 1);
    }

    /// The scope is what makes `register_active_child` safe to call from the
    /// one shared spawn helper: outside a governed run it registers nothing, and
    /// inside one it must not let a check's second child evict its first.
    #[tokio::test]
    async fn children_register_only_inside_a_scope() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));

        assert!(
            register_active_child(4242).is_none(),
            "the MCP adapter and the uv pre-step spawn outside any run scope",
        );
        assert_eq!(governor.inflight_count(), 0);

        let inner = Arc::clone(&governor);
        with_child_scope(Arc::clone(&governor), "Clippy", async move {
            let first = register_active_child(4242).expect("inside a scope");
            let second = register_active_child(4243).expect("a second child of one check");
            assert_eq!(
                inner.inflight_count(),
                2,
                "two concurrent children of one check must not share a key",
            );
            drop(second);
            assert_eq!(inner.inflight_count(), 1);
            drop(first);
        })
        .await;

        assert_eq!(
            governor.inflight_count(),
            0,
            "leaving the scope leaves no pid the governor may signal",
        );
    }

    /// Cancelling twice must be safe. Deliberately registers NOTHING: a test
    /// that parks a pid here to watch it be signalled is a test that signals a
    /// process group, and the only pid available to a unit test without spawning
    /// one is its own. The drain that makes the second cancel a no-op is
    /// asserted on a real child in
    /// `cancelling_kills_a_registered_child_process_group`.
    #[tokio::test]
    async fn cancelling_twice_is_safe() {
        let governor = ResourceGovernor::with_budget(2, 1);

        governor.cancel();
        governor.cancel();

        assert!(governor.is_cancelled());
        assert_eq!(governor.inflight_count(), 0);
        assert_eq!(
            governor.acquire(Weight::Light).await.unwrap_err(),
            Cancelled,
            "a second cancel must not reopen the budget"
        );
    }

    /// The registration path end to end: a child spawned by the shared helper
    /// inside a [`with_child_scope`] is reachable by `cancel`, grandchildren
    /// included. No SIGINT is sent anywhere — the signal is not what is under
    /// test, and the only process signalled is one this test spawned.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_reaps_a_child_spawned_inside_a_scope() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());

        let governor = Arc::new(ResourceGovernor::with_budget(4, 2));
        let worker = {
            let governor = Arc::clone(&governor);
            tokio::spawn(async move {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c").arg(&script);
                with_child_scope(
                    governor,
                    "Mock gate",
                    crate::proc::run_capture_with_timeout(
                        cmd,
                        Duration::from_secs(60),
                        "sh-tree",
                        || anyhow::anyhow!("unexpected timeout"),
                    ),
                )
                .await
            })
        };

        let mut grandchild = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                grandchild = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild = grandchild.expect("sh should record the grandchild pid");
        assert_eq!(
            governor.inflight_count(),
            1,
            "the running child is registered while it runs",
        );

        governor.cancel();
        let _ = worker.await.expect("worker must not panic");

        let mut gone = false;
        for _ in 0..100 {
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) || errno == Some(libc::EPERM) {
                    gone = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            gone,
            "grandchild {grandchild} survived cancellation of the run",
        );
    }

    /// The other half of "bounded": cancelling must reach the whole tree a
    /// registered child leads, not just its root. `sh` leads its own process
    /// group and records a grandchild `sleep`; after the cancel that grandchild
    /// must be gone. Follows the canonical `proc.rs` pattern — the only process
    /// signalled is one this test spawned.
    #[cfg(unix)]
    #[test]
    fn cancelling_kills_a_registered_child_process_group() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        use std::thread::sleep;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());

        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(&script);
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sh tree");

        let mut grandchild = None;
        for _ in 0..100 {
            if let Ok(mut file) = std::fs::File::open(&pidfile) {
                let mut buf = String::new();
                file.read_to_string(&mut buf).ok();
                if let Ok(pid) = buf.trim().parse::<i32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            sleep(Duration::from_millis(20));
        }
        let grandchild = grandchild.expect("sh should record the grandchild pid");

        let governor = ResourceGovernor::with_budget(2, 1);
        governor.register_child("sh-tree", child.id());
        governor.cancel();
        assert_eq!(
            governor.inflight_count(),
            0,
            "the first cancel takes ownership of every registered pid"
        );
        // Safe precisely because the registry was drained: a second cancel
        // re-signals nothing, and this pid may already belong to somebody else.
        governor.cancel();
        let _ = child.wait();

        // ESRCH (gone) and EPERM (pid reused or signal-limited sandbox) both mean
        // reaped; anything else would be a live grandchild.
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
        assert!(
            gone,
            "grandchild {grandchild} survived cancellation of its group leader"
        );
    }
}
