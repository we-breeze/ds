# ds

Shared low-level data structures used by Breeze components.

The crate intentionally keeps a small surface:

- `BrzMalloc` and `heap()` for the existing mimalloc-backed heap accounting;
- `EphemeralBytesArena` for short-lived encoded request frames.

## Ephemeral bytes

The arena owns one backing buffer divided into two fixed-size bump chunks. Each
frame reserves exactly its requested capacity. A chunk is frozen after a
request no longer fits, and becomes reusable as a whole once every frame issued
from it has been dropped. Release order does not matter. If neither chunk can
serve a request, allocation falls back to the heap without waiting.

This crate provides the allocation mechanism only. The process-wide request
arena and its startup policy are owned by the `net` crate so Redis, MC, and
Motan can share one instance.

```rust
use ds::EphemeralBytesArena;

let arena = EphemeralBytesArena::new(4 * 1024 * 1024);
let mut frame = arena.alloc(9);
frame.extend_from_slice(b"GET key\r\n");
let frame = frame.freeze();

assert_eq!(frame.as_ref(), b"GET key\r\n");
```

## Verification

Run the normal tests and the exhaustive Loom models separately:

```bash
cargo test --all-features
RUSTFLAGS="--cfg loom -C debug-assertions" \
  cargo test --release loom_ -- --test-threads=1
```

The Loom models use one- and two-byte logical chunks to enumerate races between
reservation, freezing, last release, reset, stale reservers, and out-of-order
release without expanding the model with the physical byte storage.
