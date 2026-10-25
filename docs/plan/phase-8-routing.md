# Phase 8: Routing thinking on or off

**Goal:** requests that opt in are answered by `qwen3.5:4b` with thinking on only when it is likely to help, so hard
questions get the better answer and easy ones stay fast. The router is trained and evaluated on questions with
checkable answers, and its report shows accuracy against latency for always-off, always-on and routed (D6).

**Size:** M · **Depends on:** 6

## Tasks

### 8.1 Opting in
- [ ] **Virtual model:** a request naming `jetway/auto` (`models.virtual`) is routed. Jetway lists it in
      `/api/tags` and `/v1/models`, and answers `/api/show` for it with `qwen3.5:4b`'s details under the virtual
      name, so `ollama run jetway/auto` and model pickers work.
- [ ] **App opt-in:** `route = "auto"` routes an app's requests that don't set `think`. A request that sets `think`
      explicitly keeps it, unless the app also sets `override_think = true`. PaperPilot always sends `think: false`
      (its `Ollama:Think` setting), so it is routed only with both settings on.
- [ ] The decision is recorded and returned in `X-Jetway-Route` (for example `think=on; p=0.81; by=classifier`).

### 8.2 Model aliases
- [ ] `[models.aliases]` and per app `[apps.<name>.aliases]`, for example `"qwen3.5:9b" = "qwen3.5:4b"`, applied
      before the cache key and the router. The requested and sent models are both on the exchange.

### 8.3 Thinking the client didn't ask for
- [ ] When the router turns thinking on for a request that didn't ask for it, Jetway removes the thinking from the
      response on the way out: Ollama's `message.thinking` (or `thinking` on `/api/generate`) and the
      OpenAI-compatible reasoning field found in spike S1. In streams, chunks that carry only thinking are dropped
      and mixed chunks are rewritten; the final chunk's statistics stay.
- [ ] Thinking tokens still count towards budgets (phase 9). The client's time to first token is its first answer
      token; the dashboard shows both that and the first thinking token.
- [ ] Cached answers keep their thinking in the stored form and are served with or without it, as the request
      needs.

### 8.4 Features
- [ ] The query slot's embedding from phase 6 (computed here when the semantic cache is off), plus: length in
      tokens, digits and operators, question words, code markers, list or multi-part markers, number of
      conversation turns.

### 8.5 Training data
- [ ] `evals/router/questions.jsonl`, about 300 questions with checkable answers: a GSM8K subset (MIT; numeric
      answers), an MMLU subset (MIT; multiple choice), and easy factual and definition questions with known answers,
      so both sides of the decision are represented.
- [ ] `jetway router collect`: each question is answered with thinking off and on through Jetway as app `jetway`,
      with a JSON schema for the answer (`{answer: number}` or `{choice: "A".."D"}`). Check first that structured
      output works with thinking on. Runs in capped, resumable batches agreed with you (R6); answers are cached, so
      a re-run costs nothing.
- [ ] Label per question: **thinking helps** when the off answer is wrong and the on answer right; plus the latency
      of each. Split train and test by source dataset row.

### 8.6 Model and evaluation
- [ ] `jetway router train`: logistic regression (`linfa-logistic`) on the features, trained in seconds on the CPU;
      the model is saved to `models/router.json` (small, committed, so routing works on a fresh clone).
- [ ] `jetway eval router`: on the test split, accuracy and mean latency for always-off, always-on, the router at a
      range of thresholds, and an oracle that thinks only where it helps; an SVG chart of accuracy against latency.
- [ ] Target: the router keeps at least 90% of always-on's accuracy gain at no more than 50% of its extra latency.
      Write `reports/router.md`, with the shortfall explained if the target isn't met.
- [ ] Until a model is trained, a rule fallback (length and math or multi-step markers) decides, marked
      `by=rules`.

## Tests
- [ ] Opt-in rules: virtual model, app opt-in, explicit `think` kept, `override_think`.
- [ ] `/api/tags`, `/v1/models` and `/api/show` list and describe the virtual model.
- [ ] Aliases apply before the cache key.
- [ ] Thinking removal in both protocols, streamed and not, against golden files with thinking.
- [ ] Feature extraction on fixed inputs; training and evaluation on a synthetic set where the answer is known.

## Done when
- [ ] `reports/router.md` is committed.
- [ ] `ollama run jetway/auto` works, and the exchange page shows each route and its reason.
- [ ] A client that didn't ask for thinking never receives thinking text.
