//! Run-scoped cancellation layered on top of the process-wide interrupt.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

static WAKE_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Default)]
pub(crate) struct CancellationToken {
    canceled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub(crate) fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
    }

    pub(crate) fn clear(&self) {
        self.canceled.store(false, Ordering::Release);
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }
}

thread_local! {
    static CURRENT_TOKEN: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
}

struct ScopeGuard(Option<CancellationToken>);

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        CURRENT_TOKEN.with_borrow_mut(|current| *current = self.0.take());
    }
}

pub(crate) fn scope<T>(token: &CancellationToken, run: impl FnOnce() -> T) -> T {
    let previous = CURRENT_TOKEN.with_borrow_mut(|current| current.replace(token.clone()));
    let _guard = ScopeGuard(previous);
    run()
}

/// True when SIGINT canceled the process activity or the current
/// conversation's token was canceled.
pub(crate) fn interrupted() -> bool {
    crate::interrupted()
        || CURRENT_TOKEN
            .with_borrow(|current| current.as_ref().is_some_and(CancellationToken::is_canceled))
}

#[cfg(unix)]
pub(crate) fn native_thread_id() -> usize {
    // SAFETY: `pthread_self` takes no arguments and returns the calling
    // thread's stable pthread identifier.
    unsafe { libc::pthread_self() as usize }
}

#[cfg(unix)]
pub(crate) fn wake_thread(thread: usize) {
    if !WAKE_HANDLER_INSTALLED.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: callers retain the worker until cancellation has settled, so
    // this pthread identifier still refers to that worker. SIGUSR1 has a
    // no-op handler and exists only to interrupt a blocking system call.
    unsafe {
        libc::pthread_kill(thread as libc::pthread_t, libc::SIGUSR1);
    }
}

pub(crate) fn mark_wake_handler_installed() {
    WAKE_HANDLER_INSTALLED.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn tokens_cancel_only_the_bound_thread() {
        crate::set_interrupted(false);
        let first = CancellationToken::default();
        let second = CancellationToken::default();
        first.cancel();
        let (tx, rx) = mpsc::channel();

        let first_thread = std::thread::spawn({
            let first = first.clone();
            let tx = tx.clone();
            move || scope(&first, || tx.send(interrupted()))
        });
        let second_thread = std::thread::spawn(move || scope(&second, || tx.send(interrupted())));

        first_thread
            .join()
            .expect("first cancellation test thread")
            .expect("first cancellation state should send");
        second_thread
            .join()
            .expect("second cancellation test thread")
            .expect("second cancellation state should send");
        let mut states = [
            rx.recv().expect("first cancellation state"),
            rx.recv().expect("second cancellation state"),
        ];
        states.sort();
        assert_eq!(states, [false, true]);
    }

    #[test]
    fn a_scoped_token_kills_only_its_shell_process_group() {
        crate::set_interrupted(false);
        let token = CancellationToken::default();
        let (ready_tx, ready_rx) = mpsc::channel();
        let started = Instant::now();
        let worker = std::thread::spawn({
            let token = token.clone();
            move || {
                scope(&token, || {
                    ready_tx.send(()).expect("shell cancellation ready");
                    let mut command = Command::new("sh");
                    command.arg("-c").arg("sleep 30");
                    crate::tools::exec::run_with_timeout(command, None, Duration::from_secs(30))
                        .expect("canceled shell result")
                })
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("shell worker should start");
        token.cancel();
        let result = worker.join().expect("shell cancellation worker");

        assert!(result.interrupted);
        assert!(!result.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
