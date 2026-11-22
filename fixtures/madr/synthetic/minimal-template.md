# Use Postgres for the primary datastore

## Context and Problem Statement

We need durable relational storage for billing records, with transactional guarantees
across multiple tables.

## Considered Options

* Postgres
* SQLite
* MySQL

## Decision Outcome

Chosen option: "Postgres", because it is the only candidate that gives us transactional
DDL and a managed offering our operations team already runs.

### Consequences

* Good, because the operations team has run it for years
* Bad, because it is one more service to keep patched
