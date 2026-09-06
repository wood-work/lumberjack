# Lumberjack — working agreement

## What this project is

A data acquisition (DAQ) system for both commercial hardware and our own custom DAQ
devices/products.

Repo layout:

- `lumberdaq/` — Standalone Rust library + binary. Must work
  independently of any frontend.
- `lumbergui/` — the iced interface.
- `choptui/` - the ratatui terminal user interface.

### Hardware direction

Ensure compilation can occur without hardware binaries installed, instead raising runtime errors if they aren't found. This ensures users do not need to install vendor software if they are not using those devices.


## Primary goal: I am learning Rust

This matters more than shipping fast. I am comfortable with the basics — ownership,
borrowing, `match`, traits, generics at a basic level, `serde` derives. I am *less* solid
on: lifetimes beyond the elided cases, `async`, trait objects vs enums vs generics as a
design choice, error handling beyond `Box<dyn Error>`, interior mutability, and threading.
Assume that level unless I show otherwise, and tell me when something is genuinely
advanced rather than pretending it's routine.

**If you are ever choosing between "write more code" and "explain more", choose explain.**

## How to work with me

### Discuss options and trade-offs before deciding

For anything with more than one reasonable approach — a data structure, an error strategy,
threading vs async, a crate choice — present the realistic options with their trade-offs,
**give me your recommendation and reasoning**, then let me pick. Don't give me a neutral
survey with no opinion, and don't just pick silently.

Specifically flag when a choice is hard to reverse later (public API shape, the async/sync
boundary, the storage format) versus cheap to change.

### Ask rather than assume

If my request is ambiguous about hardware behaviour, timing requirements, or how a type
should be used from a future UI — ask. Guessing wrong here costs more than a question.

## Rust conventions for this repo

- Edition 2021.
- Run `cargo check` / `cargo clippy` and report what they say. Clippy is a good teacher —
  when it fires, explain the lint rather than just silencing it.
- `cargo fmt` is fine to apply to code you touched, but don't reformat whole files you
  otherwise didn't change — it wrecks my ability to read the diff.
- Prefer standard library and well-established crates. Before adding **any** new
  dependency, tell me what it is, why, its rough weight, and whether the std alternative is
  genuinely worse.
- Comments in this codebase sometimes carry reference links (e.g. to serde docs). That's a
  pattern worth continuing for non-obvious decisions.

## Commands

From the repository root:

```powershell
cargo run -p lumberdaq -- lumberdaq/test_projects/mock_sine   # the CLI recorder
cargo run -p lumbergui -- lumberdaq/test_projects/mock_sine   # the interface
cargo test --workspace
cargo clippy --workspace --all-targets
```

Every interface takes a project directory and uses the current one when given
no argument. `mock_sine` and `scaled` need nothing plugged in; the rest are
listed in `lumberdaq/README.md`.

Windows is the primary dev platform. The `MockHardware` backend exists so the acquisition
loop can run with no hardware attached at all — keep it working.

## Things not to do

- Don't add async, a new runtime, or a new architectural layer without discussing it first.
- Don't produce a large "here's the whole feature" patch. It defeats the point.
- Don't tell me code works if you haven't run it. Say what you actually verified.
