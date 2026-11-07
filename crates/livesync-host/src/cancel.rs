//! Cancelling a call in flight (a folder listing nobody waits for).

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Debug, Default)]
pub struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop the call this was handed to (now or when it starts).
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once [`Cancel::cancel`] was called, before or after.
    pub async fn cancelled(&self) {
        loop {
            // Registered before the check, so a cancel in between still wakes it.
            let woken = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            woken.await;
        }
    }
}
