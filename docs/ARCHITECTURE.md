# Adrian

Adrian is an opinionated database for architectural decision records.

This document explains why it is shaped the way it is. The API surface is in
[API.md](API.md) and the MADR parsing rules are in [ADR-SUPPORT.md](ADR-SUPPORT.md).

## The shape of the problem

An ADR is a justified design choice. The existing tooling stores it as a Markdown file in
a directory and treats retrieval as grep. That works until a repository has more than
about twenty decisions, at which point the questions people actually ask are not
answerable:

- What constrains `crates/api/src/handler.rs` right now?
- Which decisions are about the same recurring choice, and how did other projects settle it?
- Which accepted decisions have no enforcement?
- What did we decide about the async runtime, and what did we reject and why?

The third and fourth need a database. The second needs a problem-space tier that is out of
scope for v0.1.0. The first needs the two things this version does have: scope binding and
late-interaction retrieval.

## System of record

**SQLite is the system of record. Qdrant is a derived index.**

Every record that matters lives in one SQLite file. Qdrant holds only vectors and a small
payload of identifiers and filters. If the two disagree, Qdrant is wrong and can be rebuilt
from SQLite without data loss.

This is not a stylistic choice. A vector database cannot express "this decision supersedes
that one, effective from this date, asserted by this actor". An index that claims to is
lying about what it can guarantee. Keeping the authoritative state in a transactional store
and the approximate state in an index is what makes the dual-store arrangement safe.

The consequence is that `adrian check` exists and exits non-zero when the two disagree, so
drift is a build failure rather than a silent wrong answer.

## Write path

```
POST /v1/adrs
   │
   ├─ 1. hash the source bytes (sha256)
   │     ├─ same external_ref + same hash      → Unchanged, no new revision
   │     ├─ same external_ref + different hash → new revision (rev + 1)
   │     └─ new external_ref, but the hash matches an existing record in this project
   │                                            → rename, update external_ref in place
   │
   ├─ 2. parse  (parser)      MADR Markdown → typed sections and items with byte spans
   ├─ 3. chunk  (chunker)     sections and items → one chunk per decision-relevant argument
   ├─ 4. persist (store)      adr, adr_rev, section, item, chunk, scope, technology, FTS,
   │                          and one embed_job per chunk, all in one transaction
   └─ 5. respond              { id, rev, status, chunks, warnings }

background worker
   │
   ├─ 6. claim up to 16 pending embed_jobs (single UPDATE ... RETURNING, so no double claim)
   ├─ 7. POST /v1/embed  { role: "document", texts: [...] }
   ├─ 8. upsert one Qdrant point per chunk: multivector `colbert` + single `dense`
   └─ 9. record chunk_vector, delete the job
```

Step 1 is the whole identity story. Numbers are a filename artifact and Adrian cannot see
filenames, so `external_ref` is the client's stable key and `source_sha256` is how a rename
is recognised rather than being mistaken for a new decision.

Step 4 is one transaction. A failure anywhere leaves zero rows, which a test asserts by
forcing a duplicate chunk.

## Read path

```
POST /v1/search
   │
   ├─ arm 1, semantic:  embed the query with role "query"
   │                    → Qdrant: dense ANN (768d) for candidates
   │                    → Qdrant: MaxSim rerank over the colbert multivector
   │
   ├─ arm 2, lexical:   SQLite FTS5 over chunk.text, BM25 ranked
   │
   ├─ fuse              reciprocal rank fusion, k = 60
   │                    (the two scores are not comparable: MaxSim is a bounded cosine,
   │                     BM25 is unbounded, so fusing ranks rather than scores is the
   │                     only defensible option)
   │
   ├─ group by adr_id   keep the best chunk score per decision
   │
   └─ hydrate           fetch chunk text from SQLite. Qdrant never stores text.
```

Text lives in exactly one place. Duplicating it into the vector index guarantees that the
two copies drift, and then a citation points at text that no longer exists.

## Chunking: the template is the scheme

A generic text splitter would be wrong here. MADR already tells you where the arguments
are, so chunks follow the template rather than a character count.

| Chunk kind | Composed from | Why |
| --- | --- | --- |
| `document` | the full source, byte for byte, split only when it exceeds the budget | "find decisions like this one" |
| `context` | the Context and Problem Statement section | the problem, which is what a question is usually about |
| `driver` | one Decision Driver bullet | one force, retrievable on its own |
| `option` | the option title, its prose under Pros and Cons, and **all** of its pro/con bullets | a three-word bullet alone is useless, and MADR keeps the title and the arguments in different sections |
| `outcome` | Decision Outcome including the justification | chosen option plus why |
| `consequence` | one Good/Bad bullet | |
| `confirmation` | the Confirmation section | the fitness function |
| `more_info` | the More Information section | |

The `document` row is the one that is easy to get wrong. It is the submitted bytes, not a
reassembly of the parsed sections: front matter, heading markers and blank lines all survive.
That matters because it is the chunk a "find decisions like this one" query matches, and
because export promises to return an unmodified import verbatim. An over-budget document is
split, and the pieces still concatenate back to the source exactly.

The `option` row is the other one that matters. MADR deliberately has no option identifiers:
its own ADR-0006 rejected them because "there is no markdown standard for identifiers". The
option listed under *Considered Options* and the H3 heading under *Pros and Cons of the
Options* are therefore related only by title string equality. Adrian synthesises a
deterministic key (`opt:` plus a normalised title) and joins on it, so the chunk that gets
embedded contains both halves: the title, the prose under the heading, and every
`Good` / `Neutral` / `Bad` bullet, including the ones that sit under a deeper sub-label such as
`#### Pros/cons`.

## Identity

| Thing | Scheme | Why |
| --- | --- | --- |
| ADR | UUIDv7 assigned at first ingest | time-ordered, globally unique, and independent of any filename |
| ADR revision | `(adr_id, rev)` | append-only; content changes create a revision, they never overwrite one |
| Chunk | UUIDv5 over `adr_id‖rev‖kind‖ordinal` | re-chunking the same revision is idempotent, so re-embedding does not duplicate points |
| Qdrant point | UUIDv5 over `chunk_id‖model‖version` | one point per chunk per model version, so two embedders can coexist |
| Project | slug, unique per workspace | |
| Client key | `(project_id, external_ref)` | the only thing Adrian can trust from a client that has a filesystem |

Status is not a column that gets updated into oblivion. `adr.cur_status` is a convenience
cache; the history lives in `status_event`, and every transition records who, when and why.

## The embedder contract

The model is `uibk-embedding-v1`, a late-interaction retriever built on ModernBERT. Two
properties of it shape the whole design.

**Output is a matrix, not a vector.** One 128-dimensional unit vector per token, with the
first three positions carrying a pooled dense sentence vector rotated into 127-dimensional
slices. Dimension 0 is a sign tag: positive for token vectors, negative for slot vectors.
Because of that tag, a plain MaxSim over the matrix already equals
`0.9 × (token MaxSim + 0.75 × dense cosine)`. The model gives hybrid retrieval inside a
single ColBERT index, with no score fusion at query time. That is why Adrian stores a
`colbert` multivector and can rerank with the model's own arithmetic.

**Role is not inferable, so it is required.** The model recognises a query by a `[Q]`
marker at position 1 and keeps only the first 64 token vectors on the query side. The same
text therefore embeds differently depending on which side of a retrieval it is on. If a
caller got this wrong the scores would degrade with no error at all, so `role` is a
required field with no default, and the API returns 400 when it is missing.

## Why three containers

The embedder is a Rust library and could be linked into the daemon. It is separate for
three reasons that are not about performance:

1. A model reload does not restart the database.
2. GPU memory, if enabled later, is isolated from everything else.
3. The engine can be swapped for an ONNX or Python implementation without touching Adrian,
   because the boundary is a versioned HTTP contract rather than a Rust trait.

The cost is one local HTTP hop per batch, which is nothing next to a 179M parameter forward
pass.

The daemon verifies the engine's `model` and `version` at startup and refuses to run on a
mismatch. Comparing vectors produced by two different models produces plausible nonsense,
which is the worst possible failure mode for a search system.

## What is deliberately absent

v0.1.0 is a walking skeleton: one working path through every layer, with the parts that
need a design conversation left out rather than half-built.

- Decision-point clustering, and therefore cross-project aggregation.
- Bitemporal valid-time queries. The event log is there; the query surface is not.
- Scope decay. Scope is an assertion; Adrian cannot verify it and does not pretend to.
- Multi-tenancy and verified identity.
