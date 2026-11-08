//! The shared AI folder mirrored with the cloud folder while an app runs: one thread per app,
//! started once, that wakes every few minutes (or when asked, [`sync_now`]), reads the shared
//! settings again (so turning syncing on in one Craft app turns it on in the others) and runs
//! [`crate::mirror::mirror`] when syncing is on. It needs no frames from the app.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::Duration;

/// Between two runs.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
/// The first run, this long after the start.
const FIRST: Duration = Duration::from_secs(5);

struct Shared {
    /// A run was asked for.
    asked: Mutex<bool>,
    wake: Condvar,
    /// What the last run did, in words.
    report: Mutex<Option<String>>,
    running: AtomicBool,
}

fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| Shared { asked: Mutex::new(false), wake: Condvar::new(), report: Mutex::new(None), running: AtomicBool::new(false) })
}

/// Starts the thread (once per run; later calls do nothing). `local_ai`: this computer's AI
/// folder (`ArtCraft/AI`). Not in the web build, which has no files of its own.
pub fn start(local_ai: PathBuf) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if cfg!(target_arch = "wasm32") || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("craft-sync".into()).spawn(move || {
        let s = shared();
        let mut wait = FIRST;
        loop {
            {
                let asked = s.asked.lock().unwrap_or_else(PoisonError::into_inner);
                let (mut asked, _) = s.wake.wait_timeout_while(asked, wait, |a| !*a).unwrap_or_else(PoisonError::into_inner);
                *asked = false;
            }
            wait = EVERY;
            let settings = crate::Settings::load();
            let Some(root) = settings.sync_root() else { continue };
            s.running.store(true, Ordering::Release);
            let report = crate::mirror::mirror(&local_ai, &root.join("AI"), &crate::mirror::ai_files);
            *s.report.lock().unwrap_or_else(PoisonError::into_inner) = Some(report.summary());
            s.running.store(false, Ordering::Release);
        }
    });
    if spawned.is_err() {
        STARTED.store(false, Ordering::Release);
    }
}

/// Run now (when the thread is started and syncing is on).
pub fn sync_now() {
    let s = shared();
    *s.asked.lock().unwrap_or_else(PoisonError::into_inner) = true;
    s.wake.notify_all();
}

/// A run is going on.
pub fn running() -> bool {
    shared().running.load(Ordering::Acquire)
}

/// What the last run did, in words ("Sent 2 file(s)"); none before the first.
pub fn report() -> Option<String> {
    shared().report.lock().unwrap_or_else(PoisonError::into_inner).clone()
}
