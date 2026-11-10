//! Sessions with the hosts' runners, kept between requests: reading a run
//! every few seconds should not log in every time. A session that broke is
//! replaced once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use homology_runner::protocol::{RUNNER_EXITED, RUNNER_INVALID_OUTPUT, RUNNER_IO, RUNNER_TIMEOUT};
use homology_runner::{Client, RunnerError, RunnerResult};

/// One session per host, by the caller's name for the host.
#[derive(Default)]
pub struct Pool {
    sessions: Mutex<HashMap<String, Arc<Mutex<Client>>>>,
}

impl Pool {
    /// Run `work` with the session to `host` (made with `connect` when there
    /// is none, or when the one there broke).
    pub fn with<T>(
        &self,
        host: &str,
        connect: &dyn Fn() -> RunnerResult<Client>,
        work: &mut dyn FnMut(&mut Client) -> RunnerResult<T>,
    ) -> RunnerResult<T> {
        for attempt in 0..2 {
            let session = {
                let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
                match sessions.get(host) {
                    Some(session) => Arc::clone(session),
                    None => {
                        let session = Arc::new(Mutex::new(connect()?));
                        sessions.insert(host.to_string(), Arc::clone(&session));
                        session
                    }
                }
            };
            let outcome = work(&mut session.lock().unwrap_or_else(PoisonError::into_inner));
            match outcome {
                Err(error)
                    if attempt == 0
                        && [RUNNER_EXITED, RUNNER_INVALID_OUTPUT, RUNNER_TIMEOUT, RUNNER_IO]
                            .contains(&error.code.as_str()) =>
                {
                    self.sessions.lock().unwrap_or_else(PoisonError::into_inner).remove(host);
                }
                outcome => return outcome,
            }
        }
        Err(RunnerError::new(RUNNER_EXITED, "the runner session broke twice"))
    }

    /// End every session (the program is closing).
    pub fn close_all(&self) {
        let sessions: Vec<_> = self.sessions.lock().unwrap_or_else(PoisonError::into_inner).drain().collect();
        for (_, session) in sessions {
            if let Ok(session) = Arc::try_unwrap(session) {
                let _ = session.into_inner().unwrap_or_else(PoisonError::into_inner).close(Duration::from_secs(5));
            }
        }
    }
}
