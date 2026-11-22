---
status: proposed
date: 2024-02-02
---

# Use feature flags instead of long-lived branches

## Context and Problem Statement

Branches live for weeks and the merges are painful.

## Decision Drivers

* Deploy frequency
* Merge conflict volume

## Considered Options

* Feature flags
* Short-lived branches

## Decision Outcome

Chosen option: "Feature flags", because they decouple deploy from release.

### Consequences

* Good, because release is decoupled from deploy
* Bad, because dead flags accumulate

## Pros and Cons of the Options

### Feature flags

* Good, because release is independent of deploy
* Bad, because flags need a cleanup process

### Short-lived branches

* Good, because no runtime cost
* Bad, because incomplete work still merges
