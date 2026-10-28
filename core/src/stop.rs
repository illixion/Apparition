//! The person's Stop button: ends the sign-ins, signings, tunnels and installs under way.
//!
//! Each of those runs as one future on the runtime (`block_on`), and almost all of its time is
//! spent waiting on the network: the anisette server, Apple, the tunnel, the upload. `stoppable`
//! races it against a stop, so it ends at its next await, its connections dropped with it. What
//! runs between awaits (signing a bundle) finishes first, and a stop never undoes what the device
//! already received. Only work that started before the stop ends: the next one runs normally.

use std::{future::Future, sync::OnceLock};

use rootcause::prelude::*;
use tokio::sync::watch;

/// What a stopped call fails with; `reply` reports it with the code `stopped`.
pub const STOPPED: &str = "Stopped before it finished";

fn stops() -> &'static watch::Sender<u64> {
    static STOPS: OnceLock<watch::Sender<u64>> = OnceLock::new();
    STOPS.get_or_init(|| watch::channel(0).0)
}

/// `work`, unless a stop comes first. Only a stop sent after this call counts.
///
/// A plain function that boxes `work` straight away, not an `async fn`: the tunnel's future is
/// large, and an async fn would hold it unboxed in its own state until first polled, copying it
/// onto the caller's stack again. Swift calls in on a 512 KB concurrency thread, where those
/// copies overflowed (SIGBUS at launch).
pub fn stoppable<T>(work: impl Future<Output = Result<T, Report>>) -> impl Future<Output = Result<T, Report>> {
    let work = Box::pin(work);
    let mut stop = stops().subscribe();
    async move {
        tokio::select! {
            biased;
            _ = stop.changed() => Err(report!(STOPPED).into()),
            result = work => result,
        }
    }
}

pub fn is_stopped(error: &Report) -> bool {
    crate::errors::describe(error).contains(STOPPED)
}

/// Stops everything `stoppable` is running now.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_stop() {
    stops().send_modify(|count| *count += 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_stop_ends_work_under_way_only() {
        let waiting = tokio::spawn(stoppable(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<_, Report>(())
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        apparition_stop();
        let ended = tokio::time::timeout(Duration::from_secs(2), waiting).await.expect("ended").unwrap();
        assert!(ended.as_ref().is_err_and(is_stopped));

        // Started after the stop: runs to the end.
        assert!(stoppable(async { Ok::<_, Report>(1) }).await.is_ok());
    }

    /// The tunnel's future is large, and Swift calls in on a 512 KB concurrency thread: wrapping
    /// it mustn't put it on that stack (a launch crashed with SIGBUS in the guard page). Release
    /// only, as the device runs: unoptimised futures and frames are several times larger.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "measures optimised code: cargo test --release")]
    fn a_large_future_fits_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let large = async {
                    let buffer = [7u8; 200 * 1024];
                    tokio::task::yield_now().await;
                    Ok::<_, Report>(buffer[buffer.len() - 1])
                };
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                assert_eq!(runtime.block_on(stoppable(large)).unwrap(), 7);
            })
            .unwrap()
            .join()
            .expect("no stack overflow");
    }
}
