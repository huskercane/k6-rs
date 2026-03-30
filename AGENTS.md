# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust workspace with three crates under `crates/`. `crates/k6-cli` contains the `k6-rs` binary and CLI entrypoint, `crates/k6-core` holds shared execution, metrics, config, and output logic, and `crates/k6-js` implements the QuickJS runtime plus k6-compatible JS APIs. End-to-end coverage lives in `crates/k6-js/tests/e2e.rs`. Sample scripts are in `examples/`, and CI definitions live in `.github/workflows/`.

## Build, Test, and Development Commands
Use Cargo from the repository root.

- `cargo build --workspace` builds all crates, matching CI.
- `cargo build --release` produces the optimized `k6-rs` binary in `target/release/`.
- `cargo test --workspace` runs the full test suite across unit and e2e tests.
- `cargo test -p k6-js --test e2e` runs the JS/runtime integration coverage only.
- `cargo run -p k6-rs -- run examples/simple.js` runs the CLI against a sample script.
- `cargo fmt --all` formats the workspace before review.
- `cargo clippy --workspace --all-targets -- -D warnings` catches lint issues early.

## Coding Style & Naming Conventions
The workspace uses Rust 2024 edition. Follow standard `rustfmt` formatting with 4-space indentation; do not hand-format around it. Use `snake_case` for modules, files, functions, and test names; use `CamelCase` for Rust types and traits. Keep crate boundaries clean: shared runtime-agnostic logic belongs in `k6-core`, JS bindings in `k6-js`, and CLI wiring in `k6-cli`.

## Testing Guidelines
Prefer focused unit tests next to the code under `#[cfg(test)]` and reserve `crates/k6-js/tests/e2e.rs` for runtime-level behavior. Name tests after the behavior being verified, such as `parses_hosts_mapping` or `drops_iterations_when_pool_exhausted`. Run `cargo test --workspace` before opening a PR; changes to execution, HTTP, or JS APIs should include regression coverage.

## Commit & Pull Request Guidelines
Recent history uses short, imperative commit subjects such as `Add k6-compatible CLI flags...` and `Fix formatting in dependabot.yml configuration`. Keep commits scoped and descriptive. PRs should explain the user-visible impact, note affected crates, link related issues, and include command output for verification. Screenshots are unnecessary unless output formatting or docs rendering changed.

## Configuration Notes
CI currently builds and tests with Rust `1.93` on Ubuntu. Keep local validation aligned with that toolchain when investigating CI-only failures.
