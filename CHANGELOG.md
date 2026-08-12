# Changelog

## 0.9.0 — the reviewer's cockpit

diffmind was built as a gate: run it, get a verdict, pass or fail. This release
adds the other half — a place to *sit* while deciding what to say about someone
else's branch. Both surfaces share one engine; neither replaces the other.

### Breaking

- **`--format json`: `stats.chunks` → `stats.units`**, plus `units_cached` and
  `units_unparseable`. The unit of review is now a region of a file rather than
  an arbitrary slice of lines, and the field names say so. Exit codes, SARIF and
  Markdown output are unchanged, so CI gates are unaffected.
- **Daemon protocol changed.** A running 0.8 daemon is ignored rather than
  misused — the version gate already handled this — but you will want
  `diffmind serve --stop` after upgrading.
- **`chunk_diff` removed** from the `core-engine` public API, superseded by
  `build_units`.

### Added

- **Code graph.** A tree-sitter symbol graph for 13 languages — Rust,
  TypeScript, TSX, JavaScript, Python, Go, Java, C#, Ruby, PHP, C, C++ and
  Scala — stored in `.diffmind/graph.db` and updated incrementally
  by mtime. Replaces the regex symbol index, which could only see `pub`/`export`
  declarations and had no concept of a reference at all.
  Review context now includes **the callers of every changed symbol** — a
  changed signature is judged against the code that depends on it — plus the
  enclosing definition, referenced definitions, and the file's tests. Everything
  stays inside a byte budget, so context does not grow with the repository.
  Bodies are read from the working tree rather than stored, so a snippet can
  never disagree with the file being reviewed. The graph refreshes itself
  incrementally before every review (~0.1s on 647 unchanged files) and builds on
  first use, so it can never fall behind the code; `--no-index` opts out. Adding a language is one entry in
  a table; contributions welcome.

- **Cross-file review units.** When a symbol and code that calls it both change
  in one diff, they are reviewed together as a single unit instead of separately.
  Reviewed apart, the model judges an interaction while seeing only one side of
  it as background. One call replaces two.
- **Reviewer's cockpit** (`diffmind --tui`). Analyses on launch. Each finding
  shows the actual hunk the model reviewed and the context it was given.
  `a` accepts (and copies a review comment via OSC 52, which works over SSH),
  `d` dismisses, `w` marks wrong. Verdicts are written through immediately.
- **Review standards as markdown** — `.diffmind/rules/*.md`, scoped by path
  glob, committed to the repo. `diffmind rules init` / `rules list`. A finding
  the model attributes to a rule set gets the ID `rulebook.<id>` and suppresses
  like any other; an attribution naming a rule set that does not govern that
  file is discarded.
- **Reportable pre-filter.** Lockfiles, `linguist-generated` paths, `@generated`
  banners, minified bundles, assets, snapshots and formatting-only hunks are
  dropped before the model sees them — and the run says what it skipped:
  `312 hunks → 74 reviewable (238 filtered: lockfiles, generated, formatting)`.
  Whitespace inside a string literal counts as content; indentation is never
  dismissed in Python or YAML.
- **`diffmind stats`** — findings, cost and the accept-to-wrong ratio over
  recorded runs. Every review is filed to `.diffmind/runs/<sha>/`.
- **Cost reporting** — wall-clock and token counts in the footer, in JSON, and
  in the run record. Estimated counts are marked `~` rather than passed off as
  exact.
- **Revision ranges** — `diffmind v1.2.0..HEAD`, or `--range`. Paths after a
  range narrow it.
- **`review.ignore`** globs in `.diffmind/config.toml`.
- diffmind writes its own `.diffmind/.gitignore`, keeping generated state out of
  git while leaving `rules/`, `rules.toml`, `config.toml` and `baseline.json`
  committable. Your repository's `.gitignore` is not touched.

### Fixed

- **The result cache never worked across files.** Context was assembled once
  from the whole diff and folded into every chunk's cache key, so editing one
  file invalidated every other file's cached result — a re-review after a
  force-push re-inferred the entire diff. Context is now assembled per unit.
- **Each chunk was given the wrong context.** The six enclosing functions were
  taken from whichever files sorted first, so most chunks paid for context about
  files they did not contain.
- **Editing one region of a file re-reviewed the whole file.** Units are grouped
  by region, so a change in one function only invalidates that function.
- **Glob matching could not express a directory prefix.** `src/api/**` matched
  nothing, and `**/gen/**` was a substring test that also matched
  `src/gen-legacy/`. Affects `rules.toml` `files` patterns as well.
- **Conflicting input selectors were silently resolved by order.** Passing both
  `--staged` and `--last` reviewed whichever the code checked first; it now
  errors.
- A diff dominated by a lockfile is no longer refused for size before the
  lockfile is filtered out.
- The TUI now honours the pre-filter and files a run record, which it did not.
