//! Watching for the operator's interrupt while a run is in flight.
//!
//! The supervisor is a SEPARATE task from the run on purpose. Listening for the
//! signal in the same `select!` as the run only works while the run keeps
//! yielding: `artifacts::generate` is a synchronous pipeline that polls its
//! children with `std::thread::sleep`, so for the whole of the longest stage of
//! a review the task was never polled and neither interrupt arm could fire.
//! `tokio::signal::ctrl_c` has by then replaced SIGINT's default disposition, so
//! the terminal could not end the process either — Ctrl-C did nothing at all.
//!
//! Watching from its own task removes that coupling: the run may block its
//! thread for minutes and the interrupt is still observed. [`blocking_stage`]
//! covers the remaining edge, a runtime with a single worker thread, by telling
//! tokio that a stage is about to block so the supervisor keeps a thread to be
//! polled on.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use colored::Colorize;

use super::{CANCELLED_EXIT_CODE, CancelReason, ResourceGovernor, format_budget};

/// What stopped the run.
///
/// Typed rather than a bare "something happened", because the two have
/// different authors and therefore different consequences: an operator's
/// Ctrl-C exits on the shell's interrupt convention, while an exhausted
/// deadline is prview failing to reach a verdict in the time it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// The operator asked the run to stop.
    Operator,
    /// The whole-run deadline elapsed.
    Deadline {
        /// The budget that was exceeded.
        budget: Duration,
    },
}

impl Interrupt {
    /// The cancellation reason this interrupt publishes to the governor.
    #[must_use]
    pub const fn reason(self) -> CancelReason {
        match self {
            Self::Operator => CancelReason::Operator,
            Self::Deadline { budget } => CancelReason::Deadline { budget },
        }
    }
}

/// Where a run's interrupts come from.
///
/// A trait rather than a direct call to [`tokio::signal::ctrl_c`] so the
/// supervisor is testable at all: a test that raised a real SIGINT would raise
/// it at the test harness, which owns the process. Production uses
/// [`DeadlineOrCtrlC`] (or bare [`CtrlC`] where no deadline applies); a test
/// drives the same state machine through a channel.
pub trait Interrupts: Send + 'static {
    /// Resolves on the next interrupt, and never when none can arrive.
    ///
    /// A handler that failed to install must not look like a signal that
    /// arrived — that would cancel a healthy run the moment it started.
    fn next(&mut self) -> impl Future<Output = Interrupt> + Send;

    /// The operator has said they are not willing to wait for the unwind.
    ///
    /// Ends the process, and therefore never returns. Overridden only by tests,
    /// which record the demand instead of acting on it.
    fn abandon_run(&mut self) {
        std::process::exit(CANCELLED_EXIT_CODE);
    }
}

/// The terminal's Ctrl-C, which is what a real run listens to.
pub struct CtrlC;

impl Interrupts for CtrlC {
    async fn next(&mut self) -> Interrupt {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
        Interrupt::Operator
    }
}

/// Ctrl-C plus the run's own deadline: whichever arrives first.
///
/// The second implementation of [`Interrupts`] the contract asks for, and the
/// only place the deadline exists as a mechanism. It is armed on the FIRST
/// poll rather than at construction, so the budget measures supervised run
/// time and not the time the caller spent building an `App`.
///
/// It disarms after the FIRST interrupt of either kind. The supervisor's later
/// calls are asking whether the operator refuses to wait for the unwind, and
/// neither an already-fired deadline resolving instantly nor a budget elapsing
/// during cleanup is an answer to that question.
pub struct DeadlineOrCtrlC {
    ctrl_c: CtrlC,
    budget: Option<Duration>,
    expires_at: Option<tokio::time::Instant>,
}

impl DeadlineOrCtrlC {
    /// Watch Ctrl-C, and `budget` too when the run is bounded.
    #[must_use]
    pub fn new(budget: Option<Duration>) -> Self {
        Self {
            ctrl_c: CtrlC,
            budget,
            expires_at: None,
        }
    }
}

impl Interrupts for DeadlineOrCtrlC {
    async fn next(&mut self) -> Interrupt {
        let Some(budget) = self.budget else {
            return self.ctrl_c.next().await;
        };
        let expires_at = *self
            .expires_at
            .get_or_insert_with(|| tokio::time::Instant::now() + budget);

        let interrupt = tokio::select! {
            biased;
            interrupt = self.ctrl_c.next() => interrupt,
            () = tokio::time::sleep_until(expires_at) => Interrupt::Deadline { budget },
        };
        self.budget = None;
        interrupt
    }
}

/// Drive `work` to completion while `interrupts` are watched from another task.
///
/// The first interrupt CANCELS rather than aborts: the governor stops granting
/// budget and SIGKILLs the process groups the run has spawned, and the run then
/// unwinds through its own error path. That difference is the whole point —
/// returning through `?` runs the destructors on the way out (the ledger's
/// shared worktree snapshot, the heuristics analysis snapshots), where killing
/// the process would leave every one of them on disk.
///
/// A second interrupt is the operator saying they are not willing to wait for
/// that, and is honoured immediately.
pub async fn with_cancellation<T>(
    work: impl Future<Output = Result<T>>,
    governor: &Arc<ResourceGovernor>,
    interrupts: impl Interrupts,
) -> Result<T> {
    with_cancellation_policy(work, governor, interrupts).await
}

/// Supervise work whose selected successful returns prove that their durable
/// commit boundary has already completed.
///
/// Once `is_committed` accepts the returned value, replacing that result with
/// `Cancelled` would publish a verdict and simultaneously claim that no verdict
/// exists. Other successful values remain cancellation-sensitive at the final
/// handoff.
pub async fn with_cancellation_after_commit_if<T>(
    work: impl Future<Output = Result<T>>,
    governor: &Arc<ResourceGovernor>,
    interrupts: impl Interrupts,
    is_committed: impl FnOnce(&T) -> bool,
) -> Result<T> {
    let supervisor = InterruptSupervisor::start(Arc::clone(governor), interrupts);
    let result = work.await;
    let committed_success = result.as_ref().is_ok_and(is_committed);
    supervisor.stop().await;
    if governor.is_cancelled() && !committed_success {
        Err(governor.cancellation_error())
    } else {
        result
    }
}

async fn with_cancellation_policy<T>(
    work: impl Future<Output = Result<T>>,
    governor: &Arc<ResourceGovernor>,
    interrupts: impl Interrupts,
) -> Result<T> {
    // A normal value/error finishes through the explicit biased handoff below:
    // an interrupt already ready when `work` completes must cancel the run, not
    // lose to Drop aborting the watcher and escape as a publishable result.
    // Panic unwind still reaches InterruptSupervisor::drop as the last-resort
    // watcher cleanup.
    let supervisor = InterruptSupervisor::start(Arc::clone(governor), interrupts);
    let result = work.await;
    supervisor.stop().await;
    if governor.is_cancelled() {
        Err(governor.cancellation_error())
    } else {
        result
    }
}

/// A signal owner that can hand responsibility to another input mechanism
/// without a blind gap. TUI preflight keeps this guard alive until raw mode is
/// enabled; `stop().await` gives any already-ready signal priority over the
/// handoff request before confirming that the watcher is gone.
pub(crate) struct InterruptSupervisor {
    handle: Option<tokio::task::JoinHandle<()>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl InterruptSupervisor {
    pub(crate) fn start(governor: Arc<ResourceGovernor>, interrupts: impl Interrupts) -> Self {
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        Self {
            handle: Some(tokio::spawn(supervise(governor, interrupts, stop_rx))),
            stop: Some(stop),
        }
    }

    pub(crate) async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

/// Turn the first interrupt into a cancel and the second into an exit, unless
/// the caller explicitly completes a responsibility handoff first.
async fn supervise(
    governor: Arc<ResourceGovernor>,
    mut interrupts: impl Interrupts,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let interrupt = tokio::select! {
        biased;
        interrupt = interrupts.next() => interrupt,
        _ = &mut stop => return,
    };
    announce_interrupt(interrupt, governor.stage());
    let mut termination = governor.begin_background_cancel_with(interrupt.reason());

    // Windows' native taskkill fallback is a synchronous process wait. It runs
    // on the blocking pool, while this task remains the live owner of the second
    // interrupt. A completed work future may already have sent `stop`; ordinary
    // handoff still waits for tree termination, but the operator can always
    // choose the hard exit instead.
    match wait_for_termination_or_second_interrupt(&mut interrupts, &mut termination).await {
        None => {
            eprintln!("{} second interrupt — exiting now", "^C".yellow().bold());
            interrupts.abandon_run();
            return;
        }
        Some(Ok(())) => {}
        Some(Err(error)) => {
            eprintln!("prview: cancellation tree-termination worker failed: {error}");
        }
    }

    tokio::select! {
        biased;
        _ = interrupts.next() => {}
        _ = &mut stop => return,
    }
    eprintln!("{} second interrupt — exiting now", "^C".yellow().bold());
    interrupts.abandon_run();
}

/// Say who stopped the run, where it was, and — for a deadline — how to change
/// the budget it just spent.
fn announce_interrupt(interrupt: Interrupt, stage: super::RunStage) {
    match interrupt {
        Interrupt::Operator => eprintln!(
            "\n{} stopping running tools and cleaning up (Ctrl-C again to exit now)",
            "^C".yellow().bold(),
        ),
        Interrupt::Deadline { budget } => eprintln!(
            "\n{} deadline of {} exceeded during {} — stopping running tools; \
             the run reports no verdict (use --deadline <time> or --no-deadline)",
            "⏱".yellow().bold(),
            format_budget(budget),
            stage.label(),
        ),
    }
}

/// Wait for the blocking process-tree batch while preserving the second
/// interrupt as the higher-priority escape hatch.
///
/// `None` means the operator interrupted again; `Some` carries the blocking
/// worker's completion so production can surface a worker panic without
/// confusing it with a second signal.
async fn wait_for_termination_or_second_interrupt(
    interrupts: &mut impl Interrupts,
    termination: &mut tokio::task::JoinHandle<()>,
) -> Option<std::result::Result<(), tokio::task::JoinError>> {
    tokio::select! {
        biased;
        _ = interrupts.next() => None,
        finished = termination => Some(finished),
    }
}

impl Drop for InterruptSupervisor {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Run a long SYNCHRONOUS stage without starving the rest of the runtime.
///
/// `artifacts::generate` is called from an async context but does not yield for
/// as long as it runs. On a multi-threaded runtime tokio can hand this worker's
/// other tasks — the interrupt supervisor above, most of all — to another
/// thread, which is what keeps Ctrl-C observable on a single-core box where the
/// runtime has exactly one worker. Elsewhere (a `#[tokio::test]`, which is
/// current-thread by default) `block_in_place` would panic, so the stage simply
/// runs: there is no other worker to protect.
pub fn blocking_stage<T>(stage: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(stage),
        _ => stage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governor::{CancelReason, Cancelled, is_cancellation};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// Interrupts a test can deliver: a real SIGINT would be delivered to the
    /// test harness, and `abandon_run` would take the harness down with it.
    struct Scripted {
        rx: mpsc::UnboundedReceiver<Interrupt>,
        abandoned: Arc<AtomicBool>,
    }

    impl Interrupts for Scripted {
        async fn next(&mut self) -> Interrupt {
            match self.rx.recv().await {
                Some(interrupt) => interrupt,
                None => std::future::pending().await,
            }
        }

        fn abandon_run(&mut self) {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }

    /// A scripted source whose sender speaks the ordinary operator interrupt,
    /// so the pre-deadline tests read exactly as they did.
    struct OperatorSender(mpsc::UnboundedSender<Interrupt>);

    impl OperatorSender {
        fn send(&self, _unit: ()) -> std::result::Result<(), mpsc::error::SendError<Interrupt>> {
            self.0.send(Interrupt::Operator)
        }

        fn send_interrupt(
            &self,
            interrupt: Interrupt,
        ) -> std::result::Result<(), mpsc::error::SendError<Interrupt>> {
            self.0.send(interrupt)
        }
    }

    fn scripted() -> (OperatorSender, Scripted, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        (
            OperatorSender(tx),
            Scripted {
                rx,
                abandoned: Arc::clone(&abandoned),
            },
            abandoned,
        )
    }

    #[tokio::test]
    async fn an_uninterrupted_run_keeps_its_budget() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (_tx, interrupts, abandoned) = scripted();

        let value = with_cancellation(async { Ok(7) }, &governor, interrupts)
            .await
            .expect("an uninterrupted run returns its own value");

        assert_eq!(value, 7);
        assert!(!governor.is_cancelled(), "nobody asked for a cancel");
        assert!(!abandoned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_interrupt_ready_at_work_completion_cancels_the_result() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();
        tx.send(())
            .expect("prefill the interrupt before work returns");

        let error = with_cancellation(async { Ok(7) }, &governor, interrupts)
            .await
            .expect_err("a ready interrupt must win the result handoff");

        assert!(is_cancellation(&error), "{error:#}");
        assert!(governor.is_cancelled());
        assert!(!abandoned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_interrupt_ready_at_post_commit_handoff_keeps_the_result() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();
        tx.send(())
            .expect("prefill the interrupt before committed work returns");

        let value =
            with_cancellation_after_commit_if(async { Ok(7) }, &governor, interrupts, |_| true)
                .await
                .expect("a durable committed success cannot be relabelled cancelled");

        assert_eq!(value, 7);
        assert!(
            governor.is_cancelled(),
            "the late signal remains observable"
        );
        assert!(!abandoned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_uncommitted_success_remains_cancellation_sensitive() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();
        tx.send(())
            .expect("prefill the interrupt before uncommitted work returns");

        let error =
            with_cancellation_after_commit_if(async { Ok(7) }, &governor, interrupts, |_| false)
                .await
                .expect_err("an uncommitted success must not suppress cancellation");

        assert!(is_cancellation(&error), "{error:#}");
        assert!(governor.is_cancelled());
        assert!(!abandoned.load(Ordering::SeqCst));
    }

    /// The first interrupt asks the run to stop and lets it unwind — that is
    /// what removes the temporary worktrees an aborted process would leave.
    #[tokio::test]
    async fn the_first_interrupt_cancels_rather_than_aborts() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();

        let run = Arc::clone(&governor);
        let work = async move {
            tx.send(()).expect("the supervisor is listening");
            run.cancelled().await;
            Err::<(), _>(Cancelled.into())
        };

        let err = with_cancellation(work, &governor, interrupts)
            .await
            .expect_err("a cancelled run produces no value");

        assert!(is_cancellation(&err));
        assert!(governor.is_cancelled());
        assert!(
            !abandoned.load(Ordering::SeqCst),
            "one interrupt asks; it does not abandon the unwind",
        );
    }

    /// The regression this module exists for: the artifact stage is synchronous
    /// and never yields, so a supervisor sharing the run's task would not be
    /// polled for the whole of it and NEITHER interrupt would be seen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupt_is_observed_while_the_run_blocks_its_thread() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, _abandoned) = scripted();

        let run = Arc::clone(&governor);
        let work = async move {
            tx.send(()).expect("the supervisor is listening");
            // Exactly the shape of `run_context_cmds_parallel`: a poll loop that
            // sleeps the thread instead of awaiting.
            for _ in 0..200 {
                if run.is_cancelled() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(anyhow::anyhow!(
                "the interrupt was never observed by the blocking stage"
            ))
        };

        let error = with_cancellation(work, &governor, interrupts)
            .await
            .expect_err("a blocking stage must return typed cancellation");
        assert!(is_cancellation(&error), "{error:#}");
        assert!(governor.is_cancelled());
    }

    #[tokio::test]
    async fn a_second_interrupt_abandons_the_unwind() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();

        let run = Arc::clone(&governor);
        let seen = Arc::clone(&abandoned);
        let work = async move {
            tx.send(()).expect("the supervisor is listening");
            run.cancelled().await;
            // The operator refuses to wait for the cleanup this unwind is doing.
            tx.send(()).expect("the supervisor is still listening");
            for _ in 0..500 {
                if seen.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err::<(), _>(Cancelled.into())
        };

        let err = with_cancellation(work, &governor, interrupts)
            .await
            .expect_err("a cancelled run produces no value");

        assert!(is_cancellation(&err));
        assert!(
            abandoned.load(Ordering::SeqCst),
            "the second interrupt must not wait for the run to finish unwinding",
        );
    }

    #[tokio::test]
    async fn a_second_interrupt_stays_live_while_tree_termination_blocks() {
        let (tx, mut interrupts, abandoned) = scripted();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut termination = super::super::spawn_blocking_cancellation(move || {
            entered_tx
                .send(())
                .expect("test still waits for the termination worker");
            release_rx
                .recv()
                .expect("test releases the blocking termination worker");
        });
        entered_rx
            .await
            .expect("blocking termination worker must start");

        tx.send(()).expect("second interrupt receiver remains live");
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_termination_or_second_interrupt(&mut interrupts, &mut termination),
        )
        .await
        .expect("second interrupt must not wait for tree termination");
        assert!(
            outcome.is_none(),
            "the interrupt, not termination, must win"
        );
        interrupts.abandon_run();
        assert!(abandoned.load(Ordering::SeqCst));

        release_tx
            .send(())
            .expect("release the blocking termination worker");
        termination
            .await
            .expect("termination worker must finish without panic");
    }

    /// Outside a multi-threaded runtime there is no other worker to protect, so
    /// the stage runs in place rather than panicking in `block_in_place`.
    #[tokio::test]
    async fn a_blocking_stage_runs_on_a_current_thread_runtime_too() {
        assert_eq!(blocking_stage(|| 1 + 1), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blocking_stage_runs_on_a_multi_thread_runtime() {
        assert_eq!(blocking_stage(|| 1 + 1), 2);
    }

    /// One worker + a synchronous sleep is the TUI/`App::run` edge: without
    /// `block_in_place` the runtime cannot poll anything else until the stage
    /// returns, so q/Escape and the interrupt supervisor stay deaf.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_blocking_stage_keeps_a_single_worker_runtime_responsive() {
        let pinged = Arc::new(AtomicBool::new(false));
        let ping_flag = Arc::clone(&pinged);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ping_flag.store(true, Ordering::SeqCst);
        });
        tokio::task::yield_now().await;

        blocking_stage(|| {
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                pinged.load(Ordering::SeqCst),
                "single-worker runtime starved during blocking_stage"
            );
        });
    }

    /// The deadline is the second implementation of `Interrupts`, so it reaches
    /// the run through exactly the same state machine — and arrives carrying
    /// its own reason, which is what keeps it out of the operator's exit code.
    #[tokio::test]
    async fn a_deadline_cancels_the_run_with_its_own_reason() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();
        let budget = Duration::from_secs(1800);

        let run = Arc::clone(&governor);
        let work = async move {
            tx.send_interrupt(Interrupt::Deadline { budget })
                .expect("the supervisor is listening");
            run.cancelled().await;
            Err::<(), _>(Cancelled.into())
        };

        let error = with_cancellation(work, &governor, interrupts)
            .await
            .expect_err("a run stopped by its deadline produces no value");

        assert!(is_cancellation(&error), "{error:#}");
        assert_eq!(
            crate::governor::deadline_exceeded(&error),
            Some(budget),
            "the deadline must survive as a typed error: {error:#}"
        );
        assert_eq!(
            governor.cancel_reason(),
            Some(CancelReason::Deadline { budget })
        );
        assert!(
            !abandoned.load(Ordering::SeqCst),
            "a deadline asks the run to stop; it does not abandon the unwind",
        );
    }

    /// A Ctrl-C after the deadline is still the operator refusing to wait — and
    /// it must not rewrite who stopped the run.
    #[tokio::test]
    async fn an_operator_interrupt_after_a_deadline_abandons_the_unwind() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let (tx, interrupts, abandoned) = scripted();
        let budget = Duration::from_secs(60);

        let run = Arc::clone(&governor);
        let seen = Arc::clone(&abandoned);
        let work = async move {
            tx.send_interrupt(Interrupt::Deadline { budget })
                .expect("the supervisor is listening");
            run.cancelled().await;
            tx.send(()).expect("the supervisor is still listening");
            for _ in 0..500 {
                if seen.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err::<(), _>(Cancelled.into())
        };

        let error = with_cancellation(work, &governor, interrupts)
            .await
            .expect_err("a run stopped by its deadline produces no value");

        assert_eq!(crate::governor::deadline_exceeded(&error), Some(budget));
        assert_eq!(
            governor.cancel_reason(),
            Some(CancelReason::Deadline { budget }),
            "the first reason is the true one",
        );
        assert!(
            abandoned.load(Ordering::SeqCst),
            "the operator's interrupt must not wait for the unwind",
        );
    }

    /// The production source, not a script: nobody presses anything and the run
    /// still ends.
    #[tokio::test]
    async fn the_production_deadline_source_stops_a_run_nobody_interrupts() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));
        let budget = Duration::from_millis(50);

        let run = Arc::clone(&governor);
        let work = async move {
            run.cancelled().await;
            Err::<(), _>(Cancelled.into())
        };

        let error = with_cancellation(work, &governor, DeadlineOrCtrlC::new(Some(budget)))
            .await
            .expect_err("an expired run produces no value");

        assert_eq!(crate::governor::deadline_exceeded(&error), Some(budget));
    }

    /// An unbounded run is the pre-deadline behaviour exactly: the source waits
    /// for an operator who may never arrive.
    #[tokio::test]
    async fn an_unbounded_run_keeps_its_result() {
        let governor = Arc::new(ResourceGovernor::with_budget(2, 1));

        let value = with_cancellation(async { Ok(7) }, &governor, DeadlineOrCtrlC::new(None))
            .await
            .expect("an unbounded, uninterrupted run returns its own value");

        assert_eq!(value, 7);
        assert!(!governor.is_cancelled());
    }

    /// The second question the supervisor asks is "does the operator refuse to
    /// wait?". An already-fired deadline answering it would turn every expired
    /// run into an immediate `abandon_run`, skipping the unwind that removes the
    /// run's temporary worktrees.
    #[tokio::test]
    async fn a_fired_deadline_does_not_answer_the_second_interrupt_question() {
        let mut interrupts = DeadlineOrCtrlC::new(Some(Duration::from_millis(20)));

        assert_eq!(
            interrupts.next().await,
            Interrupt::Deadline {
                budget: Duration::from_millis(20)
            }
        );

        let second = tokio::time::timeout(Duration::from_millis(200), interrupts.next()).await;
        assert!(
            second.is_err(),
            "a spent deadline must not impersonate a second interrupt",
        );
    }
}
