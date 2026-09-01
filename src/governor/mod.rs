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

pub(crate) use supervisor::InterruptSupervisor;
pub use supervisor::{CtrlC, Interrupts, blocking_stage};

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

/// The budget assumed when the machine will not say how many cores it has.
///
/// Not 1: a governor that serialises the whole run because one syscall failed
/// turns an unknown into a stall. Not the core count of any particular machine
/// either — four is small enough to be safe on a container with two cores and
/// large enough to keep a real box busy.
const FALLBACK_BUDGET: u32 = 4;

/// Operator-selected machine resource policy.
///
/// `Safe` is deliberately the default: a review is allowed to be slower, but
/// it must not make an ordinary developer machine unusable. `Balanced` is an
/// explicit opt-in and is still bounded by child-worker caps and load
/// backpressure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ResourceBudget {
    #[default]
    Safe,
    Balanced,
}

impl ResourceBudget {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Balanced => "balanced",
        }
    }
}

/// The concrete resource envelope selected once for a run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourcePlan {
    pub requested: ResourceBudget,
    pub effective: ResourceBudget,
    pub logical_cores: u32,
    pub total_budget: u32,
    pub heavy_cost: u32,
    pub worker_limit: u32,
    pub load_per_core: Option<f64>,
    pub backpressured: bool,
}

impl ResourcePlan {
    /// Detect a conservative plan from the machine at invocation time.
    #[must_use]
    pub fn detect(requested: ResourceBudget) -> Self {
        Self::from_observation(requested, available_budget(), current_load_average())
    }

    fn from_observation(
        requested: ResourceBudget,
        logical_cores: u32,
        load_average: Option<f64>,
    ) -> Self {
        let logical_cores = logical_cores.max(1);
        let load_per_core = load_average.map(|load| (load / f64::from(logical_cores)).max(0.0));
        // Unknown load is treated as pressure, not as spare capacity. This is
        // intentionally conservative on platforms where no cheap load probe is
        // available.
        let backpressured = requested == ResourceBudget::Balanced
            && load_per_core.is_none_or(|ratio| ratio >= 0.75);
        let effective = if backpressured {
            ResourceBudget::Safe
        } else {
            requested
        };

        match effective {
            ResourceBudget::Safe => Self {
                requested,
                effective,
                logical_cores,
                total_budget: 1,
                heavy_cost: 1,
                worker_limit: 1,
                load_per_core,
                backpressured,
            },
            ResourceBudget::Balanced => {
                // Keep the envelope bounded even on a large build host: at
                // most two capped heavy parents, each with at most four
                // descendants. This is a throughput opt-in, not "use it all".
                let total_budget = logical_cores.clamp(1, 8);
                Self {
                    requested,
                    effective,
                    logical_cores,
                    total_budget,
                    heavy_cost: heavy_cost_for(total_budget),
                    worker_limit: logical_cores.div_ceil(2).clamp(1, 4),
                    load_per_core,
                    backpressured,
                }
            }
        }
    }
}

/// How much of the machine a task is expected to want.
///
/// Deliberately semantic rather than numeric. What a weight costs is the governor's
/// call, because it depends on the budget the governor is working with — a
/// `Heavy` task on a 16-core box and on a 2-core box are the same DECLARATION
/// and very different permit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Reads a file, shells out briefly, parses something. Cheap enough that
    /// running several is free.
    Light,
    /// Wants the machine: a compiler, a test suite, a whole-project linter.
    /// Its descendant pool has an explicit worker cap.
    Heavy,
    /// A whole-machine tool whose descendant fan-out is unsupported, unknown,
    /// or not independently capped. It is always serialized.
    Exclusive,
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
struct InflightChild {
    pid: u32,
    external_birth_identity: Option<String>,
}

/// The process-tree half of a cancellation after the run has already been
/// closed to new work.
///
/// Keeping the drained children in an owned batch lets async interrupt owners
/// move the blocking platform termination calls off their executor thread while
/// they continue polling for the operator's second interrupt. Synchronous
/// callers still finish the same batch inline through [`ResourceGovernor::cancel`].
struct CancellationBatch(Vec<InflightChild>);

impl CancellationBatch {
    fn terminate(self) {
        for child in self.0 {
            if crate::proc::terminate_process_tree(child.pid)
                && let Some(identity) = child.external_birth_identity.as_deref()
            {
                crate::proc::report_external_child_group_finished(child.pid, identity);
            }
        }
    }
}

fn spawn_blocking_cancellation(
    cancellation: impl FnOnce() + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(cancellation)
}

pub struct ResourceGovernor {
    semaphore: Arc<Semaphore>,
    total_budget: u32,
    heavy_cost: u32,
    plan: ResourcePlan,
    /// Live children, keyed by a caller-chosen label. Every child pid is also
    /// its pgid — each child prview spawns leads its own group (see
    /// [`crate::proc::harden`]), so one signal reaches its grandchildren. MCP
    /// quick reviews additionally retain the external incarnation identity.
    inflight: Mutex<HashMap<String, InflightChild>>,
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
    /// A governor using the conservative default policy.
    #[must_use]
    pub fn new() -> Self {
        Self::for_resource_budget(ResourceBudget::Safe)
    }

    /// A governor using the operator-selected resource policy.
    #[must_use]
    pub fn for_resource_budget(budget: ResourceBudget) -> Self {
        Self::from_plan(ResourcePlan::detect(budget))
    }

    pub(crate) fn from_plan(plan: ResourcePlan) -> Self {
        let (cancel_tx, _) = watch::channel(false);
        Self {
            semaphore: Arc::new(Semaphore::new(plan.total_budget as usize)),
            total_budget: plan.total_budget,
            heavy_cost: plan.heavy_cost,
            plan,
            inflight: Mutex::new(HashMap::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_tx,
        }
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
        Self::from_plan(ResourcePlan {
            requested: ResourceBudget::Balanced,
            effective: ResourceBudget::Balanced,
            logical_cores: total_budget,
            total_budget,
            heavy_cost,
            worker_limit: total_budget.div_ceil(2).clamp(1, 4),
            load_per_core: None,
            backpressured: false,
        })
    }

    /// The whole budget, in permits.
    #[must_use]
    pub fn total_budget(&self) -> u32 {
        self.total_budget
    }

    /// The concrete per-run envelope, including descendant worker cap.
    #[must_use]
    pub fn plan(&self) -> ResourcePlan {
        self.plan
    }

    /// Worker cap passed to tools with supported descendant-pool controls.
    #[must_use]
    pub fn worker_limit(&self) -> u32 {
        self.plan.worker_limit
    }

    /// What a [`Weight`] costs against this governor's budget.
    #[must_use]
    pub fn cost(&self, weight: Weight) -> u32 {
        match weight {
            Weight::Light => 1,
            Weight::Heavy => self.heavy_cost,
            Weight::Exclusive => self.total_budget,
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
    /// root. Its owning child handle must not have been waited or reaped yet;
    /// that keeps the PID unavailable for reuse throughout registration.
    ///
    /// Registration and cancellation share the registry lock. If cancellation
    /// won the race after `spawn` but before this call, the child is refused and
    /// its process group is killed immediately; it must never enter a registry
    /// that the completed cancellation pass has already drained.
    pub fn register_child(&self, key: impl Into<String>, pid: u32) -> bool {
        let mut inflight = self.lock_inflight();
        if self.cancelled.load(Ordering::SeqCst) {
            drop(inflight);
            let external_birth_identity = crate::proc::report_external_child_group_started(pid);
            if crate::proc::terminate_process_tree(pid)
                && let Ok(start) = external_birth_identity
                && let Some(identity) = start.into_mirrored_identity()
            {
                crate::proc::report_external_child_group_finished(pid, &identity);
            }
            return false;
        }
        // An MCP quick-review parent cannot discover this separately-grouped
        // tool after killing the review root. Report ownership before exposing
        // the registration so the external hard fallback never sees a live
        // registry entry without its corresponding pid.
        let external_birth_identity = match crate::proc::report_external_child_group_started(pid) {
            Ok(crate::proc::ExternalChildGroupStart::NotMirrored) => None,
            #[cfg(unix)]
            Ok(crate::proc::ExternalChildGroupStart::Mirrored(identity)) => Some(identity),
            #[cfg(unix)]
            Ok(crate::proc::ExternalChildGroupStart::ExitedBeforeMirror) => {
                // The direct child is still unreaped, so this PID/PGID cannot
                // have been reused. Close the whole group NOW: after the owner
                // waits, an unmirrored surviving member would have no durable
                // birth identity and the MCP parent must not signal a merely
                // provisional PGID. ESRCH is success-shaped (the group already
                // vanished). A rejected signal is accepted only when a bounded
                // census proves no live member retains this still-unreusable
                // PGID; every unverified/live failure cancels fail-closed.
                if let Err(error) = crate::proc::close_exited_child_process_group(pid) {
                    drop(inflight);
                    eprintln!(
                        "prview: failed to close exited-before-mirror child group {pid}: {error}"
                    );
                    self.cancel();
                    let _ = crate::proc::terminate_process_tree(pid);
                    return false;
                }
                None
            }
            Err(error) => {
                drop(inflight);
                // The MCP parent cannot prove ownership without the mirror.
                // Fail closed by cancelling the run before refusing this child.
                eprintln!(
                    "prview: failed to register child group {pid} with its external owner: {error}"
                );
                self.cancel();
                crate::proc::terminate_process_tree(pid);
                return false;
            }
        };
        inflight.insert(
            key.into(),
            InflightChild {
                pid,
                external_birth_identity,
            },
        );
        true
    }

    /// Forget a child that has exited. A pid the governor still believes in is a
    /// pid it may signal, and pids are reused.
    pub fn unregister_child(&self, key: &str) {
        let child = self.lock_inflight().remove(key);
        if let Some(child) = child
            && let Some(identity) = child.external_birth_identity.as_deref()
        {
            crate::proc::report_external_child_group_finished(child.pid, identity);
        }
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
        self.begin_cancel().terminate();
    }

    /// Publish cancellation and take exclusive ownership of every child that
    /// was registered before it.
    ///
    /// This phase is deliberately synchronous and bounded: once it returns,
    /// every waiter/newcomer is refused and late child registration takes its
    /// own termination path. Platform process-tree termination is kept in the
    /// returned batch because Windows' `taskkill` fallback is a blocking wait.
    fn begin_cancel(&self) -> CancellationBatch {
        self.cancelled.store(true, Ordering::SeqCst);
        // Closing wakes every task parked on the budget with an error, so a
        // waiter is refused exactly like a newcomer.
        self.semaphore.close();
        // Persist the level even when no receiver exists yet. `send` discards
        // the value in that ordinary state, which parked a late `cancelled()`
        // subscriber forever despite the atomic already being true.
        self.cancel_tx.send_replace(true);

        let children = std::mem::take(&mut *self.lock_inflight());
        CancellationBatch(children.into_values().collect())
    }

    /// Start cancellation without parking the async interrupt owner in a
    /// platform tree-kill primitive.
    ///
    /// The state transition and registry drain happen before this method
    /// returns. Only the owned blocking termination batch moves to Tokio's
    /// blocking pool, so callers can keep polling a second Ctrl-C while still
    /// awaiting ordinary cleanup when the operator does not force an exit.
    pub(crate) fn begin_background_cancel(&self) -> tokio::task::JoinHandle<()> {
        let batch = self.begin_cancel();
        spawn_blocking_cancellation(move || batch.terminate())
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
        if self.is_cancelled() {
            return;
        }
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
    fn lock_inflight(&self) -> std::sync::MutexGuard<'_, HashMap<String, InflightChild>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// This machine's core count, or [`FALLBACK_BUDGET`] when it will not say.
fn available_budget() -> u32 {
    std::thread::available_parallelism().map_or(FALLBACK_BUDGET, |cores| cores.get() as u32)
}

/// One-minute system load where the platform exposes it without starting a
/// helper process. Unknown load deliberately triggers safe fallback for a
/// requested balanced plan.
#[cfg(unix)]
fn current_load_average() -> Option<f64> {
    let mut loads = [0.0_f64; 1];
    // SAFETY: `loads` points to one writable `f64`, and the element count passed
    // to libc exactly matches that allocation.
    (unsafe { libc::getloadavg(loads.as_mut_ptr(), 1) } == 1).then_some(loads[0])
}

#[cfg(not(unix))]
fn current_load_average() -> Option<f64> {
    None
}

/// Half the budget, rounded up, and never zero: on eight cores a `Heavy` task
/// costs four permits, so two run at once; on one core it costs the whole
/// machine, which is the honest answer rather than a deadlock.
fn heavy_cost_for(total_budget: u32) -> u32 {
    total_budget.div_ceil(2).max(1)
}

tokio::task_local! {
    /// The governor for the whole run future.
    ///
    /// Unlike `CHILD_SCOPE`, this spans stage boundaries. It lets in-process
    /// work such as loctree observe the same cancellation requested by the CLI
    /// supervisor without threading a second governor through `App`'s public
    /// API or moving its non-Send git repository to another task.
    static RUN_SCOPE: Arc<ResourceGovernor>;
}

/// Run a future under one run-wide governor cancellation scope.
pub(crate) async fn with_run_scope<F>(governor: Arc<ResourceGovernor>, future: F) -> F::Output
where
    F: std::future::Future,
{
    RUN_SCOPE.scope(governor, future).await
}

/// The governor attached to the current run future, when one exists.
#[must_use]
pub(crate) fn current_run_governor() -> Option<Arc<ResourceGovernor>> {
    RUN_SCOPE.try_with(Arc::clone).ok()
}

/// Supervise a run and expose its governor to every in-process stage.
pub async fn with_cancellation<T>(
    work: impl std::future::Future<Output = anyhow::Result<T>>,
    governor: &Arc<ResourceGovernor>,
    interrupts: impl Interrupts,
) -> anyhow::Result<T> {
    with_run_scope(
        Arc::clone(governor),
        supervisor::with_cancellation(work, governor, interrupts),
    )
    .await
}

/// Supervise synchronous startup work before an [`App`](crate::App) exists.
///
/// PR metadata lookup and config discovery can spawn governed children, but the
/// run-specific governor is not available until config construction finishes.
/// This temporary run scope closes that bootstrap gap and converts a startup
/// interrupt into the same typed cancellation as the main review.
pub async fn supervise_startup_stage<T>(
    stage: impl FnOnce() -> anyhow::Result<T>,
    interrupts: impl Interrupts,
) -> anyhow::Result<T> {
    let governor = Arc::new(ResourceGovernor::new());
    let run_governor = Arc::clone(&governor);
    let work = async move {
        let result = blocking_stage(stage);
        if run_governor.is_cancelled() {
            Err(Cancelled.into())
        } else {
            result
        }
    };
    with_cancellation(work, &governor, interrupts).await
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
/// correct behaviour for the process-spawning helpers that are not part of a
/// governed run (the MCP adapter) and must not be killed as if they were. It is
/// NOT a licence to leave a run's own long commands unscoped: the `uv sync`
/// pre-step spent five minutes ignoring a Ctrl-C that way, and is scoped now.
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

/// The governor attached to the current child scope, when one exists.
///
/// Process owners use this to distinguish an intentionally ungoverned helper
/// from a governed child whose registration was refused because cancellation
/// already won the spawn/register race.
pub(crate) fn current_child_governor() -> Option<Arc<ResourceGovernor>> {
    CHILD_SCOPE
        .try_with(|scope| Arc::clone(&scope.governor))
        .ok()
}

/// Register `pid` with the run-wide governor, if this task is inside a run.
///
/// Unlike [`register_active_child`], this does not need a per-check
/// [`with_child_scope`]. Synchronous git fetch/archive/tar children live in the
/// run scope established by [`with_cancellation`] / TUI `with_run_scope`, and
/// that is enough for Ctrl-C to reach them.
#[must_use]
pub fn register_run_child(pid: u32, label: &str) -> Option<ChildRegistration> {
    let governor = current_run_governor()?;
    let key = format!("run:{label}#{}", CHILD_SEQ.fetch_add(1, Ordering::Relaxed));
    governor
        .register_child(key.clone(), pid)
        .then(|| ChildRegistration { governor, key })
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
            scope
                .governor
                .register_child(key.clone(), pid)
                .then(|| ChildRegistration {
                    governor: Arc::clone(&scope.governor),
                    key,
                })
        })
        .ok()
        .flatten()
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
    use anyhow::Context as _;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    #[test]
    fn cancellation_detection_survives_anyhow_context() {
        let error = Err::<(), _>(Cancelled)
            .context("gate review run failed")
            .expect_err("the cancellation remains an error");
        assert!(is_cancellation(&error));
    }

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
    fn the_default_budget_is_safe_and_balanced_is_bounded() {
        assert_eq!(heavy_cost_for(8), 4, "eight cores run two heavy tasks");
        assert_eq!(heavy_cost_for(4), 2);
        assert_eq!(heavy_cost_for(3), 2, "rounded UP, never below one task");
        assert_eq!(heavy_cost_for(2), 1);
        assert_eq!(heavy_cost_for(1), 1, "one core still admits heavy work");

        let governor = ResourceGovernor::new();
        assert_eq!(governor.total_budget(), 1);
        assert_eq!(governor.cost(Weight::Heavy), 1);
        assert_eq!(governor.cost(Weight::Exclusive), 1);
        assert_eq!(governor.cost(Weight::Light), 1);

        let balanced = ResourcePlan::from_observation(ResourceBudget::Balanced, 16, Some(1.0));
        assert_eq!(balanced.effective, ResourceBudget::Balanced);
        assert_eq!(balanced.total_budget, 8, "large hosts remain capped");
        assert_eq!(balanced.heavy_cost, 4, "at most two heavy parents");
        assert_eq!(balanced.worker_limit, 4, "child pools remain capped");

        let single_core = ResourcePlan::from_observation(ResourceBudget::Balanced, 1, Some(0.0));
        assert_eq!(single_core.effective, ResourceBudget::Balanced);
        assert_eq!(single_core.total_budget, 1, "one core admits one parent");
        assert_eq!(single_core.heavy_cost, 1, "heavy work owns that core");
        assert_eq!(
            single_core.worker_limit, 1,
            "child pools stay single-worker"
        );

        let pressured = ResourcePlan::from_observation(ResourceBudget::Balanced, 8, Some(7.0));
        assert_eq!(pressured.effective, ResourceBudget::Safe);
        assert!(pressured.backpressured);
        assert_eq!(pressured.total_budget, 1);
        assert_eq!(pressured.worker_limit, 1);
    }

    #[tokio::test]
    async fn one_core_balanced_admits_only_one_heavy_parent() {
        let plan = ResourcePlan::from_observation(ResourceBudget::Balanced, 1, Some(0.0));
        assert_eq!(plan.effective, ResourceBudget::Balanced);
        assert_eq!(plan.logical_cores, 1);
        assert_eq!(plan.total_budget, 1);
        assert_eq!(plan.heavy_cost, 1);
        assert_eq!(plan.worker_limit, 1);

        let governor = ResourceGovernor::from_plan(plan);
        let first = governor.acquire(Weight::Heavy).await.expect("first heavy");
        let second =
            tokio::time::timeout(Duration::from_millis(25), governor.acquire(Weight::Heavy)).await;
        assert!(
            second.is_err(),
            "one core must not admit a second heavy parent"
        );
        drop(first);
    }

    #[tokio::test]
    async fn exclusive_work_reserves_the_entire_budget() {
        let governor = ResourceGovernor::with_budget(8, 4);
        assert_eq!(governor.cost(Weight::Exclusive), 8);
        let held = governor
            .acquire(Weight::Light)
            .await
            .expect("light work admitted");
        let exclusive = tokio::time::timeout(
            Duration::from_millis(25),
            governor.acquire(Weight::Exclusive),
        )
        .await;
        assert!(
            exclusive.is_err(),
            "exclusive work must wait for every permit"
        );
        drop(held);
        governor
            .acquire(Weight::Exclusive)
            .await
            .expect("exclusive work starts when the machine is free");
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

    #[tokio::test]
    async fn background_cancel_publishes_state_before_returning() {
        let governor = ResourceGovernor::with_budget(4, 2);

        let termination = governor.begin_background_cancel();

        assert!(governor.is_cancelled());
        assert_eq!(
            governor.acquire(Weight::Light).await.unwrap_err(),
            Cancelled
        );
        termination
            .await
            .expect("an empty termination batch must finish without panic");
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

    #[tokio::test]
    async fn cancelled_returns_for_a_subscriber_created_after_cancel() {
        let governor = ResourceGovernor::with_budget(2, 1);

        // No watch receiver exists at this point. Before send_replace plus the
        // atomic fast path, cancel() dropped the watch value and this timed out.
        governor.cancel();
        tokio::time::timeout(Duration::from_millis(100), governor.cancelled())
            .await
            .expect("a late cancelled() subscriber must observe the stored level");
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
            "the MCP adapter spawns outside any run scope",
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

    /// Real Windows orchestration proof for both sides of the spawn/register
    /// race. The late tree exists before the first cancellation pass but is not
    /// registered until afterwards, so only register_child can reap it.
    #[cfg(windows)]
    #[test]
    fn windows_cancellation_reaps_first_cancel_and_late_registration_trees() {
        let governor = ResourceGovernor::with_budget(2, 1);
        let mut first_cancel = crate::proc::WindowsProcessTree::spawn("first cancel tree");
        let mut late_registration =
            crate::proc::WindowsProcessTree::spawn("late registration tree");
        let first_pids = first_cancel.pids();
        let late_pids = late_registration.pids();

        assert!(
            governor.register_child("first-cancel-tree", first_cancel.root_pid()),
            "the first tree must register before cancellation"
        );
        assert_eq!(governor.inflight_count(), 1);
        first_cancel.assert_all_running("before first cancel");
        late_registration.assert_all_running("before delayed registration");

        // Phase 1: the first cancellation pass drains the registered tree.
        governor.cancel();
        assert_eq!(
            governor.inflight_count(),
            0,
            "the first cancel must drain the governor registry"
        );
        first_cancel.assert_all_gone("first cancel");

        // Phase 2: the already-running second tree arrives after that drain.
        assert!(
            !governor.register_child("late-registration-tree", late_registration.root_pid()),
            "registration after cancellation must be refused"
        );
        assert_eq!(
            governor.inflight_count(),
            0,
            "refused late registration must not repopulate the registry"
        );
        late_registration.assert_all_gone("late registration");

        assert_ne!(
            first_pids, late_pids,
            "both phases must exercise distinct real process trees"
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
            // SAFETY: signal 0 is a read-only existence/permission probe, and
            // `grandchild` came from the process tree created by this test.
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
        assert!(
            gone,
            "grandchild {grandchild} survived cancellation of its group leader"
        );
    }
}
