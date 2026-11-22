# Fixture corpus

Real MADR documents from public repositories in the [adr](https://github.com/adr)
organisation, plus hand-written edge cases in `synthetic/`.

They are checked in verbatim, so the parser tests are a regression suite against real
authoring habits rather than against the template. Do not reformat them: a fixture that
has been tidied no longer tests anything.

## Real documents

| File | Source |
| --- | --- |
| `real/adg__AD0001-domain-layer-is-independent-from-upper-layers.md` | https://github.com/adr/ad-guidance-tool/blob/HEAD/docs/adr/AD0001-domain-layer-is-independent-from-upper-layers.md |
| `real/adg__AD0002-application-layer-does-not-depend-on-adapters-or-infrastructure.md` | https://github.com/adr/ad-guidance-tool/blob/HEAD/docs/adr/AD0002-application-layer-does-not-depend-on-adapters-or-infrastructure.md |
| `real/adg__AD0003-no-dependency-cycles-in-internal-packages.md` | https://github.com/adr/ad-guidance-tool/blob/HEAD/docs/adr/AD0003-no-dependency-cycles-in-internal-packages.md |
| `real/adg__AD0004-required-project-files-must-exist-at-the-repository-root.md` | https://github.com/adr/ad-guidance-tool/blob/HEAD/docs/adr/AD0004-required-project-files-must-exist-at-the-repository-root.md |
| `real/adrlog__0000-use-markdown-architectural-decision-records.md` | https://github.com/adr/adr-log/blob/HEAD/docs/adr/0000-use-markdown-architectural-decision-records.md |
| `real/adrlog__0001-extend-markdown-toc.md` | https://github.com/adr/adr-log/blob/HEAD/docs/adr/0001-extend-markdown-toc.md |
| `real/adrlog__0002-use-console-stamp-as-logging-framework.md` | https://github.com/adr/adr-log/blob/HEAD/docs/adr/0002-use-console-stamp-as-logging-framework.md |
| `real/adrlog__0003-use-release-it-and-github-release-from-changelog-as-release-tooling.md` | https://github.com/adr/adr-log/blob/HEAD/docs/adr/0003-use-release-it-and-github-release-from-changelog-as-release-tooling.md |
| `real/adrlog__0004-filename-as-direct-parameter.md` | https://github.com/adr/adr-log/blob/HEAD/docs/adr/0004-filename-as-direct-parameter.md |
| `real/adrmanager__0000-use-markdown-architectural-decision-records.md` | https://github.com/adr/adr-manager/blob/HEAD/docs/adr/0000-use-markdown-architectural-decision-records.md |
| `real/adrmanager__0001-use-vue.js.md` | https://github.com/adr/adr-manager/blob/HEAD/docs/adr/0001-use-vue.js.md |
| `real/adrmanager__0002-use-antlr-for-parsing-adrs.md` | https://github.com/adr/adr-manager/blob/HEAD/docs/adr/0002-use-antlr-for-parsing-adrs.md |
| `real/adrmanager__0003-use-pizzly-as-backend.md` | https://github.com/adr/adr-manager/blob/HEAD/docs/adr/0003-use-pizzly-as-backend.md |
| `real/adrmanager__0004-implement-a-global-store.md` | https://github.com/adr/adr-manager/blob/HEAD/docs/adr/0004-implement-a-global-store.md |
| `real/gadr__0000-use-markdown-architectural-decision-records.md` | https://github.com/adr/gadr/blob/HEAD/docs/adr/0000-use-markdown-architectural-decision-records.md |
| `real/gadr__0001-use-gadr-as-name.md` | https://github.com/adr/gadr/blob/HEAD/docs/adr/0001-use-gadr-as-name.md |
| `real/madr__0000-use-markdown-architectural-decision-records.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0000-use-markdown-architectural-decision-records.md |
| `real/madr__0001-use-CC0-or-MIT-as-license.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0001-use-CC0-or-MIT-as-license.md |
| `real/madr__0002-do-not-use-numbers-in-headings.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0002-do-not-use-numbers-in-headings.md |
| `real/madr__0003-provide-own-madr-tools.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0003-provide-own-madr-tools.md |
| `real/madr__0004-write-own-toc-tool.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0004-write-own-toc-tool.md |
| `real/madr__0005-use-dashes-in-filenames.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0005-use-dashes-in-filenames.md |
| `real/madr__0006-use-names-as-identifier.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0006-use-names-as-identifier.md |
| `real/madr__0007-do-not-emphasize-line-headings.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0007-do-not-emphasize-line-headings.md |
| `real/madr__0008-add-status-field.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0008-add-status-field.md |
| `real/madr__0009-support-links-between-adrs-inside-an-adrs.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0009-support-links-between-adrs-inside-an-adrs.md |
| `real/madr__0010-support-categories.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0010-support-categories.md |
| `real/madr__0011-use-asterisk-as-list-marker.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0011-use-asterisk-as-list-marker.md |
| `real/madr__0012-use-curly-braces-to-denote-placeholder.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0012-use-curly-braces-to-denote-placeholder.md |
| `real/madr__0013-use-yaml-front-matter-for-meta-data.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0013-use-yaml-front-matter-for-meta-data.md |
| `real/madr__0014-allow-neutral-arguments.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0014-allow-neutral-arguments.md |
| `real/madr__0015-include-consulting-informed-of-raci.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0015-include-consulting-informed-of-raci.md |
| `real/madr__0016-outcome-before-detailed-pros-cons.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0016-outcome-before-detailed-pros-cons.md |
| `real/madr__0017-use-same-format-for-outcomes-and-options.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0017-use-same-format-for-outcomes-and-options.md |
| `real/madr__0018-use-confirmation-as-heading.md` | https://github.com/adr/madr/blob/HEAD/docs/decisions/0018-use-confirmation-as-heading.md |

## Synthetic documents

| File | What it exercises |
| --- | --- |
| `synthetic/minimal-template.md` | the MADR minimal template: mandatory sections only |
| `synthetic/bare-template.md` | the MADR bare template: every heading, no prose |
| `synthetic/no-front-matter.md` | a document with no YAML block at all |
| `synthetic/empty-status.md` | front matter where `status:` and `date:` have no value |
| `synthetic/list-deciders.md` | `decision-makers` and `consulted` as YAML sequences |
| `synthetic/crlf.md` | Windows line endings throughout |
| `synthetic/headings-in-code.md` | `#` lines inside fenced blocks that are not headings |
| `synthetic/smart-quotes.md` | typographic quotes and an en dash in option titles |
| `synthetic/no-options.md` | an ADR that records no considered options |
| `synthetic/orphan-pros-cons.md` | an H3 under Pros and Cons that matches no option |
