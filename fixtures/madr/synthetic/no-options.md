---
status: accepted
date: 2023-11-08
---

# Pin the Rust toolchain with a rust-toolchain.toml file

## Context and Problem Statement

Builds differ between laptops and CI because the toolchain floats. A minor version bump
changed a lint from warning to error and broke the build for a day.

## Decision Drivers

* Reproducible builds
* A predictable upgrade cadence

## Decision Outcome

Chosen option: "Add a rust-toolchain.toml", because it makes the toolchain explicit and
lets the upgrade be a reviewed change rather than an accident.

### Consequences

* Good, because CI and laptops use the same compiler
* Bad, because security patches now require a deliberate bump
