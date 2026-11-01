//! Lock-free history of the mono samples the output is playing, for the visualizers.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::shared::Shared;

/// History length in samples. A power of two so indices wrap with a mask.
pub(crate) const TAP_LEN: usize = 16384;
const MASK: u64 = TAP_LEN as u64 - 1;

pub(crate) struct TapBuffer {
    samples: Box<[AtomicU32]>,
    /// Absolute index of the next sample to be written.
    write_pos: AtomicU64,
    /// The sample at `anchor_pos` becomes audible at `anchor_due_ns`.
    anchor_pos: AtomicU64,
    anchor_due_ns: AtomicU64,
    live: AtomicBool,
    shared: Arc<Shared>,
}

impl TapBuffer {
    pub fn new(shared: Arc<Shared>) -> Self {
        TapBuffer {
            samples: (0..TAP_LEN).map(|_| AtomicU32::new(0)).collect(),
            write_pos: AtomicU64::new(0),
            anchor_pos: AtomicU64::new(0),
            anchor_due_ns: AtomicU64::new(0),
            live: AtomicBool::new(false),
            shared,
        }
    }

    /// Writer side (output callback only). Stores one sample at `pos`; publish with `commit`.
    #[inline]
    pub fn store(&self, pos: u64, sample: f32) {
        self.samples[(pos & MASK) as usize].store(sample.to_bits(), Ordering::Relaxed);
    }

    /// Writer side: makes samples before `end` visible. `first` (the first sample of this
    /// callback) becomes audible at `due_ns`.
    pub fn commit(&self, first: u64, end: u64, due_ns: u64, live: bool) {
        self.anchor_pos.store(first, Ordering::Relaxed);
        self.anchor_due_ns.store(due_ns, Ordering::Relaxed);
        self.live.store(live, Ordering::Relaxed);
        self.write_pos.store(end, Ordering::Release);
    }

    pub fn write_pos(&self) -> u64 {
        self.write_pos.load(Ordering::Acquire)
    }

    /// Absolute index just past the sample that is audible now.
    pub fn cursor(&self) -> u64 {
        let end = self.write_pos();
        if !self.shared.realtime {
            return end;
        }
        let anchor = self.anchor_pos.load(Ordering::Relaxed);
        let due = self.anchor_due_ns.load(Ordering::Relaxed);
        let now = self.shared.now_ns();
        let rate = u64::from(self.shared.rate);
        let audible = if now >= due {
            anchor.saturating_add((now - due) * rate / 1_000_000_000)
        } else {
            anchor.saturating_sub((due - now) * rate / 1_000_000_000)
        };
        audible.clamp(end.saturating_sub(TAP_LEN as u64 / 2), end)
    }

    /// Copies the `out.len()` samples ending just before `end`, oldest first.
    pub fn read_at(&self, end: u64, out: &mut [f32]) {
        let written = self.write_pos();
        let end = end.min(written);
        if !self.live.load(Ordering::Relaxed) {
            out.fill(0.0);
            return;
        }
        let oldest = written.saturating_sub(TAP_LEN as u64);
        let n = out.len() as u64;
        for (i, slot) in out.iter_mut().enumerate() {
            let idx = (end + i as u64).checked_sub(n);
            *slot = match idx {
                Some(idx) if idx >= oldest => {
                    f32::from_bits(self.samples[(idx & MASK) as usize].load(Ordering::Relaxed))
                }
                _ => 0.0,
            };
        }
    }

    pub fn rate(&self) -> u32 {
        self.shared.rate
    }
}
