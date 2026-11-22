#!/usr/bin/env python3
"""Generate reference embeddings for the Rust engine's parity test.

This uses the model authors' own Python: PyLate's `ColBERT` wrapper over the
`BranchedTransformer` and `HybridTokens` modules that ship with the model. It is
deliberately not a reimplementation, so that a mistake in the Rust port cannot be
mirrored by a matching mistake in the oracle.

Run from this directory:

    uv run python gen_golden.py

The model files are expected in ./model. They are the same files `scripts/fetch_weights.sh`
downloads, plus the four small Python and config files that PyLate needs:

    modules.json, branched_transformer.py, hybrid_tokens.py, 1_HybridTokens/config.json

Nothing here is committed except the output, which lands in ../../fixtures/golden/.
"""

from __future__ import annotations

import json
import platform
import sys
from pathlib import Path

import numpy as np
import torch
from pylate import models

HERE = Path(__file__).resolve().parent
MODEL_DIR = HERE / "model"
OUT_DIR = HERE.parent.parent / "fixtures" / "golden"

MODEL_REVISION = "b18df82e77306006a1dd0162199f6dfeacb5492e"
DOCUMENT_LENGTH = 2048
QUERY_LENGTH = 256
QUERY_LOCAL_CAP = 64

# The same text, repeated enough times to exceed the 64-token query cap. The point of this
# case is that the query side keeps exactly 64 token vectors while the document side keeps
# all of them, which is the asymmetry a caller can silently get wrong.
LONG_QUERY = (
    "We need to decide which asynchronous runtime to adopt for the API server, "
    "weighing ecosystem maturity, performance under sustained load, the size of the "
    "dependency tree, how well it integrates with the existing tracing and metrics "
    "stack, and whether the team has prior experience operating it in production. "
    "The decision also has to account for how easy it is to test deterministically "
    "and how the choice constrains future work on background jobs and scheduled tasks."
)

CASES = [
    {
        "name": "document_plain_sentence",
        "role": "document",
        "text": "The API server needs an async runtime.",
    },
    {
        "name": "document_code_identifier",
        "role": "document",
        "text": "We guard the shared state with `tokio::sync::RwLock`, because a Mutex would serialize readers.",
    },
    {
        "name": "document_smart_quotes",
        "role": "document",
        "text": "Chosen option: \u201cAssertJ\u201d, because it offers fluent assertions.",
    },
    {
        "name": "document_three_words",
        "role": "document",
        "text": "Use tokio now.",
    },
    {
        "name": "document_repeated_token",
        "role": "document",
        "text": "tokio tokio tokio tokio",
    },
    {
        "name": "query_plain",
        "role": "query",
        "text": "which async runtime did we pick",
    },
    {
        "name": "query_path_like",
        "role": "query",
        "text": "what constrains crates/api/src/handler.rs",
    },
    {
        "name": "query_over_local_cap",
        "role": "query",
        "text": LONG_QUERY,
    },
]


def load_model() -> models.ColBERT:
    return models.ColBERT(
        model_name_or_path=str(MODEL_DIR),
        trust_remote_code=True,
        device="cpu",
        document_length=DOCUMENT_LENGTH,
        query_length=QUERY_LENGTH,
    )


def embed(model: models.ColBERT, text: str, role: str):
    """Run the module chain by hand so that both the token matrix and the pooled dense
    vector are reachable.

    Order matters: `HybridTokens.forward` overwrites `token_embeddings` with the 128-dim
    output, and `dense_vectors` reads `token_embeddings` to learn the row layout, so the
    dense vector has to be extracted between the two modules.
    """
    prefix = "[D] " if role == "document" else "[Q] "
    max_length = DOCUMENT_LENGTH if role == "document" else QUERY_LENGTH

    encoded = model.tokenizer(
        [prefix + text],
        padding=False,
        truncation=True,
        max_length=max_length,
        return_tensors="pt",
    )
    input_ids = encoded["input_ids"]
    features = {
        "input_ids": input_ids,
        "attention_mask": encoded["attention_mask"],
    }
    with torch.no_grad():
        features = model[0](features)          # BranchedTransformer
        dense = model[1].dense_vectors(features)  # latent-attention pooling, L2 normalised
        features = model[1](features)          # HybridTokens

    # Which positions survive is not simply "all of them". PyLate applies two filters,
    # and both matter for parity:
    #
    #   * the attention mask, which the query cap has already zeroed past position 63;
    #   * for documents only, a skip list of punctuation token ids.
    #
    # Queries are deliberately exempt from the skip list, because a punctuation token in
    # a query can carry intent.
    ids = input_ids[0].tolist()
    keep = features["attention_mask"][0].bool().numpy().copy()
    if role == "document":
        skip = set(model.skiplist)
        for index, token_id in enumerate(ids):
            if token_id in skip:
                keep[index] = False

    vectors = features["token_embeddings"][0].numpy()[keep].astype(np.float32)
    keep_positions = [index for index, flag in enumerate(keep) if flag]
    return ids, keep_positions, vectors, dense[0].numpy().astype(np.float32)


def main() -> int:
    if not MODEL_DIR.exists():
        print(f"error: {MODEL_DIR} does not exist", file=sys.stderr)
        print("Run scripts/fetch_weights.sh and copy the extra files listed in the header.",
              file=sys.stderr)
        return 1

    model = load_model()
    cases_out = []
    shapes = []

    for case in CASES:
        ids, keep_positions, vectors, dense = embed(model, case["text"], case["role"])

        # Sanity checks that the fixture itself is meaningful. A golden file that is wrong
        # in the same way as the Rust port would be worse than no golden file at all.
        norms = np.linalg.norm(vectors, axis=1)
        assert np.allclose(norms, 1.0, atol=1e-4), f"{case['name']}: not unit norm"
        assert vectors.shape[1] == 128, f"{case['name']}: wrong width"
        assert dense.shape[0] == 768, f"{case['name']}: wrong dense width"
        assert abs(float(np.linalg.norm(dense)) - 1.0) < 1e-4, f"{case['name']}: dense not normalised"

        # Dimension 0 is the tag: positive for token vectors, negative for slot vectors,
        # and the first three positions are always slots.
        for index in range(3):
            assert vectors[index, 0] < 0, f"{case['name']}: position {index} is not a slot"
        for index in range(3, len(vectors)):
            assert vectors[index, 0] > 0, f"{case['name']}: position {index} is not a token"

        if case["role"] == "query":
            expected = min(len(ids), QUERY_LOCAL_CAP)
            assert len(vectors) == expected, (
                f"{case['name']}: expected {expected} query vectors, got {len(vectors)}"
            )
            assert keep_positions == list(range(expected)), (
                f"{case['name']}: the query cap must keep a prefix of the positions"
            )

        cases_out.append(
            {
                "name": case["name"],
                "role": case["role"],
                "text": case["text"],
                "input_ids": ids,
                "token_count": len(ids),
                "keep_positions": keep_positions,
                "token_vectors": [[float(x) for x in row] for row in vectors],
                "dense": [float(x) for x in dense],
            }
        )
        # Cross-check against PyLate's own encode path, so a transcription error in the
        # manual module chain cannot slip into the fixture.
        reference = model.encode(
            [case["text"]],
            is_query=(case["role"] == "query"),
            output_value="token_embeddings",
            convert_to_numpy=True,
        )[0]
        if not np.allclose(reference, vectors, atol=1e-5):
            raise SystemExit(
                f"{case['name']}: manual module chain disagrees with pylate encode "
                f"(max diff {float(np.abs(reference - vectors).max())})"
            )

        shapes.append(
            {
                "name": case["name"],
                "role": case["role"],
                "input_tokens": len(ids),
                "kept_positions": len(keep_positions),
                "token_vectors": len(vectors),
            }
        )
        print(f"  {case['name']}: {len(ids)} tokens -> {len(vectors)} vectors")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    with (OUT_DIR / "cases.json").open("w") as handle:
        json.dump(cases_out, handle, indent=2)
        handle.write("\n")

    import pylate
    import transformers

    manifest = {
        "oracle": "pylate.ColBERT over the model's own branched_transformer and hybrid_tokens",
        "model": "DataScience-UIBK/uibk-embedding-v1",
        "model_revision": MODEL_REVISION,
        "document_length": DOCUMENT_LENGTH,
        "query_length": QUERY_LENGTH,
        "query_local_cap": QUERY_LOCAL_CAP,
        "token_dim": 128,
        "dense_dim": 768,
        "slot_count": 3,
        "local_weight": 0.9,
        "slot_weight": 0.225,
        "device": "cpu",
        "python": platform.python_version(),
        "versions": {
            "torch": torch.__version__,
            "transformers": transformers.__version__,
            "pylate": getattr(pylate, "__version__", "unknown"),
            "numpy": np.__version__,
        },
        "skiplist_words": json.loads(
            (MODEL_DIR / "config_sentence_transformers.json").read_text()
        )["skiplist_words"],
        "skiplist_token_ids": [int(x) for x in model.skiplist],
        "cases": shapes,
    }
    with (OUT_DIR / "manifest.json").open("w") as handle:
        json.dump(manifest, handle, indent=2)
        handle.write("\n")

    print(f"wrote {len(cases_out)} cases to {OUT_DIR / 'cases.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
