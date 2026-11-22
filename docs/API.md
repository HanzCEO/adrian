# API

Two surfaces on one port. REST for scripts and CI, MCP for coding agents. Both read and
write the same records.

Authentication is a single optional static bearer token. Set `ADRIAN_API_TOKEN` and send
`Authorization: Bearer <token>` on every `/v1` route. `/healthz` is always open so a
container probe works. When no token is configured the daemon trusts its network boundary,
which is the intended default for a private install.

`actor` on every write is an **unverified string**. v0.1.0 has no identity system.

## Errors

Every failure returns the same shape:

```json
{ "error": { "code": "invalid_request", "message": "`role` is required ..." } }
```

| `code` | Status | Meaning |
| --- | --- | --- |
| `invalid_request` | 400 | Malformed JSON, a missing or wrongly typed field, or a semantically wrong value. |
| `unauthorized` | 401 | Missing or wrong bearer token. |
| `not_found` | 404 | The addressed thing does not exist. |
| `conflict` | 409 | Collides with existing state, or an idempotency key reused with a different body. |
| `payload_too_large` | 413 | The body exceeded `ADRIAN_MAX_BODY_BYTES`. |
| `unsupported_media_type` | 415 | The request was not sent as `application/json`. |
| `internal` | 500 | Anything else. The message is still returned. |
| `upstream_error` | 502 | The embedder or Qdrant failed. |
| `method_not_allowed` | 405 | The path exists, but not for this method. |

There are no other statuses. A missing required field is a 400 rather than a 422, and a body
that cannot be read is a 413 or a 415, so a client only has to handle the codes above.

The rejection cases are worth spelling out because they happen before a handler runs, in
axum's own extractors. They are mapped onto the same shape rather than axum's plain-text
default, so a client that parses `{"error": {"code", "message"}}` can read every failure:

| Request | Status | `code` |
| --- | --- | --- |
| `{ not json` | 400 | `invalid_request` |
| `{"name": "x"}` with `slug` missing | 400 | `invalid_request` |
| `{"slug": 7}` | 400 | `invalid_request` |
| `Content-Type: text/plain` | 415 | `unsupported_media_type` |
| no `Content-Type` | 415 | `unsupported_media_type` |
| body over `ADRIAN_MAX_BODY_BYTES` | 413 | `payload_too_large` |
| `GET /v1/adrs?limit=abc` | 400 | `invalid_request` |
| `GET /v1/context?limit=xyz` | 400 | `invalid_request` |
| `GET /v1/search` (it is POST only) | 405 | `method_not_allowed` |
| `GET /v1/nope` | 404 | `not_found` |

---

## REST

### `GET /healthz`

```json
{
  "status": "ok",
  "version": "0.1.0",
  "embedder": { "reachable": true, "model": "uibk-embedding-v1", "version": 1 },
  "qdrant": { "reachable": true, "collection": "adr_chunks__uibk_v1" }
}
```

### `GET /v1/models`

What the embedding engine is serving, plus the collection in use.

```json
{
  "model": "uibk-embedding-v1", "version": 1, "dim": 128, "dense_dim": 768,
  "max_doc_tokens": 2048, "max_query_tokens": 256, "query_local_cap": 64,
  "device": "cpu", "revision": "b18df82e...", "collection": "adr_chunks__uibk_v1"
}
```

### `POST /v1/projects`

```json
{ "slug": "acme/billing", "name": "Billing", "repo_url": "https://github.com/acme/billing" }
```

`201` with `{ id, slug, name, repo_url, created_at }`. `409` if the slug exists.

### `GET /v1/projects`

```json
[ { "id": "...", "slug": "acme/billing", "name": "Billing", "adr_count": 42 } ]
```

### `DELETE /v1/projects/{slug}`

Deletes the project and every decision, revision, chunk and vector that belongs to it. The
vector index is cleared first, so a failed delete leaves the records in place rather than a
search that returns decisions which no longer exist.

`200` with the footprint that was removed.

```json
{ "deleted": true, "slug": "acme/billing",
  "footprint": { "adrs": 42, "revisions": 47, "chunks": 310, "vectors": 310, "embed_jobs": 0 } }
```

`404` if the slug does not exist.

### `POST /v1/adrs/batch`

Ingests many documents in one call. Use it to bootstrap a corpus instead of looping
`POST /v1/adrs`.

```json
{
  "project": "acme/billing",
  "actor": "human:hanz",
  "preserve_status": true,
  "documents": [
    { "external_ref": "docs/adr/0001-use-tokio.md", "source": "...", "scopes": ["crates/runtime"] },
    { "external_ref": "docs/adr/0002-postgres.md", "source": "..." }
  ]
}
```

`preserve_status` defaults to `false`, so each document lands `proposed` and a human must
accept it. Set it to `true` only when migrating decisions that were already accepted
elsewhere, because that records the document's own front matter without a transition.

`201` with a per-document result. One bad document does not abort the batch.

```json
{ "project": "acme/billing", "created": 2, "revised": 0, "unchanged": 0, "failed": 1,
  "results": [
    { "external_ref": "docs/adr/0001-use-tokio.md", "action": "created", "id": "...",
      "rev": 1, "status": "accepted", "warnings": [] },
    { "external_ref": "docs/adr/0003-broken.md", "action": "failed", "error": "`source` must not be empty" }
  ] }
```

`action` is `created`, `revised`, `unchanged` or `failed`. A document that fails validation
comes back as `failed` with its reason rather than as an HTTP error.

### `POST /v1/adrs`

```json
{
  "project": "acme/billing",
  "external_ref": "docs/adr/0042-use-tokio.md",
  "source": "---\nstatus: accepted\n---\n\n# Use tokio\n...",
  "source_format": "markdown",
  "scopes": ["crates/runtime", "crates/api/**"],
  "actor": "human:hanz",
  "idempotency_key": "9f3c2a1"
}
```

`source` is the raw document. `external_ref` is the client's stable key: Adrian cannot see a
filesystem, so it cannot derive one. `scopes` are **declared** assertions and win over
anything Adrian extracts from the prose.

`201` with:

```json
{ "id": "...", "rev": 1, "status": "accepted", "chunks": 9, "action": "created", "warnings": [] }
```

`action` is `created`, `revised` or `unchanged`.

Behaviour worth knowing:

- **Same bytes under the same ref** returns `unchanged` and does nothing. No re-embedding.
- **Different bytes under the same ref** creates a new revision and closes the previous
  one's transaction interval. The old revision stays readable.
- **Same bytes under a new ref** is recognised as a rename: the decision keeps its identity
  and only `external_ref` changes. This is why renames do not create duplicates.
- **The same `idempotency_key` with the same body** replays the stored response byte for
  byte. With a different body it returns `409`. `Idempotency-Key` as a header works too.

### `GET /v1/adrs`

`?project=&status=&limit=&offset=` returns summaries:

```json
[ { "id": "...", "number": 42, "title": "Use tokio", "status": "accepted",
    "external_ref": "docs/adr/0042-use-tokio.md", "rev": 1, "created_at": "2024-03-01T00:00:00+00:00" } ]
```

`project` may be omitted when exactly one project exists.

### `GET /v1/adrs/{id}`

`?rev=` defaults to the current revision. Returns the header, the revision, every section
and item with byte spans, scopes, technologies, chunks and the full status history.

```json
{
  "id": "...", "number": 42, "title": "Use tokio", "status": "accepted", "rev": 1,
  "template": "madr", "decided_on": "2024-03-01",
  "sections": [ { "kind": "context", "raw_heading": "Context and Problem Statement",
                  "ordinal": 0, "parent": null, "option_key": null,
                  "text": "...", "byte_start": 47, "byte_end": 210 } ],
  "items": [ { "kind": "option", "section": 2, "ordinal": 0, "text": "tokio", "option_key": null } ],
  "scopes": [ { "kind": "path_prefix", "value": "crates/runtime", "origin": "declared", "confidence": 1.0 } ],
  "technologies": [ { "canonical": "tokio", "role": "chosen" } ],
  "chunks": [ { "id": "...", "kind": "option", "ordinal": 0, "text": "...", "embedded": true } ],
  "status_events": [ { "at": "2024-03-01T00:00:00+00:00", "from": null, "to": "accepted",
                       "actor": "human:hanz", "reason": "initial status" } ]
}
```

`byte_start`/`byte_end` index the original source, so a caller can cite the exact text.

### `GET /v1/adrs/{id}/chunks`

The chunks and whether each has a vector yet. Useful for polling after an ingest.

### `DELETE /v1/adrs/{id}`

Deletes one decision and everything that belongs to it, including its vector points.
Irreversible. `200` with the footprint that was removed.

```json
{ "deleted": true, "id": "...", "external_ref": "docs/adr/0042-use-tokio.md",
  "footprint": { "revisions": 1, "chunks": 9, "vectors": 9, "embed_jobs": 0 } }
```

A malformed id is `400`. An unknown id is `404`.

### `POST /v1/adrs/{id}/transition`

```json
{ "to": "deprecated", "reason": "replaced by 0071", "actor": "human:hanz" }
```

`200` with `{ "status": "deprecated", "event_id": "..." }`. A target outside the closed
vocabulary is `400`, and the message lists the valid ones.

Valid statuses: `proposed`, `accepted`, `rejected`, `deprecated`, `superseded`, `withdrawn`.

### `POST /v1/search`

```json
{ "query": "which async runtime did we pick and why", "project": "acme/billing", "status": "accepted", "limit": 5 }
```

```json
{ "results": [ {
    "adr_id": "...", "number": 42, "title": "Use tokio", "status": "accepted", "score": 0.0328,
    "matched_chunks": [ { "chunk_id": "...", "kind": "outcome",
                          "text": "Chosen option: \"tokio\", because ...", "score": 0.0328 } ]
} ] }
```

Three arms, fused with reciprocal rank fusion at k=60:

1. **Semantic.** The query is embedded in the **query** role, never the document role, then
   Qdrant returns dense candidates which are reranked with an exact MaxSim over the model's
   own token matrix.
2. **Lexical.** FTS5 over chunk text with BM25.
3. **Grouping.** Results are grouped by decision, keeping the best chunk per decision.

Scores are fused ranks, not similarities: MaxSim is a bounded cosine and BM25 is unbounded,
so the two are not comparable and fusing them numerically would let one arm dominate.

### `GET /v1/adrs/{id}/similar?limit=5`

Same result shape as search. The anchor is the decision's own document chunk. The anchor is
excluded from its own results.

### `GET /v1/context?path=crates/api/src/handler.rs&project=acme/billing`

The decisions that bind a path. This is the query to run before editing code.

```json
{ "path": "crates/api/src/handler.rs",
  "decisions": [ { "adr_id": "...", "number": 42, "title": "Use tokio", "status": "accepted",
                   "matched_scope": "crates/api", "scopes": [ ... ], "summary": "The API server needs ..." } ] }
```

Matching is on ancestor prefixes, so `crates/api` matches `crates/api/src/handler.rs` but
`crates/web` does not. Ranked by scope specificity, then recency. No embedding is involved:
this is an index scan.

### `POST /v1/export`

```json
{ "project": "acme/billing", "format": "madr", "adr_ids": ["..."] }
```

`format` is `json` or `madr`. With a single id, only that record is rendered. A record
imported from Markdown and never modified is returned **verbatim**, byte for byte.

A record that *has* been modified is re-rendered from its stored sections. The body of every
section is written exactly as parsed, so a round trip through export and re-ingest reproduces
the same projection: same headings, same section text, same items, same option keys. The
assertion for that covers every fixture in the corpus, field by field, rather than a subset.

---

## MCP

`POST /mcp`, JSON-RPC 2.0. One request per POST, one JSON response, batch arrays processed
in order. `Accept: application/json, text/event-stream` is tolerated; the server always
answers with JSON.

Methods: `initialize`, `notifications/initialized`, `ping`, `tools/list`, `tools/call`,
`resources/list`, `resources/read`. Unknown methods return `-32601`, invalid params
`-32602`.

`initialize` returns an `instructions` string alongside the tool and resource capabilities.
It names the two resource URIs and states the rules a schema cannot express: where status
comes from, the closed status vocabulary, the recognised front matter keys and how the
display number is derived from `external_ref`. A client that surfaces `instructions` in its
prompt gets the contract in the same handshake that gives it the tools. A client that does
not still gets the same facts from the `adr_submit` and `adr_import` descriptions, which is
why those descriptions repeat them rather than pointing at this document.

A tool-level failure is `isError: true` inside a successful response, which is the MCP
convention; only protocol problems become JSON-RPC errors.

```bash
curl -s localhost:38922/mcp -H 'content-type: application/json' -d '{
  "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "adr_search", "arguments": { "query": "async runtime" } }
}'
```

### Tools

| Tool | Arguments | Use it when |
| --- | --- | --- |
| `project_list` | `limit?` | You need the project slugs, or another tool said `project` is required. |
| `project_create` | `slug`, `name`, `repo_url?` | The instance is fresh and `adr_submit` failed with `no project exists yet`. |
| `project_delete` | `slug` | You are removing a probe project or one created by mistake. |
| `adr_search` | `query`, `project?`, `status?`, `limit?` | You know what you are looking for, in words. |
| `adr_get` | `id`, or `project` + `number` | You have a decision and need its detail, including its chunks and whether each one is embedded. |
| `adr_list` | `project?`, `status?`, `limit?` | You want an overview, not a search. |
| `adr_submit` | `project`, `external_ref`, `source`, `actor`, `scopes?`, `idempotency_key?` | You are recording a new decision. |
| `adr_import` | `project`, `actor`, `documents[]`, `preserve_status?` | You are migrating a directory of ADRs and want one call instead of one per file. |
| `adr_delete` | `id` | You are removing a probe or a decision submitted by mistake. |
| `adr_similar` | `id`, `limit?` | You want to know whether this has been decided before. |
| `adr_transition` | `id`, `to`, `actor`, `reason?` | You are changing a decision's status. |
| `adr_context_for` | `path`, `project?`, `limit?` | You are about to change code. |

`adr_get` returns the decision's chunks with the same fields `GET /v1/adrs/{id}` uses, so a
caller can see that a document was chunked and whether the embed worker has caught up yet.

### Resources

`resources/list` advertises two documents and `resources/read` serves them. They are
embedded in the binary, so a client that only has the container can read the contract
without a checkout.

| URI | Contents |
| --- | --- |
| `adrian://docs/api` | This document. |
| `adrian://docs/madr` | The MADR support table and the status rules. |

Read `adrian://docs/madr` before importing a corpus. It says which parts of a document the
parser reads and which it ignores.

### Bulk import and the status contract

`adr_import` ingests an array of documents. Each item needs `external_ref` and `source`, and
may carry `scopes`, `source_format` and `idempotency_key`. One bad document is reported as
`failed` in the results and does not abort the rest.

`preserve_status` defaults to `false`, so an import lands every decision as `proposed`,
exactly like `adr_submit`. Set it to `true` only when the decisions were already accepted
elsewhere and the corpus is being migrated. That path records each document's own front
matter status without a human transition, which is why it is opt-in.

Status is read from YAML front matter only. A document that writes `- **Status:** Accepted`
as a bullet is recorded as `proposed`, and the response carries a `status_bullet_ignored`
warning. The same rule applies to `POST /v1/adrs`.

### What is deliberately absent

Some tools a caller will look for still do not exist. Each absence is a decision rather than
an oversight.

`adr_update` is not there because the premise behind it is wrong. Submitting the same
`external_ref` with different bytes creates a new revision. A revision never resets the
status. An accepted decision stays accepted across an edit. A document that fails to parse
never lands at all, because parsing happens before the transaction opens. There is nothing
to patch.

`adr_bulk_submit` is folded into `adr_import`, which keeps the per-document results a
loop would give while staying one round trip.

### The safety rule

`adr_submit` **always** creates the decision as `proposed`, whatever the document's own front
matter says. An agent cannot accept its own decision. Accepting requires a separate
`adr_transition`, which is a distinct and auditable act.

`adr_import` follows the same rule unless `preserve_status` is set. That flag exists for the
operator migration path, where the decisions were accepted before Adrian saw them, and it is
still an explicit request rather than a default.

This is enforced in code and covered by a test, because it is the one rule that keeps a
decision log from filling with an agent's own conclusions.

### stdio

For agents that spawn a process rather than speaking HTTP:

```json
{ "mcpServers": { "adrian": {
    "command": "adrian", "args": ["mcp-stdio", "--url", "http://127.0.0.1:38922"] } } }
```

Newline-delimited JSON-RPC on stdin, responses on stdout, diagnostics on stderr. One response
line per request line, in order. Exits 0 on stdin EOF.
