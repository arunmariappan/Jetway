# Phase 5: Real clients

**Goal:** PaperPilot, OpsDesk and the `ollama` CLI run through Jetway with no code changes, BlackBox can record an
agent that runs behind Jetway, and a first report measures Jetway's overhead and exact-cache hits on real traffic.
The real request shapes recorded here feed the semantic cache's templates in phase 6.

**Size:** S · **Depends on:** 4

## Prerequisites

- Docker Desktop started, then PaperPilot's stack: `aspire start --apphost
  D:\ai_workspace\PaperPilot\src\PaperPilot.AppHost` from PowerShell. Keep Langfuse off (memory, R7).
- Ollama with `qwen3.5:4b`. Every run in this phase is short and capped, and agreed with you before it starts.

## Tasks

### 5.1 PaperPilot through Jetway, with no code change
- [ ] Point PaperPilot's Ollama connection at Jetway for the session with a user secret:
      `dotnet user-secrets set ConnectionStrings:ollama "Endpoint=http://127.0.0.1:8300/a/paperpilot/" --project
      src/PaperPilot.AppHost`, and remove it afterwards with `dotnet user-secrets remove ConnectionStrings:ollama
      --project src/PaperPilot.AppHost`.
- [ ] PaperPilot's default model stays `qwen3.5:9b`; requests in this phase pass `"model": "qwen3.5:4b"`, as
      BlackBox's runs do.
- [ ] Confirm spike S4: every PaperPilot call arrives as `/a/paperpilot/api/...` and is recorded under app
      `paperpilot`. If the prefix were lost, the fallback is a one-line opt-in `X-Jetway-App` header in PaperPilot's
      Ollama `HttpClient`, committed in the PaperPilot repo after asking.

### 5.2 PaperPilot's request shapes
- [ ] Run `/api/v1/ask`, `/api/v1/ask-stream` (or PaperPilot's streaming endpoint) and `/api/v1/ask-agentic` once
      each, in scope and out of scope. From Jetway's records, note for every kind of call (guardrail, rewrite,
      grading, generation, plain RAG answer): endpoint, `stream`, `format` (a JSON schema?), `think`, options, and the
      message layout.
- [ ] Save them as fixtures in `tests/fixtures/paperpilot/` (paper text is public; no secrets are in the bodies, and
      the test from phase 2 checks that no `Authorization` header was stored). Add them to the fake Ollama's scripts
      and to the cache-key tests.
- [ ] Expected layout, from PaperPilot's prompt builder: RAG answers have a system prompt and one user message
      `### Context from Papers: ... ### Question:\n{query}\n\n### Answer: ...`; the guardrail prompt holds
      `User Query: {question}`. Phase 6 builds its templates from what is actually recorded here.

### 5.3 OpsDesk and the `ollama` CLI
- [ ] OpsDesk (in the BlackBox repo): `uv run opsdesk env`, then `uv run opsdesk agent --model-url
      http://127.0.0.1:8300/a/opsdesk`; run three tasks with `uv run blackbox run opsdesk --task <id>`. Its calls are
      tool-calling `/api/chat` requests.
- [ ] `ollama run`, `ollama list` and `ollama ps` with `OLLAMA_HOST=127.0.0.1:8300` (app `default`).

### 5.4 BlackBox in the chain (D16, R12)
- [ ] Agent → BlackBox proxy (8210) → Jetway (8300) → Ollama: set BlackBox's `ollama` upstream target to
      `http://127.0.0.1:8300/a/opsdesk` (BlackBox appends the request path to its target). Record one OpsDesk run and
      replay it with `blackbox replay <run>`: the replay must be exact and must not reach Jetway.
- [ ] With `otel.endpoint` pointing at BlackBox, Jetway's spans join the run's trace; check that BlackBox still
      builds the run's steps correctly with the extra spans present.
- [ ] Write the chain and its settings into `CLAUDE.md` and the README.

### 5.5 First measurement (capped, agreed)
- [ ] Questions: the out-of-scope and not-in-corpus groups of BlackBox's `datasets/paperpilot/questions.yaml` plus
      ten answerable questions written from PaperPilot's indexed papers, each asked twice through
      `/api/v1/ask-agentic`, then twice through the streaming endpoint.
- [ ] Measure: Jetway's added time as a share of end-to-end time (target under 1%); exact-cache hits on the second
      pass, by kind of call; PaperPilot's endings unchanged against the first pass.
- [ ] Expect PaperPilot's own answer cache (Redis) to answer repeated plain `/ask` questions before they reach
      Jetway; Jetway's exact hits come from the agentic steps (guardrail, rewrite, grading) and from other clients.
      The real gain for PaperPilot is paraphrases, which is phase 6.
- [ ] Write `reports/phase-5-real-clients.md`.

## Tests
- [ ] The PaperPilot fixtures run through the fake Ollama in CI: their cache keys, records and served hits.

## Done when
- [ ] PaperPilot, OpsDesk and the `ollama` CLI work through Jetway, streaming included, with no code changes.
- [ ] A BlackBox recording made through Jetway replays exactly.
- [ ] The report is committed, and the temporary PaperPilot user secret is removed.
