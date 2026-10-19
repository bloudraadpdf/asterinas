# osdk-frame-allocator

This is the default buddy system frame allocator shipped with
[OSDK](https://crates.io/crates/cargo-osdk). It relies on the physical frame
metadata system in [OSTD](https://crates.io/crates/ostd) to provide a heap-free
implementation of a buddy system allocator for OS kernels. It also features
per-CPU caches and pools for scalable allocations.

Application data should use `AllocationPriority::Normal`. Kernel allocations
can fall back to an 8 MiB frame reserve when the ordinary pools are exhausted,
so the kernel can report allocation failure and reap the allocating process.
`FrameAllocOptions` defaults to `AllocationPriority::Kernel`.

This crate is part of the [Asterinas](https://github.com/asterinas/asterinas)
project.
