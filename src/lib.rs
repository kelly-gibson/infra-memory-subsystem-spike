//! halos-rt — SPIKE
//!
//! Exploration scaffold for the HALOS region-oriented memory runtime
//! (HALOS-MEMORY-SPEC v0.2.0). Throwaway code to answer one question.
//!
//! Spike question: can the lock-free frame allocator (§4.2) and the bump arena
//! (§4.5) be built `#![no_std]` with no external crates, and is the arena's
//! `Relaxed` / `compare_exchange_weak` claim correct under real concurrency?
//!
//! Out of scope for this spike: the constructor (§7), regions/teardown,
//! capabilities, manifests. Those are separate spikes.

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]
extern crate alloc;

use core::alloc::Layout;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Shared ABI newtypes (mirror of SENTINEL §5.1). When the spikes consolidate,
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

/// W^X is structural: there is no Read-Write-Execute variant (§4.4).
#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Protection {
    ReadOnly = 0,
    ReadWrite = 1,
    ReadExecute = 2,
}

/// Lock-free physical frame allocator: one bit per frame, per-core hints (§4.2).
pub struct FrameAllocator {
    bitmap: &'static [AtomicU64], // 0 = free, 1 = allocated
    frame_count: usize,
    hints: &'static [AtomicUsize],
}

impl FrameAllocator {
    pub const fn new(
        bitmap: &'static [AtomicU64],
        frame_count: usize,
        hints: &'static [AtomicUsize],
    ) -> Self {
        Self { bitmap, frame_count, hints }
    }

    /// Claim one free frame, or `None` under physical exhaustion.
    ///
    /// SPIKE TODO (§4.2): claim a bit via a single CAS on the containing word,
    /// using `trailing_ones` for the first free bit and a per-core hint to
    /// spread contention. `Relaxed` is sufficient — the bit is bookkeeping;
    /// memory validity comes from the page mapping, not this write.
    pub fn alloc_frame(&self) -> Option<PhysFrame> {
        let words = self.bitmap.len();
        // might try: let start = self.hints[words - 1].load(Ordering::Relaxed);
        let start = self.hints[core_id()].load(Ordering::Relaxed); % words;

        // The claim is a two-level loop. Outer scan over words, inner CAS retry on a word.
        // The guard in the loop makes the tail's existence pretty expensive. By the book it looks fine,
        // but i'm beginning to see why bitmap allocators are not as simple to implement in reality.
        // Putting two loops in the hot path is ehh, i'll refactor after nailing down the proof of concept
        for offset in 0..words {
            let i = (start + offset) % words;
            let mut word = self.bitmap[i].load(Ordering::Relaxed)

            loop {
                if word == u64::MAX {
                    break;
                }
            let bit = word.trailing_ones() as u64;

            let frame = i as u64 * 64 + bit;
            if frame as usize >= self.frame_count {
                break;
            }

            match self.bitmap[i].compare_exchange_weak(
                word,
                word | (1 << bit),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.hints[core_id()].store(i, Ordering::Relaxed);
                    return Some(PhysFrame(frame));
                }
                Err(actual) => word = actual,
            }
        }
    }
    // fails at physical exhaustion
    None // todo!(revise error type for resource exhaustion)
}

    /// Return a frame to the free pool (clear the bit via CAS). Freed exactly
    /// once, at teardown, by the owning region.
    pub fn free_frame(&self, _frame: PhysFrame) {
        todo!("spike: clear bit via CAS")
    }
}

/// Bump allocator over one region: lock-free, allocation-only, never rewinds
/// (§4.5). Also the C heap (brk/sbrk) for ELF processes.
pub struct ProcessArena {
    base: usize,
    bump: AtomicUsize,
    end: usize,
}

impl ProcessArena {
    /// Build an arena over the half-open byte range [base, end).
    pub const fn new(base: usize, end: usize) -> Self {
        Self { base, bump: AtomicUsize::new(base), end }
    }

    pub fn base(&self) -> usize {
        self.base
    }

    /// Allocate `layout`-sized, `layout`-aligned bytes, or null on OOM.
    /// Lock-free, overflow-checked, never rewinds. This implementation is
    /// complete; the spike's job is the harness that validates it (§4.5).
    #[inline]
    pub fn alloc_raw(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align(); // power of two, by Layout's invariant
        let mut current = self.bump.load(Ordering::Relaxed);
        loop {
            let aligned = align_up(current, align);
            let next = match aligned.checked_add(size) {
                Some(n) if n <= self.end => n,
                _ => return core::ptr::null_mut(), // OOM or overflow
            };
            match self
                .bump
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return aligned as *mut u8,
                Err(observed) => current = observed,
            }
        }
    }
}

/// `align` must be a power of two; `addr` lies inside a bounded arena, so the
/// add cannot wrap in practice.
/// todo!(prototype SIMD intrinsics for alignment and exec)
#[inline]
const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use super::*;

    #[test]
    fn align_up_rounds_to_power_of_two() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(9, 16), 16);
    }

    #[test]
    fn arena_hands_out_aligned_disjoint_ranges() {
        // A small heap on the host; the arena treats it as an opaque byte range.
        let backing = vec![0u8; 4096];
        let base = backing.as_ptr() as usize;
        let arena = ProcessArena::new(base, base + backing.len());

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
        let backing = vec![0u8; 64];
        let base = backing.as_ptr() as usize;
        let arena = ProcessArena::new(base, base + backing.len());
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
    /// boundary (validates the Relaxed / CAS justification in §4.5).
    #[test]
    #[ignore = "spike: implement the concurrency stress and assertions"]
    fn arena_is_correct_under_concurrency() {
        // TODO: share a &arena across std::thread workers (Box::leak for 'static,
        // or scoped threads), collect every returned (ptr, len), assert pairwise
        // non-overlap and correct alignment.
    }

    /// SPIKE TARGET — frame allocator.
    /// Success: alloc all frames with no duplicate PhysFrame; free some; re-alloc;
    /// observe reuse.
    #[test]
    #[ignore = "spike: implement alloc_frame/free_frame, then assert no double-alloc"]
    fn frame_allocator_never_double_allocates() {
        // TODO: back the bitmap with a leaked 'static AtomicU64 slice.
    }
}
