---
status: accepted
date: 2024-09-03
---

# Choose a test assertion style

## Context and Problem Statement

Assertions are written three different ways across the codebase.

## Decision Drivers

* Readability for people reviewing a failing test
* Partial string matching, which the standard library does not offer

## Considered Options

* Plain JUnit5 assertions
* AssertJ
* Hamcrest

## Decision Outcome

Chosen option: “AssertJ”, because it offers fluent assertions and partial string matching.

### Consequences

* Good, because failure messages name the expectation and the actual value
* Bad, because it is another dependency

## Pros and Cons of the Options

### Plain JUnit5 assertions

* Good, because no extra dependency
* Bad, because there is no partial string matching

### AssertJ

* Good, because it offers fluent assertions
* Good, because it allows partial string testing
* Neutral, because it is a large library but a well known one
* Bad, because assertions are less obvious to readers new to it

### Hamcrest – the older alternative

* Good, because it composes matchers well
* Bad, because the failure messages are worse than AssertJ's
