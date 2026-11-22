# MADR support

What Adrian understands, what it does not, and why some of it is guesswork.

The test corpus is `fixtures/madr/`: 35 real ADRs from public repositories in the
[adr](https://github.com/adr) organisation plus 10 hand-written edge cases. They are checked
in verbatim, so the tests are a regression suite against how people actually write ADRs
rather than against the template.

## What parses

| Variant | Support |
| --- | --- |
| MADR full, annotated | yes |
| MADR full, bare | yes |
| MADR minimal | yes, but see the warning about options below |
| Front matter absent | yes, with a warning |
| Front matter with empty values | yes, treated as absent rather than unknown |
| `decision-makers` as a list or a comma-separated string | yes |
| CRLF line endings | yes |
| Headings inside fenced code blocks | not treated as headings |
| Headings inside HTML comments | not treated as headings |
| HTML anchors in headings (`<a name="question"></a> Question`) | anchors stripped before matching |
| Typographic quotes and dashes | normalised before comparison |
| YAML front matter keys Adrian does not understand | preserved verbatim and re-emitted |

## What does not parse perfectly

**Option titles are matched by string equality, so a rename breaks the link.** MADR has no
option identifiers, by design: its own
[ADR-0006](https://github.com/adr/madr/blob/develop/docs/decisions/0006-use-names-as-identifier.md)
rejected them because "there is no markdown standard for identifiers". The option listed under
*Considered Options* and the H3 under *Pros and Cons of the Options* are therefore related
only by their text.

Adrian synthesises a key from the normalised title and joins on that. When the two spellings
differ by more than punctuation, whitespace or quote style, the join fails and Adrian says so
rather than guessing:

- `UnmatchedOption` when an option has no matching Pros and Cons subsection.
- `OrphanProsCons` when a Pros and Cons subsection matches no option.

Both are warnings, not errors, because both are common in real corpora. The MADR minimal
template, which has no Pros and Cons section at all, produces an `UnmatchedOption` for every
option. That is correct: there is nothing to join to.

**A heading that is a label rather than an option.** MADR's own ADR-0010 has
`### Pros/cons` as a sub-label inside an option, and it becomes an option of its own. It
warns. There is no reliable way to tell a label from an option title without reading the
prose, and Adrian does not read prose.

**Nygard and Y-statement records.** The parser maps headings onto a closed set of semantic
kinds, so a Nygard document with `## Status`, `## Context`, `## Decision`, `## Consequences`
parses into the same projection. A Y-statement is a single sentence with no headings, so it
yields a `document` chunk and nothing else. Both are recognised by `template` and both are
lossy on export, because rendering requires sections that a Y-statement does not have.

**Foreign formats.** ADG uses front matter for the title and anchors in headings; both are
handled. Anything else with a different structure will parse as mostly `other` sections and
is reported as such rather than silently mangled.

## The normalisation

Every heading maps onto one of a small closed set, so that templates can coexist in one
table:

| `kind` | Recognised headings |
| --- | --- |
| `context` | Context, Context and Problem Statement, Problem Statement, Motivation, Issue, Question |
| `drivers` | Decision Drivers, Drivers, Forces, Criteria |
| `options` | Considered Options, Options, Alternatives |
| `outcome` | Decision Outcome, Outcome, Decision, Chosen Option, Resolution |
| `consequences` | Consequences (as an H3 under Decision Outcome) |
| `confirmation` | Confirmation, Validation (as an H3 under Decision Outcome) |
| `pros_cons` | Pros and Cons of the Options, Pros and Cons, Pros/Cons, and every H3 under it |
| `more_info` | More Information, Links, References, Related Decisions, Notes, Comments |
| `other` | everything else, keeping the original heading |

The original heading is always kept as `raw_heading`, so rendering reproduces the author's
wording rather than Adrian's normalised name.

## Byte spans

Every section and every item carries `byte_start` and `byte_end` into the original source.
A test asserts over the whole corpus that slicing the source with those offsets returns
exactly the text reported. This is what makes a citation verifiable rather than decorative.

## Warnings you will see

| Warning | Meaning |
| --- | --- |
| `MissingTitle` | No H1, and no `title` in the front matter. |
| `InvalidFrontMatter` | The YAML block is malformed or unterminated. Parsing continues without it. |
| `UnknownStatus` | The status is not in Adrian's closed vocabulary. Falls back to `proposed`. |
| `UnmatchedOption` | An option has no Pros and Cons subsection. |
| `OrphanProsCons` | A Pros and Cons subsection matches no option. |
| `MissingSection` | A section MADR expects is absent. |
| `Note` | Something was skipped and the reason is worth reporting. |

Warnings never fail an ingest. They come back in the `POST /v1/adrs` response so a caller can
decide what to do about them.

## Status vocabulary

MADR leaves this open and its own
[ADR-0013](https://github.com/adr/madr/blob/develop/docs/decisions/0013-use-yaml-front-matter-for-meta-data.md)
records that as a weakness: the field "pretends to be more accurate than it can be (e.g.,
possible status values)". A database has to compare two statuses, so Adrian closes the set:

`proposed`, `accepted`, `rejected`, `deprecated`, `superseded`, `withdrawn`.

Imports are mapped onto it: `open`, `draft`, `pending` and `proposal` become `proposed`;
`approved`, `agreed` and `decided` become `accepted`; `obsolete` becomes `deprecated`;
`abandoned` becomes `withdrawn`; `superseded by ADR-0123` becomes `superseded`. Anything else
produces an `UnknownStatus` warning and falls back to `proposed`.

## Round-tripping

Rendering a decision to MADR and parsing it again produces the same projection. This is
asserted over the whole corpus, and it is the property that makes export trustworthy: if it
drifts, export has quietly lost or invented something.

An imported record that was never modified exports as its **original bytes**, not as a
re-rendering. Only records authored in Adrian, or revised after import, are rendered from the
projection.
