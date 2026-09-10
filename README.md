# brz-ds

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
use brz_ds::EphemeralBytesArena;

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

## Installation

The crates.io package is `brz-ds`; the Rust library name remains `ds`:

```toml
[dependencies]
brz-ds = "0.0.3"
```

This version becomes available after the first successful Publish run.

## CI and publishing

Pushes and pull requests run formatting, Clippy (warnings are errors), tests
with all features and without default features, and the exhaustive Loom models.
`Cargo.lock` is tracked so checks use a reproducible dependency resolution.

One-time setup:

1. Log in to crates.io and verify your email. Create an API token with permission
   to publish `brz-ds`, including permission to create the crate on first publish.
   There is no separate crate creation step.
2. Store it as `CARGO_REGISTRY_TOKEN` in GitHub Actions secrets, either at
   repository level or as an organization secret granting this repository access.
   The `we-breeze` organization secret is configured for public repositories.
   A repository secret with the same name takes precedence. Never commit or
   paste the token.
3. Ensure repository policy allows the workflow's `contents: write` permission
   to push version commits to the default branch and create `v0.0.*` tags.
   Branch protection requiring PRs or tag rules can reject these pushes; those
   rules need an approved release identity/bypass or a PR-based release design.
4. Merge the workflow files into the default branch to enable the manual button.

Use **Actions → Publish → Run workflow**, select the default branch, and leave
`retry_tag` empty. The workflow chooses the next `v0.0.x` tag (currently
`v0.0.2`, since `v0.0.1` already exists), updates `Cargo.toml` and `Cargo.lock`,
then runs all CI checks and `cargo publish --dry-run`. After these pass, it
atomically pushes the version commit and annotated tag, then publishes to
crates.io. The initial Cargo version `0.1.0` is replaced by the requested
`0.0.x` version sequence. Publishing is serialized, and stale runs fail if the
default branch has advanced. Versions are derived from tags, not workflow run
numbers. No GitHub Release is created.

If the tag was pushed but the upload failed, fix the credential/network issue
and start a **new** Publish run with `retry_tag` set to that tag, e.g. `v0.0.2`.
The workflow checks out that exact tag and reruns validation before uploading;
it does not bump the version or move the tag. Only tags reachable from the
default branch with matching Cargo versions can be retried. If the version is
already on crates.io (including an upload that succeeded before a timeout), do
not retry it: crates.io versions cannot be overwritten. Source fixes require
a new release. Failures before the atomic push leave no remote release commit
or tag. The GitHub token's push does not trigger a separate CI run; Publish
runs the same checks itself.

## License

Licensed under either the MIT license or the Apache License, Version 2.0,
at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

## Crate naming

The package name is `brz-ds`; the Rust library name is `brz_ds`.
Use `brz_ds::...` in Rust code. This replaces the previous `ds`
library name. Existing explicit dependency aliases remain supported.

```toml
[dependencies]
brz-ds = "0.0.3"
```
