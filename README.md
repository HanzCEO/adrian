# Adrian v0.1.0

An opinionated database for architectural decision records.

Adrian stores decisions, not documents. A decision is parsed into a typed projection,
chunked along the lines the template already draws, embedded with a late-interaction
retrieval model, and indexed so that a coding agent can ask "what constrains this file"
and get an answer with citations.

```
                 ┌──────────────────────────────────────────┐
   :8080  ──────▶│  adrian                                  │
                 │  REST + MCP on one port, SQLite inside   │
                 └───────┬───────────────────────┬──────────┘
                         │                       │
             internal network `adrian`           │
                         │                       │
                 ┌───────▼───────┐       ┌───────▼──────────┐
                 │  qdrant       │       │  embedder        │
                 │  derived      │       │  uibk-embedding  │
                 │  vector index │       │  v1, candle, CPU │
                 └───────────────┘       └──────────────────┘
```

Only `adrian` publishes a port. Qdrant and the embedder are implementation details of the
database, reachable only from inside the compose network.

## Quickstart

```bash
cp .env.example .env
make weights            # one-time: fetch and verify the 717 MB model
# a corrupt or partial download fails loudly; re-fetch with:
#   scripts/fetch_weights.sh --force
docker compose -f docker/docker-compose.yml up -d --build
```

Then:

```bash
# create a project
curl -s localhost:8080/v1/projects \
  -H 'content-type: application/json' \
  -d '{"slug":"demo","name":"Demo"}' | jq

# ingest an ADR
curl -s localhost:8080/v1/adrs \
  -H 'content-type: application/json' \
  -d "$(jq -Rn --rawfile s docs/adr/0001-use-tokio.md \
        '{project:"demo", external_ref:"docs/adr/0001-use-tokio.md", source:$s, actor:"human:me"}')" | jq

# search
curl -s localhost:8080/v1/search \
  -H 'content-type: application/json' \
  -d '{"query":"which async runtime did we pick and why","project":"demo"}' | jq
```

## Connecting a coding agent

Two forms, both backed by the same seven tools.

Streamable HTTP, on the port the daemon already exposes:

```json
{
  "mcpServers": {
    "adrian": { "type": "http", "url": "http://127.0.0.1:8080/mcp" }
  }
}
```

stdio, for agents that insist on spawning a process:

```json
{
  "mcpServers": {
    "adrian": {
      "command": "adrian",
      "args": ["mcp-stdio", "--url", "http://127.0.0.1:8080"]
    }
  }
}
```

The tools are `adr_search`, `adr_get`, `adr_list`, `adr_submit`, `adr_similar`,
`adr_transition`, and `adr_context_for`. See [docs/API.md](docs/API.md).

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `ADRIAN_BIND` | `0.0.0.0:8080` | Address the HTTP server binds to. |
| `ADRIAN_PORT` | `8080` | Host port published by compose. |
| `ADRIAN_DB_PATH` | `/data/adrian.db` | SQLite file. |
| `ADRIAN_QDRANT_URL` | `http://127.0.0.1:6333` | Qdrant base URL. Compose overrides it with the container address. |
| `ADRIAN_EMBEDDER_URL` | `http://127.0.0.1:8081` | Embedding engine base URL. Compose overrides it with the container address. |
| `ADRIAN_COLLECTION` | `adr_chunks__uibk_v1` | Qdrant collection for chunk vectors. |
| `ADRIAN_API_TOKEN` | *(empty)* | Static bearer token for `/v1`. Empty disables auth. |
| `ADRIAN_MCP_TOKEN` | *(empty)* | Static bearer token for `/mcp`. Falls back to `ADRIAN_API_TOKEN`. |
| `ADRIAN_WORKER` | `1` | Run the background embed worker in this process. |
| `ADRIAN_MAX_BODY_BYTES` | `4194304` | Maximum accepted request body. |
| `ADRIAN_LOG` | `info` | `tracing` filter directive. |
| `ADRIAN_EMBED_MODEL` | `uibk-embedding-v1` | Model the engine must be serving. |
| `ADRIAN_EMBED_VERSION` | `1` | Model version the engine must be serving. |

The embedding engine container reads its own three:

| Variable | Default | Meaning |
| --- | --- | --- |
| `EMBEDDER_BIND` | `0.0.0.0:8081` | Address the engine binds to. Never published. |
| `ADRIAN_WEIGHTS_DIR` | `/models` in the image | Directory holding the model files and `SHA256SUMS`. |
| `RUST_LOG` | `info` | `tracing` filter for the engine. |

## What v0.1.0 does not do

Stated plainly, because a database that quietly implies more than it delivers is worse
than one that does less:

- **No clustering into decision points.** The cross-project question "30 of 40 projects
  chose tokio, here is why" needs a problem-space clustering tier. Not in this version.
- **No bitemporal valid-time queries.** Status is stored as an event log, so the history is
  there, but there is no "which decisions were in force on 2024-03-01" query yet.
- **No scope decay tracking.** Scope is an assertion by the client. Adrian cannot see a
  repository, so it cannot notice that a path it was told about no longer exists.
- **No multi-tenancy.** One workspace, one user. A single optional bearer token stands in
  for authentication, and `actor` on every write is an unverified string.
- **No verified actor identity.** Anything can claim to be `human:you`.
- **No GPU.** CPU only, with a `cuda` feature flag present in the embedder crate but off.
- **No repository scanning, ever.** Documents arrive over the API. Adrian has no filesystem
  view of your project and never will.

## Layout

```
crates/adrian-core/   domain types, MADR parser, chunker, SQLite store, Qdrant client
crates/adrian-api/    axum router, REST handlers, MCP server
crates/adrian-bin/    the `adrian` binary
embedder/             separate workspace: the candle inference engine
migrations/           SQL schema
fixtures/madr/        real MADR corpus used by parser and round-trip tests
fixtures/golden/      reference embeddings generated from the model's own Python
docker/               compose file and both Dockerfiles
```

The embedder is a separate cargo workspace so that `cargo test` on the daemon does not
have to compile candle.

## Development

```bash
make check      # fmt, clippy, both test suites
make test
make up / down / logs
make weights    # fetch model weights
make golden     # regenerate reference embeddings (needs uv + torch)
make smoke      # full end-to-end against the compose stack
```

## Licence

Apache-2.0.
