//! Signal-free cancellation of blocking HTTP I/O.
//!
//! Every ureq agent in Yawl is built through [`cancellable_agent`]. Its
//! connector chain ends in [`Interruptible`], which wraps the finished
//! transport. On Unix the rest of the chain is ureq's own `DefaultConnector`
//! (proxies, TLS, pooling); on Windows it is the same chain with a TCP
//! connector that never lets a receive time out (see `ready`). Reads
//! wait in short slices and, between slices, fail with
//! `io::ErrorKind::Interrupted` once the run was canceled or the provider
//! stall watch fired. No signal is involved, so this works on Windows, and on
//! Unix it backs up the `SIGUSR1` wake-up.

use std::io;
use std::time::{Duration, Instant};

use ureq::Error;
use ureq::config::Config;
use ureq::unversioned::resolver::DefaultResolver;
#[cfg(unix)]
use ureq::unversioned::transport::DefaultConnector;
use ureq::unversioned::transport::time::Duration as UreqDuration;
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};

/// Longest a single read waits before cancellation is rechecked. Bounds the
/// delay between Ctrl+C and a blocked read failing.
const SLICE: Duration = Duration::from_millis(100);

/// Builds an agent from `config` whose blocking reads honor cancellation.
#[cfg(unix)]
pub(crate) fn cancellable_agent(config: Config) -> ureq::Agent {
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default().chain(Interruptible),
        DefaultResolver::default(),
    )
}

/// Builds an agent from `config` whose blocking reads honor cancellation.
#[cfg(windows)]
pub(crate) fn cancellable_agent(config: Config) -> ureq::Agent {
    ureq::Agent::with_parts(
        config,
        super::ready::connector(),
        DefaultResolver::default(),
    )
}

/// Wraps whatever transport the rest of the chain produced.
#[derive(Debug)]
pub(super) struct Interruptible;

impl<In: Transport> Connector<In> for Interruptible {
    type Out = InterruptibleTransport;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        Ok(chained.map(|inner| InterruptibleTransport {
            inner: inner.boxed(),
            abort: Abort::default(),
        }))
    }
}

#[derive(Debug)]
pub(super) struct InterruptibleTransport {
    inner: Box<dyn Transport>,
    abort: Abort,
}

impl Transport for InterruptibleTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    /// Writes are not sliced. A write that times out after sending part of the
    /// buffer cannot report how much went out, so retrying it would corrupt
    /// the request. Cancellation is checked before the write instead.
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        if let Some(error) = self.abort.check() {
            return Err(error);
        }
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let Self { inner, abort } = self;
        await_in_slices(timeout, abort, |slice| inner.await_input(slice))
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// Reports cancellation or a provider stall as an I/O error.
///
/// The first report is `ErrorKind::Interrupted`, which the stream readers and
/// the web tools already treat as "check why". Std helpers such as
/// `read_to_string` retry `Interrupted` forever, so while the condition holds
/// every later report is a plain `Other` error that ends such loops. Ureq
/// treats that kind as a failed read, unlike `ConnectionAborted`, which it
/// would take for a clean close.
#[derive(Debug, Default)]
struct Abort {
    reported: bool,
}

impl Abort {
    fn check(&mut self) -> Option<Error> {
        if !(crate::cancellation::interrupted() || super::stalled()) {
            self.reported = false;
            return None;
        }
        let kind = if self.reported {
            io::ErrorKind::Other
        } else {
            io::ErrorKind::Interrupted
        };
        self.reported = true;
        Some(Error::Io(io::Error::new(kind, "request interrupted")))
    }
}

/// Runs `await_input` in slices of at most [`SLICE`], checking for
/// cancellation between them, while preserving ureq's own deadline: once
/// `timeout.after` has elapsed the result is `Timeout(timeout.reason)`, as if
/// the wait had never been split. A timed-out slice consumes no bytes in
/// either the TCP or the TLS transport, so the next slice resumes cleanly.
fn await_in_slices(
    timeout: NextTimeout,
    abort: &mut Abort,
    mut await_input: impl FnMut(NextTimeout) -> Result<bool, Error>,
) -> Result<bool, Error> {
    // Like ureq's own transports, treat a zero `after` as one second rather
    // than an instant timeout (`not_zero`), and no deadline at all for
    // `NotHappening` (`None`).
    let deadline = timeout
        .not_zero()
        .and_then(|after| Instant::now().checked_add(*after));
    loop {
        if let Some(error) = abort.check() {
            return Err(error);
        }
        let slice = match deadline {
            None => SLICE,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::Timeout(timeout.reason));
                }
                remaining.min(SLICE)
            }
        };
        let slice = NextTimeout {
            after: UreqDuration::Exact(slice),
            reason: timeout.reason,
        };
        match await_input(slice) {
            // The slice elapsed without data; the deadline check above decides
            // whether that was the real timeout.
            Err(Error::Timeout(_)) => {}
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use ureq::Timeout;

    use super::super::scenarios;
    use super::*;
    use crate::cancellation::{CancellationToken, scope};

    fn next(after_ms: u64) -> NextTimeout {
        NextTimeout {
            after: UreqDuration::from_millis(after_ms),
            reason: Timeout::RecvBody,
        }
    }

    #[test]
    fn slices_forward_data_and_other_errors_unchanged() {
        let mut calls = 0;
        let result = await_in_slices(next(1_000), &mut Abort::default(), |slice| {
            calls += 1;
            assert!(*slice.after <= SLICE);
            assert_eq!(slice.reason, Timeout::RecvBody);
            if calls < 3 {
                Err(Error::Timeout(slice.reason))
            } else {
                Ok(true)
            }
        });
        assert!(matches!(result, Ok(true)));
        assert_eq!(calls, 3);

        let failed = await_in_slices(next(1_000), &mut Abort::default(), |_| {
            Err(Error::ConnectionFailed)
        });
        assert!(matches!(failed, Err(Error::ConnectionFailed)));
    }

    #[test]
    fn the_original_deadline_wins_with_its_reason() {
        let started = Instant::now();
        let mut slices = 0;
        let result = await_in_slices(next(250), &mut Abort::default(), |slice| {
            slices += 1;
            std::thread::sleep(*slice.after);
            Err(Error::Timeout(slice.reason))
        });
        assert!(matches!(result, Err(Error::Timeout(Timeout::RecvBody))));
        assert!(slices >= 3, "a 250 ms wait should span several slices");
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_zero_timeout_waits_one_second_like_ureq() {
        let started = Instant::now();
        let result = await_in_slices(next(0), &mut Abort::default(), |slice| {
            std::thread::sleep(*slice.after);
            Err(Error::Timeout(slice.reason))
        });
        assert!(matches!(result, Err(Error::Timeout(Timeout::RecvBody))));
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_wait_without_a_deadline_still_slices() {
        let token = CancellationToken::default();
        let mut slices = 0;
        let result = scope(&token, || {
            await_in_slices(
                NextTimeout {
                    after: UreqDuration::NotHappening,
                    reason: Timeout::Global,
                },
                &mut Abort::default(),
                |slice| {
                    slices += 1;
                    assert_eq!(*slice.after, SLICE);
                    if slices == 2 {
                        token.cancel();
                    }
                    Err(Error::Timeout(slice.reason))
                },
            )
        });
        assert!(
            matches!(&result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(slices, 2);
    }

    // The scenarios run against the agent this platform ships.
    #[test]
    fn an_untouched_request_completes() {
        scenarios::untouched_request_completes(cancellable_agent);
    }

    #[test]
    fn canceling_the_scoped_token_interrupts_a_stalled_body_without_a_wake() {
        scenarios::token_cancellation_interrupts_a_stalled_body(cancellable_agent);
    }

    #[test]
    fn std_read_helpers_that_retry_interrupted_still_terminate() {
        scenarios::std_read_helpers_that_retry_interrupted_terminate(cancellable_agent);
    }

    #[test]
    fn the_stall_flag_interrupts_a_stalled_body() {
        scenarios::stall_flag_interrupts_a_stalled_body(cancellable_agent);
    }

    #[test]
    fn a_configured_ureq_timeout_still_fires_with_its_reason() {
        scenarios::configured_timeouts_keep_their_reasons(cancellable_agent);
    }

    #[test]
    fn pooled_connections_survive_requests_that_waited_through_slices() {
        scenarios::pooled_connections_survive_slow_requests(cancellable_agent);
    }
}
