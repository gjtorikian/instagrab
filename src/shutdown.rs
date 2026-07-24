//! SIGINT/SIGTERM handling. A global flag is checked at operation boundaries.

use std::process;
use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn install() {
    let _ = ctrlc::set_handler(|| {
        // Second signal force-quits (128 + SIGINT), like an unhandled ^C.
        if SHUTDOWN.swap(true, Ordering::SeqCst) {
            process::exit(130);
        }
    });
}

pub fn requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}
