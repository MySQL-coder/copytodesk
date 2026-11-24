//! Cross-platform input injection abstraction (default safe DryRun-Audit; OS-specific hook points).

use ctd_protocol::{KeyEventPacket, MouseEventPacket};
use std::sync::atomic::{AtomicU64, Ordering};

pub trait InputInjector: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn total_events(&self) -> u64;
    fn inject_mouse(&self, ev: MouseEventPacket);
    fn inject_key(&self, ev: KeyEventPacket);
}

#[derive(Default)]
pub struct DryRunAuditInjector {
    count: AtomicU64,
}

impl InputInjector for DryRunAuditInjector {
    fn backend_name(&self) -> &'static str {
        "DryRun-Audit"
    }

    fn total_events(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn inject_mouse(&self, _ev: MouseEventPacket) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn inject_key(&self, _ev: KeyEventPacket) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}
