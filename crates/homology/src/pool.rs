//! Sessions with the hosts' runners, kept between requests: reading a run
//! every few seconds should not log in every time. A session that broke is
//! replaced once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use homology_runner::protocol::{RUNNER_EXITED, RUNNER_INVALID_OUTPUT, RUNNER_IO, RUNNER_TIMEOUT};
use homology_runner::{Client, RunnerError, RunnerResult};

/// A host's place for its session: logging in and using the session happen
/// under its own lock, so a slow host holds up only its own calls.
type Slot = Arc<Mutex<Option<Client>>>;

/// One session per host, by the caller's name for the host.
#[derive(Default)]
pub struct Pool {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Pool {
    /// The place for `host`'s session (the table is locked only for this).
    fn slot(&self, host: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(slots.entry(host.to_string()).or_default())
    }

    /// Run `work` with the session to `host` (made with `connect` when there
    /// is none, or when the one there broke).
    pub fn with<T>(
        &self,
        host: &str,
        connect: &dyn Fn() -> RunnerResult<Client>,
        work: &mut dyn FnMut(&mut Client) -> RunnerResult<T>,
    ) -> RunnerResult<T> {
        let slot = self.slot(host);
        let mut session = slot.lock().unwrap_or_else(PoisonError::into_inner);
        for attempt in 0..2 {
            let client = match session.as_mut() {
                Some(client) => client,
                None => session.insert(connect()?),
            };
            match work(client) {
                // Only this session is dropped: the slot stays locked meanwhile.
                Err(error)
                    if attempt == 0
                        && [RUNNER_EXITED, RUNNER_INVALID_OUTPUT, RUNNER_TIMEOUT, RUNNER_IO]
                            .contains(&error.code.as_str()) =>
                {
                    *session = None;
                }
                outcome => return outcome,
            }
        }
        Err(RunnerError::new(RUNNER_EXITED, "the runner session broke twice"))
    }

    /// End every session not in use (the program is closing).
    pub fn close_all(&self) {
        let slots: Vec<_> = self.slots.lock().unwrap_or_else(PoisonError::into_inner).drain().collect();
        for (_, slot) in slots {
            let Ok(mut session) = slot.try_lock() else { continue };
            if let Some(client) = session.take() {
                let _ = client.close(Duration::from_secs(5));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::mpsc;

    use homology_runner::protocol::RUNNER_START_FAILED;

    use super::*;

    /// A host still logging in does not hold up another host (#32).
    #[test]
    fn a_host_logging_in_does_not_hold_up_another() {
        let pool = Arc::new(Pool::default());
        let (entered, in_login) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let slow = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || {
                let connect = || {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    Err(RunnerError::new(RUNNER_START_FAILED, "slow"))
                };
                pool.with("slow", &connect, &mut |_| Ok(())).unwrap_err().message
            })
        };
        in_login.recv().unwrap();
        let (done, fast_done) = mpsc::channel();
        {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || {
                let connect = || Err(RunnerError::new(RUNNER_START_FAILED, "fast"));
                done.send(pool.with("fast", &connect, &mut |_| Ok(())).unwrap_err().message).unwrap();
            });
        }
        let fast = fast_done.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        assert_eq!(fast.unwrap(), "fast", "the other host waited for the slow login");
        assert_eq!(slow.join().unwrap(), "slow");
    }
}
