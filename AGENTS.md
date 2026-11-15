# AGENTS.md

homeassistant-rust (`hars`) is a Rust rebuild of Home Assistant. **The #1 goal
is minimal resource usage** (RAM, CPU, allocations, idle wakeups, binary size).
Read `docs/ARCHITECTURE.md` and `docs/ROADMAP.md` before starting work.

## Commands
- Build: `cargo build` / `cargo build --release`
- Test: `cargo test --workspace`
- Lint (must pass, CI uses `-D warnings`): `cargo clippy --all-targets -- -D warnings`
- Format: `cargo fmt --all`
- Run: `cargo run --release` (binds `0.0.0.0:8123`; override with `HARS_BIND`)

## Rules
- Pick work from `docs/ROADMAP.md` and tick boxes as you finish items.
- Match Home Assistant's API/JSON shapes when implementing an API surface;
  check HA's docs/source for the exact shape rather than guessing.
- Hot paths: share data via `Arc`, avoid cloning `String`s (use `SmolStr`/`EntityId`),
  no blocking I/O on the async runtime, no busy polling.
- Claims about performance need numbers: add or run a benchmark, or measure RSS.
- Add a new crate only when a module clearly outgrows its current crate.
- `unsafe` is denied workspace-wide; justify any exception in review.
- Every behaviour change ships with a test.
- Test vectors made by running HA's code ship with the script that made
  them, in `scripts/vectors/` (see its README).
