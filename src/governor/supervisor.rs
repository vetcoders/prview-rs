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

use anyhow::Result;
use colored::Colorize;

use super::{CANCELLED_EXIT_CODE, ResourceGovernor};

/// Where a run's interrupts come from.
///
/// A trait rather than a direct call to [`tokio::signal::ctrl_c`] so the
/// supervisor is testable at all: a test that raised a real SIGINT would raise
/// it at the test harness, which owns the process. Production uses [`CtrlC`];
/// a test drives the same state machine through a channel.
pub trait Interrupts: Send + 'static {
    /// Resolves on the next operator interrupt, and never when none can arrive.
    ///
    /// A handler that failed to install must not look like a signal that
    /// arrived — that would cancel a healthy run the moment it started.
    fn next(&mut self) -> impl Future<Output = ()> + Send;

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
    async fn next(&mut self) {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
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
    // The guard stops the supervisor whichever way `work` leaves — a value, an
    // error or a panic — so a task listening for a signal never outlives the run
    // it was listening on behalf of.
    let _supervisor = Supervisor(tokio::spawn(supervise(Arc::clone(governor), interrupts)));
    work.await
}

/// Turn the first interrupt into a cancel and the second into an exit.
async fn supervise(governor: Arc<ResourceGovernor>, mut interrupts: impl Interrupts) {
    interrupts.next().await;
    eprintln!(
        "\n{} stopping running tools and cleaning up (Ctrl-C again to exit now)",
        "^C".yellow().bold(),
    );
    governor.cancel();

    interrupts.next().await;
    eprintln!("{} second interrupt — exiting now", "^C".yellow().bold());
    interrupts.abandon_run();
}

struct Supervisor(tokio::task::JoinHandle<()>);

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.0.abort();
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
    use crate::governor::{Cancelled, is_cancellation};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// Interrupts a test can deliver: a real SIGINT would be delivered to the
    /// test harness, and `abandon_run` would take the harness down with it.
    struct Scripted {
        rx: mpsc::UnboundedReceiver<()>,
        abandoned: Arc<AtomicBool>,
    }

    impl Interrupts for Scripted {
        async fn next(&mut self) {
            if self.rx.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }

        fn abandon_run(&mut self) {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }

    fn scripted() -> (mpsc::UnboundedSender<()>, Scripted, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        (
            tx,
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

        with_cancellation(work, &governor, interrupts)
            .await
            .expect("a blocking stage must still see the cancel");
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
}
