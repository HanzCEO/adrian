---
status:
date:
---

# Split the monolith into three services

## Context and Problem Statement

The deploy unit takes forty minutes and one team's change blocks every other team.

## Considered Options

* Keep the monolith and improve the pipeline
* Split into three services
* Split into a service per bounded context

## Decision Outcome

Chosen option: "Split into three services", because it buys independent deploys without
requiring a domain model we do not have yet.
