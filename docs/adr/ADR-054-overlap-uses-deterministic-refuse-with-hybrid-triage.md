# ADR-054: Overlap uses deterministic refuse with hybrid triage — Laya is dropped

**Status:** Proposed
**Date:** 2026-09-29
**Deciders:** George
**Supersedes:** the note-449 judge-process-model plan (VOID — see Context)

## Context

Duplicate filings reach the board as separate rows for one piece of work, and the
corpus-first gate (`a-9438f17d`) required measured evidence before any refusal machinery
could land. That evidence now exists: corpus-v1 (7 duplicate / 220 distinct pairs, all 114
row texts resolved live) and the full eval (kb `t-91e669a8` note 560; evidence at
`/Users/geoyws/work/wt/laya-eval-t-91e669a8/EVAL.md`). The eval compared two shapes
head-to-head on the filing-time question — refuse, warn, or land — and the margins are not
close:

- **Zero-false-refusal bar** (the refuse bar: highest recall with 0 FP on the 220
  negatives): hybrid exact-tier 1/7 clean; Laya 0/7 on every signal (noul, similarity,
  P(duplicate), P(dup+overlapping)) and both state constructions (full bodies and
  titles-only). Every Laya signal's negative maximum exceeds every positive value.
- **Retrieval:** hybrid recall@3 4/7, @5 5/7, @10 7/7 on the live board with real open-set
  distractors. Laya is a pair judge and contributes no retrieval of its own.
- **Latency per filing:** hybrid ~0.1–0.6s (0.08–0.09s server-side, ~0.5–0.6s end-to-end
  via `kb-board`) against Laya ≈ 40s for k=5 judged candidates (warm, capped 8-CPU).
- **The four `--lane` duplicates** that motivated the slice sit below every clean
  threshold on both systems (hybrid ranks 3/3/6 at 0.49/0.55/0.41; Laya noul 0.55–0.84),
  and no warn-level cutoff on any fuzzy signal separates the classes (60–80% of distinct
  pairs admitted — alert fatigue).

The note-449 plan — a Laya judge process with a pinned model revision behind the filing
path — predates these measurements and is superseded by them: there is no operating point
at which the judge refuses a true duplicate without also refusing distinct rows, so
pinning its revision would pin a refusal engine with no safe setting. The pinned revision
(`receptron/laya-onnx@68f27df`, `laya.onnx a874eb25..6dba1e`) stays recorded in the eval
for the record, not in the product.

Work is specified in `docs/specs/overlap.md` (slice `OVERLAP`, `OV-01` .. `OV-12`).

## Decision

Refuse stays deterministic; hybrid top-k feeds acknowledgement triage; Laya ships nothing:

1. **Two refuse rules, both equalities, no scores.** A normalised-title rule (lowercase,
   trim, collapse whitespace — `OV-01`) and a touches path/glob intersection rule
   (`OV-02`) refuse checked writes (`task add`, `attention raise`, `claim`, `claim --next`;
   explicitly not `task update`) with sentences naming the blocking row. Equality and
   intersection are the whole rule because the eval proved cutoffs cannot be: the only
   numeric rule with zero false refusals is hybrid exact-tier `score >= 1.0` at 1/7
   recall, and the specification refuses on equality rather than re-stating that
   threshold.
2. **Hybrid retrieval warns, never refuses** (`OV-07`, `OV-09`). Each checked write takes
   the top-10 open rows by rank position alone — no threshold, because none separates the
   classes — lands anyway, and answers the scored candidates as a payload for the filer.
3. **Acknowledgement is per-write and on the record** (`OV-08`). `--overlap-ack ID`
   names considered candidates, is valid only for the write's own warn set, is consumed
   by that write, and appends chained audit events (ADR-029).
4. **No judge process, no semantic threshold, no model revision, no `ort` build-time
   download, no 1.7 GB bundle.** The product gains no Laya dependency in any form.

## Alternatives considered

- **Laya judge behind the filing path (the note-449 plan).** Rejected on the eval
  margins above: 0/7 zero-false-refusal recall on every signal and both constructions,
  no retrieval contribution, ≈ 40s per filing. A judge that cannot refuse without false
  refusals and cannot warn without alert fatigue is triage theatre at filing-time cost.
- **Pure-lexical refuse (hybrid score cutoff as the refusal rule).** Rejected: the
  semantic-only sweep shows neg semantic max 0.612 exceeding 6/7 positives, and only the
  exact tier is clean at 1/7 — a cutoff set to catch the four `--lane` duplicates
  (scores 0.41–0.55) also refuses distinct rows. Lexical signal is therefore confined to
  ordering the warn payload, where its recall@10 7/7 is genuinely useful.
- **Checking `task update`.** Rejected for now (`OV-06`): edits refine live rows, and
  refusing them strands in-progress work behind a rule meant for new filings. Revisit as
  a supersession with corpus evidence if update-time doubles materialise.

## Consequences

- The slice adds `touches` storage (board schema 37 migration), `--touches`,
  `--overlap-ack`, cross-board claim provenance (`--repo`/`--commit`, local-oracle
  only, never a fetch), and `overlap sweep` (read-only, CLI only — no web panel: serve
  is retired under ADR-053).
- Per-write screening carries a 2-second budget, fail-closed on timeout (`OV-12`) —
  headroom over the measured 0.1–0.6s, an order below any judge-shaped step.
- Corpus-first (`a-9438f17d`) stays the standing rule: any widening of the refuse rules
  (notably the OQ-1 normalisation edges and the OQ-2 repo identity in the specification)
  arrives with corpus evidence as a supersession, never as a reinterpretation.
- History stays readable: the note-449 plan is marked superseded here, the eval keeps
  the pinned revision for the record, and the withdrawn `spa.md` precedent is untouched.
