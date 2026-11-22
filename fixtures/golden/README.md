# Reference embeddings

`cases.json` is the ground truth for the Rust engine's parity test. It was produced by the
model authors' own Python, not by a transcription, so that a mistake in the Rust port
cannot be mirrored by a matching mistake in the oracle.

## How it was generated

```bash
scripts/fetch_weights.sh          # the eight model files, verified by sha256

# PyLate needs four more small files that are not needed at inference time by Rust:
#   modules.json, branched_transformer.py, hybrid_tokens.py, 1_HybridTokens/config.json
# Copy them, plus the fetched files, into scripts/oracle/model/.

cd scripts/oracle
uv run python gen_golden.py
```

The script uses `pylate.models.ColBERT` over the model's `BranchedTransformer` and
`HybridTokens` modules with `trust_remote_code=True`, `document_length=2048`,
`query_length=256`, and runs on CPU. Every case is cross-checked against PyLate's own
`encode` path, and the script fails if the two disagree.

`manifest.json` records the exact package versions, the pinned model revision, the
dimensions, the skip list, and the shape of every case.

## What a case contains

| Field | Meaning |
| --- | --- |
| `text` | the input, without the `[Q] ` / `[D] ` prefix the oracle adds |
| `input_ids` | the token ids the tokenizer produced, including `[CLS]` and `[SEP]` |
| `keep_positions` | which of those positions survive into `token_vectors` |
| `token_vectors` | the 128-dimensional unit vectors, in position order |
| `dense` | the pooled 768-dimensional sentence vector, unit norm |

`keep_positions` exists because not every token survives. Two filters apply:

- the attention mask, which the query cap has already zeroed past position 63;
- for **documents only**, a skip list of punctuation token ids taken from
  `config_sentence_transformers.json`.

Queries are exempt from the skip list on purpose: a punctuation token in a query can carry
intent, and PyLate's own code makes the same distinction.

## Why these cases

| Case | What it pins down |
| --- | --- |
| `document_plain_sentence` | the ordinary path |
| `document_code_identifier` | how `tokio::sync::RwLock` tokenizes, and that its punctuation is skipped |
| `document_smart_quotes` | typographic quotes are ordinary tokens, not special cases |
| `document_three_words` | the smallest useful document |
| `document_repeated_token` | repeated tokens must not be deduplicated |
| `query_plain` | the `[Q]` marker path |
| `query_path_like` | a query made mostly of path segments |
| `query_over_local_cap` | 87 input tokens collapse to exactly 64 vectors |

The last case is the important one. A caller that sends a document as a query, or the
reverse, gets no error at all: the scores simply degrade. The fixture makes the asymmetry
visible and the Rust test asserts it.

## Do not regenerate casually

These numbers are what the Rust engine is measured against. Regenerating them after a
dependency bump is legitimate; regenerating them to make a failing test pass is not.
