This is oxbow, a secure-minimal capability microkernel written from scratch
in Rust (x86_64, Limine). Zero ambient authority: every right is a capability.
It has a userland network stack (e1000 + smoltcp TCP), a slab heap, SSE/FPU
support, and runs real crypto. Browse the source under /usr/src/oxbow.
