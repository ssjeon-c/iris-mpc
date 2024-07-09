//! Long-running async task monitoring.

use std::{
    ops::{Deref, DerefMut},
    panic,
};
use tokio::task::{JoinError, JoinSet};

/// A long-running async task monitor which checks all its tasks for panics or
/// hangs when dropped. Designed for ongoing tasks which run until the program
/// exits.
///
/// Create with `new()`, and monitor for panics or exits regularly with
/// `check_tasks()`.
///
/// When exiting the program, `abort_all()`, wait, then check for hangs with
/// `check_tasks_finished()`.
#[derive(Debug, Default)]
pub struct TaskMonitor {
    pub tasks: JoinSet<()>,
}

// Instead of writing trivial wrappers for all the useful JoinSet methods, we
// can just Deref to the inner JoinSet.
impl Deref for TaskMonitor {
    type Target = JoinSet<()>;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

impl DerefMut for TaskMonitor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tasks
    }
}

impl Drop for TaskMonitor {
    // As a last-ditch effort, check for hangs or panics before the program exits.
    fn drop(&mut self) {
        // When the program exits or the task set is dropped, we can't check for
        // cancellations and early exits, because other drops might have already
        // cancelled or finished tasks.
        self.tasks.abort_all();

        // Check for hangs and panics.
        //
        // If there is a hang (or hang panic) here, try calling abort_all() and waiting
        // before dropping the TaskMonitor. Or call
        // `check_tasks_finished_ignoring_hangs()` here instead.
        self.check_tasks_finished();
    }
}

impl TaskMonitor {
    /// Create a new task monitor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Panics if any of the monitored tasks have finished normally, were
    /// cancelled, or panicked. This function panics even if a task finishes
    /// without an error.
    ///
    /// Call this method after adding each new task, and before starting a new
    /// batch or long-running operation.
    pub fn check_tasks(&mut self) {
        // Any finished task is an error, so we just need to check for the first one.
        if let Some(finished_task) = self.tasks.try_join_next() {
            finished_task.expect("Monitored task was panicked or cancelled");
            panic!("Monitored task unexpectedly finished without an error");
        }
    }

    /// Checks for panics, cancellations, or early finishes, then aborts all
    /// tasks.
    ///
    /// # Panics
    ///
    /// If there is already a task panic, cancellation, or early finish.
    pub fn abort_all(&mut self) {
        self.check_tasks();

        self.tasks.abort_all();
    }

    /// Panics if any of the `server_tasks` have finished with a panic or hang.
    /// (Ignores tasks that have finished normally or were cancelled).
    ///
    /// When exiting the program, call `abort_all()`, wait for the tasks to
    /// finish, then call this function.
    pub fn check_tasks_finished(&mut self) {
        // Any hung task is an error, so we need to check they've all finished.
        while let Some(finished_task) = self.tasks.try_join_next() {
            // If there is a hang (or hang panic) here, try calling abort_all() and waiting
            // before dropping the TaskMonitor.
            TaskMonitor::resume_panic(finished_task);
        }

        if !self.tasks.is_empty() {
            // If this panics, try waiting for longer between the abort and this function
            // call.
            panic!(
                "{} monitored tasks hung even when aborted",
                self.tasks.len()
            );
        }
    }

    /// Like `check_tasks_finished()`, but ignores hangs.
    pub fn check_tasks_finished_ignoring_hangs(&mut self) {
        // Any hung task is an error, so we need to check they've all finished.
        while let Some(finished_task) = self.tasks.try_join_next() {
            // If there is a hang (or hang panic) here, try calling abort_all() and waiting
            // before dropping the TaskMonitor.
            TaskMonitor::resume_panic(finished_task);
        }
    }

    /// Panics if any of the `server_tasks` have finished with a panic.
    /// (Ignores tasks that have finished normally).
    ///
    /// When exiting the program, call `abort_all()`, wait for the tasks to
    /// finish, then call this function.
    ///
    /// This function can't detect hangs: it hangs if any task does not finish
    /// when aborted.
    pub async fn abort_and_wait_for_finish(&mut self) {
        self.abort_all();

        // Any hung task is an error, so we need to check they've all finished.
        while let Some(finished_task) = self.tasks.join_next().await {
            TaskMonitor::resume_panic(finished_task);
        }

        assert!(self.tasks.is_empty());
    }

    /// If `err` is a task panic, resume that panic.
    #[track_caller]
    pub fn resume_panic(err: Result<(), JoinError>) {
        if let Err(err) = err {
            if !err.is_cancelled() {
                panic::resume_unwind(err.into_panic());
            }
        }
    }
}