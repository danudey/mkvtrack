//! Reading every file in the directory on background threads, so that moving
//! the cursor through the file list never waits on disk.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::mkv::MkvFile;

/// Upper bound on worker threads. The work is dominated by seeks rather than
/// by the parse, so a few in flight at once hide the latency of a slow disk
/// without swamping it.
const MAX_WORKERS: usize = 8;

pub struct Scanner {
    rx: Receiver<(usize, Result<MkvFile, String>)>,
    cancel: Arc<AtomicBool>,
    done: usize,
    total: usize,
}

impl Scanner {
    /// Starts reading `paths` in order. Results arrive out of order; each one
    /// carries the index it belongs to.
    pub fn start(paths: &[PathBuf]) -> Scanner {
        let total = paths.len();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let next = Arc::new(AtomicUsize::new(0));
        let paths: Arc<Vec<PathBuf>> = Arc::new(paths.to_vec());

        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(1, MAX_WORKERS)
            .min(total.max(1));
        for _ in 0..workers {
            let tx = tx.clone();
            let cancel = Arc::clone(&cancel);
            let next = Arc::clone(&next);
            let paths = Arc::clone(&paths);
            std::thread::spawn(move || {
                while !cancel.load(Ordering::Relaxed) {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { return };
                    if tx.send((i, MkvFile::open(path))).is_err() {
                        return; // the interface has gone away
                    }
                }
            });
        }
        Scanner {
            rx,
            cancel,
            done: 0,
            total,
        }
    }

    /// Hands every result that has arrived to `f`. Returns true when there was
    /// at least one, which is the cue to redraw.
    pub fn drain(&mut self, mut f: impl FnMut(usize, Result<MkvFile, String>)) -> bool {
        let mut any = false;
        loop {
            match self.rx.try_recv() {
                Ok((i, result)) => {
                    self.done += 1;
                    any = true;
                    f(i, result);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Every worker has finished; nothing more is coming.
                    self.done = self.total;
                    break;
                }
            }
        }
        any
    }

    pub fn in_progress(&self) -> bool {
        self.done < self.total
    }

    pub fn progress(&self) -> (usize, usize) {
        (self.done, self.total)
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        // Stop the workers before the next file rather than after all of them.
        self.cancel.store(true, Ordering::Relaxed);
    }
}
