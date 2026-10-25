# Phase 6: Semantic cache

**Goal:** a question that means the same as one answered before is served from the cache, and a question that only
looks similar is not. The similarity threshold comes from an eval set you have reviewed, chosen for precision first:
a wrong answer served from the cache is much worse than a miss.

**Size:** L · **Depends on:** 3, 5

## Tasks

### 6.1 Embedder (D10)
- [ ] `embed::Embedder` trait with two implementations: `FastEmbedder` (`fastembed`, `BAAI/bge-small-en-v1.5`, 384
      dimensions, normalised) and `HashingEmbedder` (feature hashing of word n-grams, for tests and CI).
- [ ] Inference in `spawn_blocking` behind a semaphore sized from spike S2; small batches when several requests
      wait at once.
- [ ] Model files in `data/models/`, fetched by `jetway models fetch` or on first use; the model loads at startup
      when the semantic cache is enabled, so the first request doesn't pay for it.

### 6.2 Query slots, templates and partitions
- [ ] The **query slot** is the part of a request that may differ between paraphrases. Default: the last user
      message (for `/api/generate`, the `prompt`).
- [ ] Per-app **templates** find the slot inside templated prompts. A template is an optional endpoint, an optional
      "contains" marker, and a regular expression with a named group `q`; the first match wins:

      ```toml
      [[apps.paperpilot.semantic.templates]]
      name = "rag-answer"
      contains = "### Context from Papers:"
      slot = '(?s)### Question:\n(?P<q>.+?)\n\n### Answer:'

      [[apps.paperpilot.semantic.templates]]
      name = "guardrail"
      contains = "assessing whether a user query is within the scope"
      slot = 'User Query: (?P<q>.+?)\n'
      ```

      The real templates are written from the phase 5 fixtures.
- [ ] The **partition** is everything else: the exact-key fields with the slot replaced by a marker, hashed. Matches
      are searched only within one partition, so a RAG answer is reused only when the retrieved context is identical
      (a different context should give a different answer), and a guardrail verdict only for the same prompt version.
- [ ] Requests with images, tools or no text slot aren't semantically cached.

### 6.3 Index (D9)
- [ ] Per partition: a contiguous `Vec<f32>` of normalised vectors plus entry ids; search is a dot product over
      all of them (the compiler vectorises it), best match only.
- [ ] Partitions in a map behind an `RwLock`; vectors persisted in `semantic_entries` (partition, slot text, vector,
      cache entry id) and loaded at startup. A cap per app (default 50 000 entries), oldest evicted first, in step
      with the exact cache's expiry.
- [ ] Measured: a search over 10 000 vectors under 0.5 ms.

### 6.4 Match rule
- [ ] A hit needs cosine similarity ≥ the app's threshold **and** the lexical guard: the "key tokens" of both slots
      must be the same set. Key tokens are numbers, quoted strings, words with digits or inner capitals (`BERT`,
      `GPT-4`, `ResNet-50`, `LoRA`), and negations (`not`, `without`, `never`). This catches "How does BERT work?"
      against "How does GPT work?", which embeddings often rate above 0.9.
- [ ] Near misses (similarity between `threshold - 0.1` and the threshold) are logged for tuning.

### 6.5 Eval set (D19)
- [ ] `jetway dataset paraphrases draft`: from about 100 PaperPilot-style questions (BlackBox's question set,
      PaperPilot's paper titles, the phase 5 questions), `qwen3.5:4b` drafts paraphrases and hard negatives (same
      topic, different entity, number or intent), with a JSON schema in `format`, through Jetway as app `jetway`.
      About 400 pairs; a capped run agreed with you.
- [ ] A labelling page on the dashboard: each pair shown side by side, you answer **same**, **different** or
      **unsure** with keyboard shortcuts. Only reviewed pairs count; `unsure` pairs are left out.
- [ ] Stored in `evals/semantic/pairs.jsonl`; split into dev and test by source question, so paraphrases of one
      question never sit on both sides.

### 6.6 Threshold from the eval
- [ ] `jetway eval semantic`: precision, recall and F1 against the threshold on the dev split, with and without the
      lexical guard; bootstrap 95% intervals; an SVG chart.
- [ ] Choose the lowest threshold whose precision is ≥ 0.98 on dev, then report precision and recall on the test
      split at that threshold. Write `reports/semantic-cache.md` and the threshold into `jetway.toml`. A threshold of
      0.0 means "not tuned", and Jetway refuses to start with the semantic cache enabled and an untuned threshold.

### 6.7 Serving and visibility
- [ ] Hits use the exact cache's serving (phase 3) with `X-Jetway-Cache: semantic`, `X-Jetway-Similarity` and
      `X-Jetway-Source` (the exchange that produced the answer).
- [ ] The exchange page shows both questions side by side with the score. A **wrong hit** button removes the entry
      and adds the pair to the eval set as a negative.
- [ ] Order of lookups: exact first; the slot is embedded only on an exact miss. The slot's embedding is kept on the
      exchange for the router (phase 8).

### 6.8 Latency
- [ ] On an exact hit, 0 ms added; on an exact miss, the embedding plus the search, target p95 under 15 ms. The
      dashboard shows the semantic stage's time separately.

## Tests
- [ ] Slot extraction: each template, the fallback to the last user message, requests with no slot.
- [ ] Partition keys: same context gives the same partition; one changed chunk gives another.
- [ ] The index agrees with a naive reference search (property test); eviction and persistence.
- [ ] Lexical guard cases: entities, versions, numbers, negations, case differences that shouldn't matter.
- [ ] The eval runner on a small fixture set with the hashing embedder; the startup refusal for an untuned
      threshold.
- [ ] An `#[ignore]` test with the real embedder on a dozen known pairs.

## Done when
- [ ] `reports/semantic-cache.md` is committed, with the chosen threshold and test-set precision ≥ 0.98 (and its
      lower confidence bound stated).
- [ ] A capped PaperPilot run with paraphrased questions shows semantic hits on the guardrail step and on RAG
      answers with identical context, and no wrong hit on reviewed pairs.
- [ ] The semantic stage's p95 is under 15 ms on this PC.
