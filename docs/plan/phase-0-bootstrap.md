# Phase 0: Bootstrap and spikes

**Goal:** a Rust project that builds, lints and tests in CI on Ubuntu and Windows, and four short spikes that settle
the assumptions the rest of the plan depends on. The spikes' answers go into `CLAUDE.md` and, where they change the
design, into this plan.

**Size:** S · **Depends on:** nothing

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
- [ ] `%USERPROFILE%\.cargo\bin` is on the user PATH, but shells and VS Code windows opened before the install don't
      see it yet: restart VS Code once.
- [ ] Ask whether to add a Microsoft Defender exclusion for `D:\ai_workspace\Jetway\target` (faster builds; your
      call, R10).

### 0.2 Repository
- [x] The repo is cloned at `D:\ai_workspace\Jetway` with the repo-local identity `Arun Mariappan Karunanithi
      <2525449+arunmariappan@users.noreply.github.com>` (done 2026-10-25). It came with GitHub's Rust `.gitignore` and
      a one-line `README.md`.
- [ ] `LICENSE-MIT` and `LICENSE-APACHE` (D2); `license = "MIT OR Apache-2.0"` in `Cargo.toml`.
- [ ] `.gitignore` gains `data/`, `.env`, `reports/tmp/`; `.gitattributes` (`* text=auto eol=lf`); `.editorconfig`.
- [ ] `README.md`: one paragraph on what Jetway is, a "status: in development" line, a link to the plan.
- [ ] `CLAUDE.md`: commands, layout and gotchas. Start small; each phase adds to it.

### 0.3 Cargo project
- [ ] `cargo init --name jetway` as one package with a library (`src/lib.rs`) and the `jetway` binary
      (`src/main.rs`); edition 2024.
- [ ] `rust-toolchain.toml`: `channel = "1.99"`, components `rustfmt` and `clippy`, so CI and this PC use the same
      compiler.
- [ ] `Cargo.toml` `[lints]`: `unsafe_code = "forbid"`; Clippy `all` as warnings, `unwrap_used` and `expect_used`
      denied (tests allow them with `#[cfg_attr(test, allow(...))]`).
- [ ] Profiles: `[profile.dev.package."*"] opt-level = 2` (dependencies optimised, so ONNX Runtime, regex and
      compression aren't painfully slow in debug builds); `[profile.release]` with `lto = "thin"` and
      `codegen-units = 1`. Keep `panic = "unwind"`: a panic in one request's task must not stop the server.
- [ ] Add the crates of phases 1 and 2 now (README §4); later phases add theirs, so early builds stay quick.
- [ ] `jetway --version` and `jetway --help` work (clap, no real commands yet). `jetway.toml` with the `[server]`
      and `[upstream]` sections, loaded and validated by `config.rs`; `jetway check-config` prints the result.

### 0.4 CI
- [ ] `.github/workflows/ci.yml`: on push and pull request, a matrix of `ubuntu-latest` and `windows-latest`; rustup
      picks the toolchain from `rust-toolchain.toml`; `Swatinem/rust-cache`; `cargo fmt --check`,
      `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`.
- [ ] A `deny` job on Ubuntu runs `cargo-deny check` with `deny.toml`: allowed licences (MIT, Apache-2.0, BSD,
      ISC, Unicode, Zlib, MPL-2.0 and the like), advisories from RustSec, duplicate crate versions as warnings.
- [ ] One trivial test so the pipeline is green.

### 0.5 Spike S1: how Ollama streams, stops and queues (risks R1, R8, R9)
Short calls with `curl` against `http://127.0.0.1:11434`, a minute or two of GPU time in all.

- [ ] **Native stream:** `POST /api/chat` with `stream` true and false. Record the framing (one JSON object per
      line), whether a line is ever split across reads (assume yes, and test for it anyway), and the final chunk's
      fields (`done`, `done_reason`, `total_duration`, `load_duration`, `prompt_eval_count`, `prompt_eval_duration`,
      `eval_count`, `eval_duration`). Same for `/api/generate` and `/api/embed`.
- [ ] **Thinking:** the same request with `think: true`. Where does the thinking text arrive (`message.thinking`
      chunks?), and does `eval_count` include thinking tokens?
- [ ] **OpenAI-compatible stream:** `POST /v1/chat/completions` with `stream: true`, with and without
      `stream_options: {include_usage: true}`. Record the SSE framing, the `[DONE]` line, where usage appears, and
      where thinking appears (for example a `reasoning` field) when `reasoning_effort` is set.
- [ ] **Client disconnect:** stop a streaming request halfway (`curl` killed). Does Ollama stop generating at once?
      Check the server log and `ollama ps`. Jetway's cancellation design (D15) relies on it.
- [ ] **Concurrency:** two long requests at once. Do they run in parallel or one after the other? What does the
      server log say about parallel slots? Tokens per second of each. This sets `upstream.slots` (D14, R8).
- [ ] **Errors:** unknown model, malformed JSON, missing fields: status codes and bodies in both APIs.
- [ ] Save the raw responses (they hold no secrets) as golden files in `tests/fixtures/ollama/`, named by
      endpoint, stream mode and case. The fake Ollama replays them from phase 1 on.

### 0.6 Spike S2: fastembed and ONNX Runtime on Windows (risks R4, R7)
A throwaway crate in the scratchpad, not committed.

- [ ] `fastembed` with `BGESmallENV15` builds with MSVC, downloads its model and embeds text. Note where the model
      and the ONNX Runtime library end up, and set Jetway's model directory to `data/models`.
- [ ] Latency for 16, 64 and 256 tokens: p50 and p95 over 200 runs, with 1 and 4 intra-op threads; cold start;
      resident memory. Target for phase 6: p95 under 15 ms for a 64-token question.
- [ ] The same crate builds on CI's Ubuntu runner (one manual workflow run, then removed).

### 0.7 Spike S3: the injection classifier (risks R4, R5)
- [ ] List candidates that are ungated, have a permissive licence and an ONNX export, for example
      `protectai/deberta-v3-base-prompt-injection-v2` (Apache-2.0) and any smaller DeBERTa or MiniLM variant. Note
      licence, size, input length and whether an int8 version exists.
- [ ] With `ort` and `tokenizers` in the scratch crate: latency at 64, 256 and 512 tokens, resident memory, and a
      quick accuracy check on 100 rows of `deepset/prompt-injections` (Apache-2.0) plus 30 research-style questions
      about prompt injection and jailbreaks that should count as benign.
- [ ] Pick one and write it into D11, with the numbers.

### 0.8 Spike S4: OllamaSharp keeps a base path (D12)
- [ ] Read OllamaSharp 5.5.0's source (the version PaperPilot pins) to confirm it sends requests to relative paths
      (`api/chat`), so a base address `http://127.0.0.1:8300/a/paperpilot/` keeps its prefix. PaperPilot's
      `OllamaEndpoint` already adds the trailing slash. Check the Python `ollama` client the same way for OpsDesk's
      `--model-url`. Phase 5 proves both against the running gateway.

## Tests
- [ ] CI is green on the first push on both operating systems: format, Clippy, tests, `cargo-deny`.

## Done when
- [ ] `cargo run -- --help` works on Windows, and CI is green on GitHub.
- [ ] `CLAUDE.md` records the answers to S1–S4, and any change they force is written into the affected phase files.
- [ ] The Ollama golden files are committed under `tests/fixtures/ollama/`.
