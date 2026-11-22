---
status: proposed
date: 2024-01-20
---

# Select a message broker

## Context and Problem Statement

Three services need asynchronous delivery with at-least-once semantics.

## Considered Options

* Kafka
* NATS

## Decision Outcome

Chosen option: "NATS", because the operational footprint is smaller and at-least-once is
sufficient.

## Pros and Cons of the Options

### Kafka

* Good, because it is the default choice in this organisation
* Bad, because it needs ZooKeeper or KRaft and a lot of tuning

### NATS

* Good, because a single binary runs the cluster
* Bad, because the ecosystem is smaller

### RabbitMQ

* Good, because it is familiar to the team
* Bad, because it was ruled out before this record was written
