//! Keeping the long-lived tasks alive.
//!
//! The server runs two loops for its whole life: the sweeper, which removes
//! torrents nobody is watching, and the history writer, which puts everyone's
//! position on disk. Both were spawned and forgotten.
//!
//! A panic in either was absorbed by the runtime and reported nowhere. The
//! sweeper simply stopped sweeping, for ever, and the first anyone would know
//! is a disk filling up over a fortnight; the writer simply stopped writing,
//! and the first anyone would know is losing their place. Neither produces a
//! symptom anybody would connect to a panic that happened a week earlier.
//!
//! So they are supervised: a panic is logged loudly and the loop is started
//! again. Restarting is right for these two because both are idempotent by
//! construction, and because a sweeper that has died is strictly worse than one
//! that panics occasionally.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;

/// How long to wait before starting a loop again.
///
/// Long enough that a task panicking on every pass cannot spin the machine,
/// short enough that a transient failure costs one cycle. It is a supervisor,
/// not a retry policy: anything that needs finer control should handle its own
/// errors.
const RESTART_AFTER: Duration = Duration::from_secs(5);

/// Run a loop for the life of the process, restarting it if it panics.
///
/// `make` is called again for each attempt, so the future may capture whatever
/// it needs by cloning it inside the closure.
pub fn forever<F, Fut>(name: &'static str, make: F) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            // Spawned rather than awaited directly, because that is what turns
            // a panic into a value we can look at instead of one that unwinds
            // through this supervisor as well.
            let attempt = tokio::spawn(make());
            match attempt.await {
                Ok(()) => {
                    // Returned cleanly. These loops are not supposed to, so it
                    // is worth saying, but it is a decision rather than a
                    // failure and is not second-guessed.
                    tracing::info!(task = name, "background task finished");
                    return;
                }
                Err(err) if err.is_cancelled() => {
                    tracing::debug!(task = name, "background task cancelled");
                    return;
                }
                Err(err) => {
                    tracing::error!(
                        task = name,
                        panic = %panic_message(&err),
                        "background task panicked; starting it again"
                    );
                    tokio::time::sleep(RESTART_AFTER).await;
                }
            }
        }
    })
}

/// Get something readable out of a panic payload.
///
/// A panic carries a `Box<dyn Any>`, which is almost always a `&str` or a
/// `String` and prints as neither without this.
fn panic_message(err: &tokio::task::JoinError) -> String {
    if !err.is_panic() {
        return err.to_string();
    }
    // `into_panic` consumes, and we only have a reference, so the message is
    // recovered from the Display impl, which includes it.
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn a_task_that_panics_is_started_again() {
        let attempts = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&attempts);

        let handle = forever("test", move || {
            let counted = Arc::clone(&counted);
            async move {
                let attempt = counted.fetch_add(1, Ordering::SeqCst);
                if attempt < 3 {
                    panic!("deliberately");
                }
                // Fourth time, sit still rather than returning, which is what a
                // real loop does.
                std::future::pending::<()>().await;
            }
        });

        // Time is paused, so the restart waits pass instantly.
        for _ in 0..4 {
            tokio::time::sleep(RESTART_AFTER * 2).await;
        }
        assert!(
            attempts.load(Ordering::SeqCst) >= 4,
            "started {} times",
            attempts.load(Ordering::SeqCst)
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_finishes_is_left_finished() {
        // Restarting something that returned on purpose would be a supervisor
        // arguing with the thing it supervises.
        let attempts = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&attempts);

        let handle = forever("test", move || {
            let counted = Arc::clone(&counted);
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });

        handle.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn aborting_the_supervisor_stops_it_rather_than_restarting() {
        let handle = forever("test", || async {
            std::future::pending::<()>().await;
        });
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
    }
}
