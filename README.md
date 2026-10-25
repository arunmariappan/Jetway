# Jetway

Jetway is an LLM gateway: a proxy between your apps and a local Ollama that makes every model call cheaper, faster
and safer. Apps point at Jetway instead of Ollama and change nothing else. It streams responses through as they
arrive, answers repeated and paraphrased questions from caches, joins identical requests in flight, flags or blocks
prompt injection and personal data, decides when the model's thinking mode is worth it, enforces per-app budgets and
priorities, and shows it all on a live dashboard. One Rust binary; it speaks Ollama's API and the OpenAI-compatible
API.

> **Status:** in development. Phases 0 (bootstrap) and 1 (streaming pass-through) are done; phase 2 (request log) is next. See the
> [implementation plan](docs/plan/README.md).

## Building

Rust 1.99 (pinned in `rust-toolchain.toml`; rustup installs it on first use). On Windows, the MSVC build tools from
Visual Studio are needed.

```bash
cargo build
cargo test
cargo run -- check-config     # prints the merged configuration
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
