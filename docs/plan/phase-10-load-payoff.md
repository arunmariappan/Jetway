# Phase 10: Load, resilience and payoff

**Goal:** proof that Jetway keeps its promises under load and when things break, release builds anyone can download,
and a payoff report from a real session with PaperPilot and OpsDesk behind Jetway.

**Size:** M · **Depends on:** all

## Tasks

### 10.1 Load generator
- [ ] `jetway load`: open-loop (a fixed rate) or closed-loop (a fixed concurrency) load, streaming-aware. It
      measures time to first byte, gaps between chunks, total time, errors and broken streams (a stream that ends
      without its final chunk). Request mixes come from the fixtures; output is JSON plus a summary table.

### 10.2 Load targets (fake Ollama, no GPU)

| Test | Target |
|---|---|
| Added latency at 200 concurrent streams | p99 under 2 ms per request, under 1 ms per chunk |
| Soak: 30 minutes, 100 concurrent streams, 20 ms between chunks | 0 broken streams; memory flat |
| Cache hits | Over 5 000 per second on this PC |
| Jetway's memory | Stated with and without the CPU models loaded |

- [ ] Results in `reports/load.md`, with the machine and settings.

### 10.3 Resilience (automated, against the fake Ollama)
- [ ] Upstream killed mid-stream: the client gets the protocol's error line; the exchange is `upstream_error`.
- [ ] Upstream goes silent: the idle timeout ends the stream cleanly.
- [ ] A client that stops reading: memory stays bounded, and other streams are unaffected.
- [ ] 1 000 clients disconnect at once: every upstream call is dropped and the in-flight gauge returns to 0.
- [ ] The store writer stalls (simulated full disk): records are dropped and counted, requests are unaffected.
- [ ] Jetway restarted under load: in-flight streams drain within the grace period, and new connections are refused
      cleanly meanwhile.

### 10.4 Hardening
- [ ] Limits everywhere: request size, header size, number of messages, timeouts; `cargo-deny` clean; property tests
      for the decoders, the canonical key and the placeholder restorer.
- [ ] A review of every error path that ends a stream, so no panic or swallowed error can end a connection
      silently.

### 10.5 Release
- [ ] `.github/workflows/release.yml`: on a `v*` tag, build `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`
      release binaries and attach them to a GitHub release; `jetway --version` shows the git commit.

### 10.6 Payoff session (capped, agreed)
- [ ] PaperPilot and OpsDesk behind Jetway with everything on: exact and semantic cache, guards in monitor mode, the
      router for OpsDesk through `jetway/auto`, budgets and priorities (PaperPilot interactive, OpsDesk normal).
- [ ] Traffic: BlackBox's PaperPilot questions with paraphrases mixed in, and a set of OpsDesk tasks, run at a
      realistic pace in batches.
- [ ] Measured: exact and semantic hit rates, GPU-seconds saved, end-to-end latency compared with direct runs,
      Jetway's overhead, guard flags, routing decisions; quality unchanged (PaperPilot's endings and OpsDesk's
      checker results against direct runs, and BlackBox's trusted judges if available).
- [ ] Write `reports/payoff.md`.

### 10.7 Documentation
- [ ] README: what Jetway does, quick start, configuration, how to put an app behind it, the BlackBox chain,
      screenshots, the headline numbers from the reports.
- [ ] `CLAUDE.md` and this plan's status brought up to date.

## Done when
- [ ] `reports/load.md` shows the targets met, and the resilience tests run in CI.
- [ ] A tagged release has Windows and Linux binaries.
- [ ] `reports/payoff.md` is committed.
- [ ] Every item in the plan's definition of done (README §9) is ticked.
