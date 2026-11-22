# Cache rendered pages in Redis

## Context and Problem Statement

Every product page requires three queries and a template render. The render dominates
latency and the data changes at most once an hour.

## Decision Drivers

* p99 latency budget of 150 ms
* Tolerance for stale content of up to an hour

## Considered Options

* Redis
* In-process LRU cache
* No cache

## Decision Outcome

Chosen option: "Redis", because the cache must be shared across four instances.

## More Information

Related to the decision to run four instances behind a load balancer.
