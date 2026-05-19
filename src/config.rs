//! Live, mutable settings handle shared between the UI and background services.
//!
//! Backed by `arc_swap::ArcSwap` so reads are wait-free for hot paths like the
//! audio engine and HTTP client.

use crate::models::Settings;
use arc_swap::ArcSwap;
use std::sync::Arc;

#[derive(Clone)]
pub struct SettingsHandle {
    inner: Arc<ArcSwap<Settings>>,
}

impl SettingsHandle {
    pub fn new(initial: Settings) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(initial)),
        }
    }

    pub fn get(&self) -> Arc<Settings> {
        self.inner.load_full()
    }

    pub fn set(&self, new: Settings) {
        self.inner.store(Arc::new(new));
    }

    pub fn update<F: FnOnce(&mut Settings)>(&self, f: F) -> Arc<Settings> {
        let current = self.inner.load_full();
        let mut next = (*current).clone();
        f(&mut next);
        let next_arc = Arc::new(next);
        self.inner.store(next_arc.clone());
        next_arc
    }
}
