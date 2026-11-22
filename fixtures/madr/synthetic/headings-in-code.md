# Document configuration with a single source

## Context and Problem Statement

Configuration is described in three places and they disagree. The canonical shape is:

```yaml
# Server settings
server:
  port: 8080
## not a heading, this is a YAML comment
```

And the equivalent in a shell script:

```
#!/bin/sh
# Start the server
exec ./server --port 8080
```

Neither block contains a heading, and a parser that thinks they do will lose the rest of
the document.

## Considered Options

* One YAML file referenced everywhere
* Generated documentation from the schema
* Leave it as it is

## Decision Outcome

Chosen option: "Generated documentation from the schema", because drift becomes
impossible rather than merely discouraged.
