# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Jetway is an LLM gateway in Rust between apps and the host Ollama: streaming pass-through for Ollama's API and the
OpenAI-compatible API, exact and semantic caches, joined in-flight requests, personal-data and prompt-injection
guards, thinking on/off routing for `qwen3.5:4b`, budgets, a priority queue and a live dashboard. **Phase 0 is in
progress.** Read [docs/plan/README.md](docs/plan/README.md) and the current phase file before working; each phase
file has tasks, tests and "done when" items to tick, and ends with a "What was built" section (with "Rust notes",
plan D20) recording decisions and what is left. Keep the plan's status line and the README in step when a phase
lands.

## Working rules

- Commit on `main`, one small conventional commit per task (`feat:`, `fix:`, `test:`, `docs:`, `chore:`, `ci:`),
  pushed at least once per phase. No attribution trailer in commit messages. The repo-local identity is
  `Arun Mariappan Karunanithi <2525449+arunmariappan@users.noreply.github.com>`.
- Rust only (plan D1): dev tools are `jetway` subcommands, not scripts. The dashboard is askama + HTMX with no
  hand-written JavaScript.
- Model calls Jetway makes itself use `qwen3.5:4b` in the host Ollama with `think` off, a JSON schema in `format`,
  validation, one retry, then an explicit `invalid` result.
- Ask the user before any GPU run longer than a few minutes: this PC has shut down during long GPU runs. Develop and
  test against `jetway fake-ollama`.
- Changes to PaperPilot or BlackBox go in their own repos under their own CLAUDE.md, after asking.
- When a decision changes, update the plan (decisions D1–D20, risks R1–R12 in docs/plan/README.md) in the same commit.
- Plan D20: each phase's "What was built" ends with "Rust notes": short explanations of the Rust ideas the phase
  used, with links to the code. The user is learning Rust with this project.

## Commands

```bash
cargo build                                   # rustup installs the pinned 1.99 toolchain on first use
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                    # never needs Ollama, a GPU or a downloaded model
cargo test config::tests::file_then_environment   # a single test
cargo test -- --ignored                       # tests that need real models (later phases)
cargo run -- check-config                     # merged configuration (defaults, jetway.toml, JETWAY__* env)
cargo deny check                              # licences, advisories, duplicates (cargo install cargo-deny)
```

## Layout

- `src/main.rs`: the clap CLI. `src/lib.rs`: modules. `src/config.rs`: figment configuration with validation.
- `jetway.toml`: committed defaults; a test checks it equals `Config::default()`, so change both together.
- `docs/plan/`: the plan. Later phases add the modules listed in plan §3.

## Gotchas

- Shells and VS Code windows opened before Rust was installed don't have `%USERPROFILE%\.cargo\bin` on PATH; in Git
  Bash prepend `$HOME/.cargo/bin`.
- `rust-toolchain.toml` pins channel `1.99`, which rustup installs as its own toolchain next to `stable`.
- reqwest has no TLS feature until phase 9 (Telegram): the upstream is plain HTTP on 127.0.0.1, and TLS pulls in a
  heavy C build (`aws-lc-sys`).
- Git here has `core.autocrlf=true`; `.gitattributes` keeps files LF. Python's `Path.write_text` writes CRLF on
  Windows; pass `newline="\n"`.
- Clippy denies `unwrap`/`expect` outside tests (`clippy.toml` allows them in tests). `figment::Jail` tests need
  `#[allow(clippy::result_large_err)]`, because Jail fixes the closure's error type.
- Microsoft Defender excludes `D:\ai_workspace\Jetway\target` (added 2026-10-25 at the user's request) for faster
  builds.
