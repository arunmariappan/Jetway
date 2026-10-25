# Phase 0: Bootstrap and spikes

**Goal:** a Rust project that builds, lints and tests in CI on Ubuntu and Windows, and four short spikes that settle
the assumptions the rest of the plan depends on. The spikes' answers go into `CLAUDE.md` and, where they change the
design, into this plan.

**Size:** S · **Depends on:** nothing · **Status:** done (2026-10-25)

## Prerequisites

- Ollama on the Windows host with `qwen3.5:4b` pulled (Ollama 0.40.11 is installed). If Ollama isn't running, start
  the tray app detached, not from a Claude Code shell (an Ollama started from the shell dies with the session).
- No Docker needed in this phase.

## Tasks

### 0.1 Toolchain
- [x] rustup with the stable toolchain `x86_64-pc-windows-msvc`: Rust 1.99.1, Cargo 1.99.1, rustfmt, Clippy and
      rust-analyzer (installed 2026-10-25; the installer was checked against rust-lang's published SHA-256).
- [x] The MSVC linker and Windows SDK come from Visual Studio 2022 Community (MSVC 14.44, SDK 10.0.26100), already
      installed. A test program builds and runs from both PowerShell and Git Bash; Git's own `/usr/bin/link.exe` on
      Git Bash's PATH doesn't get in the way.
- [x] VS Code extensions `rust-lang.rust-analyzer` and `tamasfe.even-better-toml`.
- [x] `%USERPROFILE%\.cargo\bin` is on the user PATH; shells and VS Code windows opened before the install need a
      restart (noted in `CLAUDE.md`).
- [x] A Microsoft Defender exclusion for `D:\ai_workspace\Jetway\target` (you agreed on 2026-10-25; added through an
      elevated prompt).

### 0.2 Repository
- [x] The repo is cloned at `D:\ai_workspace\Jetway` with the repo-local identity `Arun Mariappan Karunanithi
      <2525449+arunmariappan@users.noreply.github.com>` (done 2026-10-25). It came with GitHub's Rust `.gitignore` and
      a one-line `README.md`.
- [x] `LICENSE-MIT` and `LICENSE-APACHE` (D2); `license = "MIT OR Apache-2.0"` in `Cargo.toml`.
- [x] `.gitignore` gains `data/`, `.env`, `reports/tmp/`; `.gitattributes` (`* text=auto eol=lf`, fixtures `-text`);
      `.editorconfig`.
- [x] `README.md`: one paragraph on what Jetway is, a "status: in development" line, a link to the plan.
- [x] `CLAUDE.md`: commands, layout and gotchas. Start small; each phase adds to it.

### 0.3 Cargo project
- [x] `cargo init --name jetway` as one package with a library (`src/lib.rs`) and the `jetway` binary
      (`src/main.rs`); edition 2024.
- [x] `rust-toolchain.toml`: `channel = "1.99"`, components `rustfmt` and `clippy`, so CI and this PC use the same
      compiler.
- [x] `Cargo.toml` `[lints]`: `unsafe_code = "forbid"`; Clippy `all` as warnings, `unwrap_used` and `expect_used`
      denied (`clippy.toml` allows them in tests).
- [x] Profiles: `[profile.dev.package."*"] opt-level = 2`; `[profile.release]` with `lto = "thin"` and
      `codegen-units = 1`. `panic = "unwind"` kept.
- [x] The crates of phase 1 are added. Phase 2's (SQLite, metrics, OpenTelemetry) come with phase 2, so builds
      stay quick until they are used.
- [x] `jetway --version`, `jetway --help` and `jetway check-config` work; `jetway.toml` has `[server]` and
      `[upstream]`, loaded and validated by `config.rs`.

### 0.4 CI
- [x] `.github/workflows/ci.yml`: on push and pull request, a matrix of `ubuntu-latest` and `windows-latest`;
      `rustup toolchain install` reads `rust-toolchain.toml`; `Swatinem/rust-cache`; `cargo fmt --check`,
      `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`.
- [x] A `deny` job on Ubuntu runs `cargo-deny check` with `deny.toml` (licences, RustSec advisories, duplicates as
      warnings, crate sources). `cargo deny check` passes locally (cargo-deny 0.20.2).
- [x] Tests: eight configuration tests.

### 0.5 Spike S1: how Ollama streams, stops and queues (risks R1, R8, R9)
- [x] **Native stream:** one JSON object per line, `\n`-terminated. The final line has `done: true`, `done_reason`,
      `total_duration`, `load_duration`, `prompt_eval_count`, a new `prompt_eval_cached_count`,
      `prompt_eval_duration`, `eval_count`, `eval_duration`. Same shape for `/api/generate` (`response` instead of
      `message`).
- [x] **Thinking:** `message.thinking` chunks (with `content: ""`) come before the answer's `content` chunks.
      `eval_count` includes thinking tokens (391 tokens for a three-character answer).
- [x] **OpenAI-compatible stream:** `data: {...}\n\n` events and a final `data: [DONE]`. Usage appears only with
      `stream_options.include_usage`, as an extra event with `choices: []`, `usage` and llama-server `timings`.
      Thinking arrives in `delta.reasoning` when `reasoning_effort` is set; `"none"` turns it off.
- [x] **Client disconnect:** Ollama stops at once (its log shows `srv stop: cancel task`); a short request right
      after a killed long stream started in 0.33 s against 0.20 s alone. D15's design holds.
- [x] **Concurrency:** `OLLAMA_NUM_PARALLEL:1`, `OLLAMA_MAX_QUEUE:512`. Two requests run one after the other (the
      second's first byte came when the first finished), each at about 60 tokens/s. `upstream.slots = 1` is right.
- [x] **Errors:** unknown model → 404 `{"error":"model 'x' not found"}`; malformed JSON → 400
      `{"error":"unexpected EOF"}`; OpenAI's forms are `{"error":{"message","type","param","code"}}` with types
      `not_found_error`, `invalid_request_error`, `api_error`. `qwen3.5:4b` doesn't embed: `/api/embed` and
      `/v1/embeddings` answer 501 "This server does not support embeddings".
- [x] Golden files in `tests/fixtures/ollama/` (23 files).

### 0.6 Spike S2: fastembed and ONNX Runtime on Windows (risks R4, R7)
- [x] `fastembed` 7.5 (ONNX Runtime through `ort` 2.0.0-rc.15, binaries downloaded at build time) builds with MSVC
      and embeds. Models download from Hugging Face into the given cache directory (bge-small 128 MB, MiniLM 87 MB).
- [x] Latency on the Ryzen 7 3700X, p50 / p95 in ms (fp32; batch-longest padding):

| Model, intra-op threads | ~16 tokens | ~64 tokens | ~256 tokens | Memory |
|---|---|---|---|---|
| bge-small-en-v1.5, 8 | 8.2 / 9.6 | 20.6 / 21.5 | 61.0 / 63.4 | 182 MB |
| bge-small-en-v1.5, 4 | 10.3 / 11.7 | 27.2 / 31.9 | 86.5 / 93.5 | 182 MB |
| bge-small int8, 4 | 24.8 / 28.7 | 40.1 / 43.7 | 108 / 113 | 113 MB |
| all-MiniLM-L6-v2, 4 | 5.3 / 6.4 | 11.2 / 13.2 | 38.6 / 41.0 | 141 MB |

- [x] Findings: int8 is slower on this Zen 2 CPU (no VNNI); bge-small meets 15 ms only for short questions
      (≤ ~32 tokens); MiniLM-L6 is about twice as fast. D10 and phase 6 are updated: the eval picks the model.
- [x] The CI build of fastembed is checked from phase 6 on, when it becomes a dependency, instead of a one-off
      workflow now.

### 0.7 Spike S3: the injection classifier (risks R4, R5)
- [x] Candidates (ungated, Apache-2.0, ONNX): `protectai/deberta-v3-base-prompt-injection-v2` (fp32, 739 MB),
      `Horizon-Labs/prompt-injection-guard-small` (mmBERT-small, int8, 268 MB) and
      `patronus-studio/wolf-defender-prompt-injection-small` (int8/int4, 96 MB). ProtectAI's small model is gated.
- [x] Measured with `ort` + `tokenizers`, 8 threads, on the test split of `deepset/prompt-injections` (100 rows),
      30 benign research questions about prompt injection and jailbreaks, and 10 attacks phrased as research
      questions:

| Model | 64 / 256 / 512 tokens (p50 ms) | Memory | deepset at 0.5 (recall / precision) | Benign flagged at 0.9 | Attacks caught at 0.9 |
|---|---|---|---|---|---|
| ProtectAI DeBERTa-v3-base v2 | 95 / 281 / 755 | 1 016 MB | 0.39 / 1.00 | 3 / 30 | 10 / 10 |
| Horizon Labs guard-small (int8) | 44 / 126 / 288 | 426 MB | 0.64 / 1.00 | **0 / 30** | 10 / 10 |
| wolf-defender small (int8/int4) | 41 / 116 / 261 | 263 MB | scores every input ≈ 0.98 (needs its own cascade) | 30 / 30 | 10 / 10 |

- [x] Picked **`Horizon-Labs/prompt-injection-guard-small`, int8 ONNX** (D11 updated). Its highest benign score was
      0.09. The deepset rows it misses are mostly role-play prompts ("act as an interviewer") that deepset labels as
      injections; phase 7's eval decides the threshold.

### 0.8 Spike S4: OllamaSharp keeps a base path (D12)
- [x] OllamaSharp 5.5.0 builds every request from relative paths (`Endpoints.Chat` = `"api/chat"`) against
      `HttpClient.BaseAddress`, and PaperPilot's `OllamaEndpoint` adds the trailing slash, so
      `http://127.0.0.1:8300/a/paperpilot/` keeps its prefix. Its "is Ollama running" check requests the base
      address itself, so Jetway must treat `/a/{app}/` as `/` (phase 1 updated).
- [x] The Python `ollama` client's `_parse_host` keeps a path (`example.com:56789/path` →
      `http://example.com:56789/path`) and httpx appends `/api/chat` to it, so OpsDesk's `--model-url
      http://127.0.0.1:8300/a/opsdesk` works.

## Tests
- [x] Format, Clippy and eight tests pass locally; CI runs them on both operating systems on push.

## Done when
- [x] `cargo run -- --help` works on Windows. CI green on GitHub: checked on the first push.
- [x] `CLAUDE.md` records the answers to S1–S4, and the changes they force are in D10, D11 and phases 1, 2, 6 and 7.
- [x] The Ollama golden files are committed under `tests/fixtures/ollama/`.

## What was built (2026-10-25)

- Toolchain, Defender exclusion, repo files, the Cargo package, `jetway.toml`, configuration loading and
  `check-config`, CI and `cargo-deny`, and the S1 golden files. The spike crates (embedding and classifier) lived in
  the scratchpad and are not committed; their numbers are above.
- Deviation: phase 2's crates are added in phase 2, not now. reqwest is built without TLS until phase 9 (Telegram),
  because TLS pulls in `aws-lc-sys`, a heavy C build, and the upstream is plain HTTP on 127.0.0.1.

### Rust notes
- **A package with two crates.** [src/lib.rs](../../src/lib.rs) is the library crate `jetway`;
  [src/main.rs](../../src/main.rs) is a separate binary crate that uses it like any other dependency
  (`use jetway::config::Config`). Integration tests and benches will link the library the same way, which is why the
  logic lives there and `main.rs` stays thin.
- **Deriving instead of writing code.** `#[derive(Deserialize, Serialize)]` on the config structs generates the
  parsing; `#[serde(default)]` fills missing fields from each struct's `Default` impl, and `deny_unknown_fields`
  turns typos into errors ([src/config.rs](../../src/config.rs)).
- **Errors as values.** `ConfigError` is an enum made with `thiserror`. `#[from]` lets `?` convert a figment error
  automatically. The figment error is boxed (`Box<figment::Error>`) because a `Result` is as large as its largest
  variant, and Clippy flags results that are expensive to move. `main.rs` uses `anyhow` instead, which wraps any
  error, and prints the whole chain with `{error:#}`.
- **Collecting all problems.** `validate` pushes onto a `Vec<String>` and returns `Err` only at the end, so the user
  sees every problem at once; `matches!` tests an enum variant without a full `match`.
- **Lints are configuration too.** `[lints]` in `Cargo.toml` forbids `unsafe` and denies `unwrap()`/`expect()`,
  which would panic and take the server down; `clippy.toml` relaxes that for tests, and `#[allow(...)]` relaxes one
  lint for one item with a comment saying why.
