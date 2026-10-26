# Phase 5: real clients behind Jetway (partial)

Measured on 2026-10-26 on the development PC (Ryzen 7 3700X, RX 5700 XT 8 GB, 16 GB RAM), Ollama 0.40.11 with
`qwen3.5:4b`, Jetway built from `59520b0` (release). The session stopped early: **the PC shut down unexpectedly about
15 minutes into the GPU work** (see the end). The parts that need a second pass are still open.

## What works

| Client | How it reaches Jetway | Result |
|---|---|---|
| PaperPilot (.NET, OllamaSharp 5.5.0) | `ConnectionStrings__ollama=Endpoint=http://127.0.0.1:8300/a/paperpilot/` (an environment override for the run; no code or user-secret change) | Health checks, `/ask`, `/stream` and `/ask-agentic` all go through Jetway as app `paperpilot` |
| OpsDesk (Python `ollama` client), recorded by BlackBox | Agent → BlackBox proxy `:8210` → Jetway `/a/opsdesk` → Ollama | Three tasks finished (17–18 steps, 23–36 s each) |
| The `ollama` CLI | `OLLAMA_HOST=127.0.0.1:8300` | `run`, `list`, `ps`, cached repeats (phases 1 and 3) |

- **BlackBox in front of Jetway** (plan D16): a recorded OpsDesk run replayed **exactly, with 0 live calls and identical
  output**, and Jetway saw no new exchanges during the replay (29 before, 29 after).
- **Spans:** with `otel.endpoint` pointing at BlackBox, Jetway's server and client spans joined the OpsDesk run's trace
  (57 spans), with Jetway's server span under the agent's HTTP client span; BlackBox still built the run's steps.

## PaperPilot's request shapes

Recorded from Jetway's own log and saved in `tests/fixtures/paperpilot/`:

| Step | Endpoint | Stream | `format` | Temperature | Where the question sits |
|---|---|---|---|---|---|
| Guardrail | `/api/chat`, one user message | no | JSON schema (`score`, `reason`) | 0 | `User Query: {question}` |
| Grade documents | `/api/chat`, one user message (~13 KB with chunks) | no | JSON schema (`binary_score`, `reasoning`) | 0 | `User Question: {question}` after the documents |
| Rewrite | `/api/chat`, one user message | no | JSON schema (`rewritten_query`, ...) | 0.3 | `Here is the initial question:\n{question}` |
| RAG answer (`/ask`) | `/api/chat`, system + user | no | none | 0.7, `top_p` 0.9 | `### Question:\n{question}\n\n### Answer:` |
| RAG answer (`/stream`) | same | yes | none | 0.7, `top_p` 0.9 | same |

Every call sets `think: false`; OllamaSharp also sends an empty `CustomHeaders` field in the body (harmless; part of
the cache key). The structured steps are deterministic (temperature 0) except the rewrite.

## Measurements so far

First pass of the agentic endpoint for the 20 out-of-scope and not-in-corpus questions of BlackBox's question set:

| Measure | Value |
|---|---|
| Model calls through Jetway | 47 (`/api/chat`) |
| Jetway's time before the upstream call | 315 µs mean, 705 µs max (includes parsing 13 KB prompts and the cache lookup) |
| Share of the upstream calls' time | **0.0027%** (target: under 1%) |
| Exact-cache hits on a first pass | 2 (a guardrail call for a question asked earlier) |
| Endings | Out of scope: 10/10 answered by the guardrail in ~1.2 s; not in corpus: 9/10 stopped after 2 retrieval attempts, 1 judged out of scope |

### A runaway generation

One rewrite call (101 prompt tokens, temperature 0.3, JSON schema) generated **16 283 tokens until the context limit**
(`done_reason: length`), taking 284 s of GPU for a question that otherwise takes 20–30 s. PaperPilot sets no output
cap on its structured steps. Jetway recorded it accurately, and it led to a fix in Jetway (`59520b0`): an answer cut
off by the context limit is no longer cached unless the client capped the output itself. It is also exactly what
phase 9's budgets and an output-token cap per app are for.

## Not done yet

- The second pass of the agentic endpoint (cache hits on the agentic steps, endings unchanged) and both passes of the
  streaming endpoint.
- The answerable questions (the first seven had run when the PC shut down).

## The shutdown

- At about 09:45 IST, with PaperPilot's agentic runs generating on the GPU (the first pass over the answerable
  questions), the PC shut down without warning. Windows logged event 6008 on the next boot at 10:10, with a stale
  "last alive" time of 9:14. Jetway's log ends at 09:45:16 in the middle of that batch.
- It is the fourth unexpected shutdown since 2026-10-04 (three during the model benchmark that day). Load at the time:
  Ollama on the GPU, Docker (PaperPilot's Postgres, Redis, OpenSearch), about 1–3 GB of RAM free, Ollama's
  `llama-server` holding 5.1 GB of RAM after an hour.
- GPU work for Jetway is paused until the user decides how to continue.
