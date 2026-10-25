# Phase 7: Guards for personal data and prompt injection

**Goal:** personal data in a request is flagged, redacted or blocked per app, and prompt-injection attempts are
monitored or blocked per app, with precision, recall and false-positive rates measured on committed eval sets. The
defaults (`flag` and `monitor`) change no request and add no latency.

**Size:** M · **Depends on:** 2, 5

## Tasks

### 7.1 Guard framework
- [ ] A `Guard` trait returning `Verdict { guard, kind, score, location (message index and byte range), action }`.
      Inline guards run after limits and before the cache, so a redacted request is what gets cached.
- [ ] Per-app actions (D11), defaults in `[guards]`, overrides in `[apps.<name>.guards]`.
- [ ] A block never reaches Ollama: 400 in the client's error format, for example
      `{"error":"blocked by Jetway: possible prompt injection (score 0.97)"}`, with the verdict on the exchange.
- [ ] Verdicts are stored with the exchange, counted in metrics and shown on the exchange page and the overview.

### 7.2 Personal-data detectors
- [ ] Regular expressions with validators, microseconds per message:

| Kind | Rule |
|---|---|
| Email | Address pattern |
| Phone | E.164, Indian mobile numbers |
| Card number | 13–19 digits with separators, Luhn check |
| IBAN | Country format, mod-97 check |
| Aadhaar | 12 digits, Verhoeff check |
| PAN | `AAAAA9999A` format |
| IP address | IPv4, IPv6 |
| Secrets | AWS access key ids, GitHub tokens (`ghp_`, `github_pat_`), Slack tokens, PEM private-key headers, JWTs, `api_key=...` with high entropy |

- [ ] Actions: `flag` records it; `redact` replaces each value with a consistent placeholder (`[EMAIL_1]`, the same
      placeholder for the same value within one request); `block` refuses the request.
- [ ] Optional per app, `restore = true`: placeholders in the answer are put back to the original values on the way
      out. In streams this needs a small look-behind buffer, because a placeholder can be split across chunks; only
      that buffer delays output, by at most a placeholder's length.
- [ ] Scope: user messages by default; `scan = "all"` adds system, assistant and tool content.
- [ ] Stored bodies are redacted when the app's action is `redact` or `store_redacted = true` (R11).

### 7.3 Injection classifier
- [ ] The model chosen in spike S3, through `ort` and `tokenizers`, int8 when available, loaded only when an app
      uses it.
- [ ] Input: the scanned text in 512-token windows with an overlap, at most 4 windows per request (the start and end
      of longer texts); the score is the highest window's.
- [ ] A small rule detector next to it (`injection_rules`: phrases such as "ignore previous instructions", role
      markers, hidden-text tricks), explainable and cheap, reported as its own verdict.
- [ ] **monitor:** the classifier runs alongside the upstream call and its verdict is attached afterwards; adds 0 ms.
      **enforce:** it runs before the upstream call and blocks above `injection_threshold`; adds the classifier time.

### 7.4 Indirect injection
- [ ] With `scan = "all"` in monitor mode, PaperPilot's retrieved chunks are scanned too. Expect false positives on
      papers about prompt injection (R5); this is measured and shown, never enforced by default.

### 7.5 Evals (D19)
- [ ] `evals/injection/`: the test split of `deepset/prompt-injections` (Apache-2.0); `paperpilot-benign.jsonl`, 100
      research questions, including ones about jailbreaks, red-teaming and prompt injection; 30 hand-written attacks
      phrased like research questions.
- [ ] `evals/pii/`: synthetic messages with known spans, using fake values that pass the checksums, plus look-alikes
      that must not match: arXiv ids (`2401.12345`), version numbers, dates, DOIs, paper counts.
- [ ] `jetway eval injection` and `jetway eval pii`: precision, recall and false-positive rate per kind; classifier
      latency p50 and p95 by length; choose `injection_threshold` for at most 1% false positives on the benign set.
      Write `reports/guards.md` and the threshold into `jetway.toml`.

## Tests
- [ ] Detector tables, including each validator's edge cases and the look-alikes.
- [ ] Placeholder consistency, and restoring across chunk splits (property test over split points).
- [ ] Block responses in both protocols (snapshots); a blocked stream request never reaches the fake Ollama.
- [ ] Monitor mode doesn't delay the first chunk (timing test against the fake Ollama).
- [ ] Window splitting and score aggregation.
- [ ] The classifier wrapper with a stub model in CI; an `#[ignore]` test with the real model.

## Done when
- [ ] `reports/guards.md` is committed with its numbers and the chosen threshold.
- [ ] With the defaults, PaperPilot's phase 5 run behaves exactly as before.
- [ ] With an app in `enforce` mode, an injection attempt sent with `ollama run` is blocked, and a redacting app's
      stored bodies hold placeholders, not values.
- [ ] The dashboard shows guard verdicts on the overview and on each exchange.
