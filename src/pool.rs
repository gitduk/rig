//! Shared worker pool for `rig install` / `rig update`. Each tool's heavy
//! work (download, build, version query) is independent, but `rig.lock`
//! writes must stay serialized — so workers only compute, and the calling
//! thread merges results as they stream in (completion order). Streaming
//! matters: merging incrementally keeps the "persist after every tool"
//! guarantee, which a collect-then-merge API would give up.

use std::sync::{Arc, Mutex, mpsc};
use std::thread;

/// Runs `job` over `items` on a pool of `workers` threads, invoking
/// `on_result` from the calling thread for each result as it arrives.
/// `job` runs concurrently and must be free of shared-state side effects —
/// return what needs applying, and apply it in `on_result`.
pub fn run_pool<T, R>(
    items: Vec<T>,
    workers: usize,
    job: impl Fn(&T) -> R + Sync,
    mut on_result: impl FnMut(T, R),
) where
    T: Send,
    R: Send,
{
    let (task_tx, task_rx) = mpsc::channel::<T>();
    let (result_tx, result_rx) = mpsc::channel::<(T, R)>();
    // Receiver isn't `Clone` — the shared task queue is wrapped so every
    // worker can pull from it; the lock is held only for the `recv` itself.
    let task_rx = Arc::new(Mutex::new(task_rx));

    thread::scope(|scope| {
        let job = &job;
        for _ in 0..workers.max(1).min(items.len()) {
            let task_rx = task_rx.clone();
            let result_tx = result_tx.clone();
            scope.spawn(move || {
                loop {
                    let item = task_rx.lock().expect("task mutex poisoned").recv();
                    match item {
                        Ok(item) => {
                            let result = job(&item);
                            let _ = result_tx.send((item, result));
                        }
                        Err(_) => break, // channel closed: no more tasks
                    }
                }
            });
        }
        for item in items {
            let _ = task_tx.send(item);
        }
        drop(task_tx);
        drop(result_tx);
        for (item, result) in result_rx {
            on_result(item, result);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_every_item_exactly_once_in_completion_order() {
        let mut seen = Vec::new();
        run_pool((0..100).collect(), 8, |i| *i, |i, r| seen.push((i, r)));
        seen.sort();
        assert_eq!(seen, (0..100).map(|i| (i, i)).collect::<Vec<_>>());
    }

    #[test]
    fn empty_input_spawns_no_workers_and_calls_nothing() {
        let mut calls = 0;
        run_pool(Vec::<u32>::new(), 4, |_| (), |_, _| calls += 1);
        assert_eq!(calls, 0);
    }

    #[test]
    fn workers_are_capped_at_item_count() {
        // Would deadlock if it spawned more workers than items while the
        // caller had no more work to send — guarded by `.min(items.len())`.
        let mut seen = Vec::new();
        run_pool(vec![1u32], 64, |i| *i, |i, r| seen.push((i, r)));
        assert_eq!(seen, vec![(1, 1)]);
    }
}
