//! halos-rt — SPIKE
//!
//! Exploration scaffold for the HALOS region-oriented memory runtime
//! (HALOS-MEMORY-SPEC v0.5.0). Throwaway code to answer one question.
//!
//! Spike question: can the lock-free frame allocator and the bump arena
//! be built `#![no_std]` with no external crates, and is the arena's
//! `Relaxed` / `compare_exchange_weak` claim correct under real concurrency?
//!
//! Out of scope for this spike: the constructor, regions/teardown,
//! capabilities, manifests. Those are separate spikes.

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]
extern crate alloc;

use core::alloc::Layout;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Shared ABI newtypes. When the spikes consolidate,
/// lift 'em into a shared `halos-abi` crate rather than duplicating them.
pub mod abi {
    #[repr(transparent)]
    #[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
    pub struct Epoch(pub u64);

    #[repr(transparent)]
    #[derive(Copy, Clone, Eq, PartialEq, Debug)]
    pub struct ProcessId(pub u64);
}

pub const FRAME_SIZE: usize = 4096;

/// Physical frame number. Byte address = (number as usize) * FRAME_SIZE.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct PhysFrame(pub u64);

/// Platform virtual address.
#[repr(transparent)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub struct VirtAddr(pub usize);

/// W^X is structural: there is no Read-Write-Execute variant.
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Protection {
    ReadOnly = 0,
    ReadWrite = 1,
    ReadExecute = 2,
}

/// Identity of the executing core, for hint-slot selection.
/// Emulates the per-CPU id the kernel is given at bring-up.
fn core_id() -> usize {
    todo!("kernel bring-up: per-CPU identity (RDPID / IA32_TSC_AUX)")
}

/// Lock-free physical frame allocator: one bit per frame, per-core hints.
pub struct FrameAllocator {
    bitmap: &'static [AtomicU64], // 0 = free, 1 = allocated
    frame_count: usize, // Total valid frames
    hints: &'static [AtomicUsize],
}

impl FrameAllocator {
    pub fn new(
        // We start with a zeroed bitmap.
        bitmap: &'static [AtomicU64],
        frame_count: usize,
        hints: &'static [AtomicUsize],
    ) -> Self {
        // Nifty assertions for debugging
        assert!(!hints.is_empty(), "at least one hint slot");
        assert_eq!(
            bitmap.len(),
            frame_count.div_ceil(64),
            "bitmap sized exactly for frame count"
        );
        let tail = frame_count % 64;
        if tail != 0 {
            // Seal the tail from the last word, marking the frames as allocated.
            // Relaxed ordering only works here because the allocator is not shared (yet, working on it).
            // Todo!(test for shared ownership, multiple concurrent observers)
            bitmap[frame_count / 64].store(!0u64 << tail, Ordering::Relaxed);
        }
        Self { bitmap, frame_count, hints }
    }

    /// Claim one free frame, or `None` under physical exhaustion. Callers map 'None'
    /// to their own error domain.
    pub fn alloc_frame(&self) -> Option<PhysFrame> {
        self.alloc_frame_from(core_id())
    }

    fn alloc_frame_from(&self, core: usize) -> Option<PhysFrame> {
        let words = self.bitmap.len();
        let hint = &self.hints[core % self.hints.len()];
        // might try: let start = self.hints[words - 1].load(Ordering::Relaxed);
        let start = hint.load(Ordering::Relaxed) % words;

        // Inner CAS-claim the first free bit. On a lost race, retry on oberved value.
        for offset in 0..words {
            let i = (start + offset) % words;
            let mut word = self.bitmap[i].load(Ordering::Relaxed);

            while word != u64::MAX {
                let bit = word.trailing_ones();
                match self.bitmap[i].compare_exchange_weak(
                    word,
                    word | 1 << bit,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        hint.store(i, Ordering::Relaxed);
                        return Some(PhysFrame(i as u64 * 64 + bit as u64));
                    }
                    Err(observed) => word = observed,
                }
            }
        }
        None
}

    /// Return a frame to the free pool (clear the bit via CAS). Freed exactly
    /// once, at teardown, by the owning region.
    pub fn free_frame(&self, frame: PhysFrame) {
        debug_assert!(
            (frame.0 as usize) < self.frame_count,
        );
        let mask = 1u64 << (frame.0 % 64);
        let prev = self.bitmap[(frame.0 / 64) as usize].fetch_and(!mask, Ordering::Relaxed);
        debug_assert!(prev & mask != 0, "double free of {frame:?}");
    }
}

/// Bump allocator over one region: lock-free, allocation-only, never rewinds
/// (§4.5). Also the C heap (brk/sbrk) for ELF processes.
pub struct ProcessArena {
    base: *mut u8,
    bump: AtomicUsize,
    end: usize,
}

unsafe impl Sync for ProcessArena {}
unsafe impl Send for ProcessArena {}

impl ProcessArena {
    /// construct an arena.
    pub fn new(base: *mut u8, len: usize) -> Self {
        let base_addr = base as usize;
        let end = base_addr
            .checked_add(len)
            .expect("arena range must not wrap address space");
        Self { base, bump: AtomicUsize::new(base_addr), end }
    }

    pub fn base_addr(&self) -> usize {
        self.base as usize
    }

    /// Allocate `layout`-sized, `layout`-aligned bytes, or null on OOM.
    /// Lock-free, overflow-checked, never rewinds. This implementation is
    /// complete; the spike's job is the harness that validates it.
    #[inline]
    pub fn alloc_raw(&self, layout: Layout) -> *mut u8 {
        debug_assert!(layout.size() > 0, "zero size alloc is a caller error, maps back to the callers error domain.");
        let size = layout.size();
        let align = layout.align();
        let mut current= self.bump.load(Ordering::Relaxed);
        loop {
            let aligned = match align_up(current, align) {
                Some(a) => a,
                None => return core::ptr::null_mut(),
            };
            let next = match aligned.checked_add(size) {
                Some(n) if n <= self.end => n,
                _ => return core::ptr::null_mut(), // OOM or overflow
            };
            match self
                .bump
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return self.base.wrapping_add(aligned - self.base_addr()),
                Err(observed) => current = observed,
            }
        }
    }
}

/// `align` must be a power of two; `addr` lies inside a bounded arena, so the
/// add cannot wrap in practice.
#[inline]
const fn align_up(addr: usize, align: usize) -> Option<usize> {
    match addr.checked_add(align - 1) {
        Some(s) => Some(s & !(align - 1)),
        None => None,
    }
}


#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;
    use super::*;

    fn leaked_allocator(frame_count: usize, cores: usize) -> &'static FrameAllocator {
        let words = frame_count.div_ceil(64);
        let bitmap: &'static [AtomicU64] = Box::leak((0..words).map(|_| AtomicU64::new(0)).collect());
        let hints: &'static [AtomicUsize] = Box::leak((0..cores).map(|_| AtomicUsize::new(0)).collect());
        Box::leak(Box::new(FrameAllocator::new(bitmap, frame_count, hints)))
    }

    #[test]
    fn align_up_rounds_to_power_of_two() {
        assert_eq!(align_up(0, 8), Some(0));
        assert_eq!(align_up(1, 8), Some(8));
        assert_eq!(align_up(8, 8), Some(8));
        assert_eq!(align_up(9, 16), Some(16));
    }

    #[test]
    fn arena_hands_out_aligned_disjoint_ranges() {
        // A small heap on the host; the arena treats it as an opaque byte range.
        let mut backing = vec![0u8; 4096];
        let arena = ProcessArena::new(backing.as_mut_ptr(), backing.len());

        let a = arena.alloc_raw(Layout::from_size_align(16, 16).unwrap());
        let b = arena.alloc_raw(Layout::from_size_align(16, 16).unwrap());
        assert!(!a.is_null() && !b.is_null());
        assert_eq!(a as usize % 16, 0);
        assert_eq!(b as usize % 16, 0);
        assert_ne!(a, b);
        assert!((b as usize) >= (a as usize) + 16);
    }

    #[test]
    fn arena_returns_null_on_exhaustion() {
        let mut backing = vec![0u8; 64];
        let arena = ProcessArena::new(
            backing.as_mut_ptr(),
            backing.len()
        );
        assert!(!arena
            .alloc_raw(Layout::from_size_align(64, 1).unwrap())
            .is_null());
        // The next byte is past `end`.
        assert!(arena
            .alloc_raw(Layout::from_size_align(1, 1).unwrap())
            .is_null());
    }

    /// SPIKE TARGET — un-ignore as you implement and stress the allocator.
    /// Success: N threads hammering `alloc_raw` get pairwise-disjoint,
    /// correctly-aligned ranges, with no double-hand-out and explicit OOM at the
    /// boundary (validates the Relaxed / CAS justification).
    #[test]
    #[ignore = "spike: implement the concurrency stress and assertions"]
    fn arena_is_correct_under_concurrency() {
        // TODO: share a &arena across std::thread workers (Box::leak for 'static,
        // or scoped threads), collect every returned (ptr, len), assert pairwise
        // non-overlap and correct alignment.
    }

    /// SPIKE TARGET — frame allocator.
    /// Success: alloc all frame    #[ignore = "spike: implement alloc_frame/free_frame, then assert no double-alloc"]s with no duplicate PhysFrame; free some; re-alloc;
    /// observe reuse.
    #[test]
    fn frame_allocator_never_double_allocates() {
        // TODO: back the bitmap with a leaked 'static AtomicU64 slice.
    }
}