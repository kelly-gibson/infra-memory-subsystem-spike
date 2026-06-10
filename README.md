# halos-rt — spike

Exploration/prototyping spike for the HALOS region-oriented memory runtime
(HALOS-MEMORY-SPEC v0.5.0). **Throwaway code**, the resulting ADR will guide the 
actual production deployment. Once the kernel ABI surface is stabilized, all the
shared newtypes will live in a separate crate.

## Spike question
Can the lock-free frame allocator and the bump arena be built
`#![no_std]` with no external crates, and is the arena's `Relaxed`-ordering /
`compare_exchange_weak` sufficient under real concurrency (pairwise-disjoint,
correctly-aligned allocations, explicit OOM, never aliasing memory)?

## Why it matters
The arena is the primary allocator primitive, and it is where process heap objects live. The frame
allocator sits beneath every region. Both allocators aim to be lock-free by mechanism, but until this 
is proven, I can't finish writing the spec. This spike serves to validate these allocators before I move onto
building the constructor on top.

## What "answered" looks like
- `arena_is_correct_under_concurrency` passes under a thread-stress harness.
- `frame_allocator_never_double_allocates` passes.
- The `Relaxed`-ordering justification is confirmed or corrected in an ADR that
  feeds the production `frame-alloc` / `arena` work.

## Scope
- In: `FrameAllocator`, `ProcessArena`, `Protection`, the host test harness.
- Out (separate spikes): the constructor and atomic rollback, regions and
  teardown, capabilities, manifests.

## Run
```
cargo test                 # arena smoke tests run; spike targets are #[ignore]
cargo test -- --ignored    # run the spike targets as you implement them
```

## Status
Phase: Exploration / Prototyping. The code is exploratory and expected to be
discarded once the question is answered; the durable output is the ADR.
