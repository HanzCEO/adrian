---
status: accepted
date: 2024-06-14
decision-makers:
  - Alice Nguyen
  - Bob Ortiz
  - Chen Wei
consulted:
  - the platform team
  - security review
informed: []
---

# Require signed commits on the default branch

## Context and Problem Statement

Provenance of a change is not currently verifiable.

## Considered Options

* Require signed commits
* Require review only
* Do nothing

## Decision Outcome

Chosen option: "Require signed commits", because the cost is a one-time setup per
developer and the benefit is a verifiable chain of custody.

### Consequences

* Good, because every commit on the default branch is attributable
* Bad, because it needs key distribution and rotation
