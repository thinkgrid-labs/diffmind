# Changelog

## [0.9.2] — 08-12-2026

### Fixed

- **The dropped-rule-set warning no longer cries wolf.** 0.9.1 measured every
  rule set in `.diffmind/rules/` against the budget, so a `react.md` scoped to
  `**/*.tsx` was reported as dropped from a diff containing no `.tsx` at all.
  The check now runs per changed file, against only the rule sets that actually
  govern it, and names the file:

  ```
  !  2 rule set(s) do not fit the prompt budget (4851 bytes) and were dropped:
     aaa-style (on app/dashboard/page.tsx), mmm-quality (on app/dashboard/page.tsx)
  ```

  A warning that fires when nothing is wrong is one people learn to scroll past,
  which costs more than having no warning at all.

## [0.9.1] — 08-12-2026

Config is not code, and reviewing it as though it were produced confident
nonsense. Reported from the field: every file under `.claude/` came back HIGH.
Rule sets had the mirror-image problem — they could stop applying without ever
saying so.

### Fixed — rule sets

- **A rule set dropped for prompt budget is now reported**, with the budget and
  the reason. Previously it vanished silently — the failure this feature was
  otherwise built to avoid.
- **The lowest severity is dropped first.** Rule sets load in filename order, so
  overflow used to shed whichever sorted last: a `high` security set lost to a
  style guide on the letter `s`. Severity is the only ranking the author
  declared, so it decides. Surviving sets still *render* in filename order, so
  the cache key does not move.

### Added

- **`!` exclusion globs in `scope`** — `["src/**", "!src/legacy/**"]`, the way
  CodeRabbit's `path_filters` reads. An exclusion beats every include.
- **`always: true`** to apply a rule set to every file regardless of `scope`.
  An empty `scope` already meant this; now it can be said out loud, and
  `rules list` marks it.
- **`description:`** — one line for humans, shown by `rules list`, never sent to
  the model.
- **`globs:` and `alwaysApply:` accepted as aliases**, so a rule set ported from
  `.cursor/rules/` loads unedited.
- **`diffmind rules check`** — validates every rule set and pattern rule and
  exits non-zero, for CI. Catches parse errors, duplicate ids, identical bodies,
  scopes that match nothing, and invalid regex in `rules.toml`.
- **A `scope` of only `!` exclusions is now a parse error.** It matched no file
  at all, which is a rule set that silently does no work.
- **`examples/nextjs-app-router/`** — a complete, working rule set for a
  React/Next.js project: three scoped `.md` rule sets and 13 regex rules.

### Fixed — what gets reviewed

- **Coding-agent and editor config is no longer reviewed.** `.claude/`,
  `.cursor/`, `.windsurf/`, `.aider/`, `.vscode/`, `.idea/`, `.zed/` and
  `.fleet/` are dropped by the pre-filter. These files are imperative English
  about credentials, shell commands and permissions — precisely the shape a
  reviewer prompt primed for "exposed secrets, disabled auth" reads as an
  emergency. `.claude/` is also instructions written *for* a model, which is a
  poor thing to hand to one.
- **Ignore files and formatter config are dropped**: `.gitignore`,
  `.dockerignore`, `.prettierignore` and the rest of the `.*ignore` family,
  plus `.editorconfig`, `.gitattributes`, `.prettierrc*`, `.eslintrc*`,
  `.npmrc`, `.nvmrc`, `.cursorrules`. Declarative lists with no program logic
  in them. `eslint.config.js` and friends are still reviewed — those are real
  JavaScript, and a bug in one is a bug.
- **Documentation is dropped by default** (`.md`, `.mdx`, `.rst`, `.txt`,
  `.adoc`, `.org`, `.tex`). Re-enable with `--include-docs` or
  `review.include_docs = true` for docs that carry API contracts. The switch
  covers prose only — agent config stays out either way.

Everything dropped is *counted and reported* in the existing summary line
("312 hunks → 74 reviewable"), under the new `docs` and `tool config` reasons,
rather than silently disappearing. Test files are unaffected and still
reviewed: a test that asserts nothing is worth catching.

## [0.9.0] — 08-12-2026

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

### Security

- **Model weights are now pinned and verified.** Downloads used HuggingFace's
  `resolve/main/…`, a moving ref, and the only checksum on record was whatever
  digest the bytes that arrived happened to produce — so `--verify` could prove a
  file had not rotted on disk and never that it was the right file. Each entry
  now carries an immutable commit sha plus the expected SHA-256 and byte count,
  checked before the download is moved into place; a mismatch refuses to install
  rather than warning. The tokenizer is pinned the same way, since one that
  disagrees with the model shifts every token id without failing loudly.
  A model left behind by a build pinning a different revision is treated as
  absent and re-fetched instead of loaded.

  This closes a real gap in the reproducibility claim: the same model id could
  previously mean different bytes on two machines, or on one machine a month
  apart. The per-download `.receipt.json` sidecars are gone — the expectation
  ships in the binary, so the "no checksum on record" state no longer exists.
  Existing downloads are verified against the new pins on next use; the current
  files match, so no re-download is expected.

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

### Changed

- `Graph::definitions_of` is replaced by `definitions_of_names` (batched) and
  `declarations_overlapping` (span-based). The "prefer a definition in this
  file" rule moved to `rag`, where the file being reviewed is known.
- `ReviewAnalyzer::analyze`'s progress callback takes the unit being reviewed:
  `Fn(usize, usize)` → `Fn(usize, usize, &ReviewUnit)`. Callers that need to
  show a reader the hunk behind a finding should record it here rather than
  predicting the unit list with `plan_units`.

### Fixed

- **A non-ASCII path in a diff header could abort the process.** The
  `diff --git a/… b/…` parser sliced the line at a byte midpoint without a
  character-boundary check, so a rename involving a non-ASCII filename — routine
  with `core.quotePath=false`, or in any diff piped to `--stdin` — panicked
  instead of reviewing. The same arithmetic was one byte off, so the branch had
  never matched anything and every correct answer came from the fallback below
  it. Now fixed rather than removed: a `diff --git` line resolves on its own,
  which matters for binary files and mode changes, where no `+++ b/…` follows to
  correct it.

- **A large rule set could fail the whole review.** Two faults compounding.
  Splitting an oversized prompt halves the *diff*, but the context and rule
  sections were sized from the window alone — identical at every recursion
  level — so a prompt that overran because of its rules failed the same way four
  times and then gave up. And giving up returned an error that propagated out of
  `analyze`, exiting 2 and discarding the findings every *other* unit had
  produced, including deterministic ones that never needed a model.

  Both sections now shrink with each split (dropping whole rule sets rather than
  truncating one mid-sentence), and are held to at most half of what the window
  has left, so the diff under review cannot be crowded out. A unit that still
  will not fit is counted in the new `units_too_large` stat and skipped, and the
  footer says so. Unchanged for an ordinary review: at depth 0 on the shipped
  32K window the budgets are exactly what they were.

- **A non-ASCII identifier could abort the detector.** `references_identifier`
  advanced by one *byte* past a match, so a name whose first character is
  multi-byte — `const élan = 1`, which `extract_declared_name` collects because
  `char::is_alphanumeric` is Unicode-aware — landed mid-codepoint and panicked
  the process on the next search. The same byte arithmetic also read a UTF-8
  continuation byte as a word boundary, so `élan` looked like a standalone
  reference inside `béélan`. Both now work in characters.

- **A diff with no `diff --git` lines was parsed as one merged file.** Piping
  `diff -u`, `git format-patch` or a review tool's output to `--stdin` gives no
  `diff --git` line, and nothing else closed the previous file — so the second
  file's `+++` merely *renamed* the first, every hunk in the diff collapsed under
  the last path seen, and the pre-filter dropped the paths entirely (which also
  meant lockfiles and generated files in a piped diff were never filtered). The
  `--- old` / `+++ new` / `@@` sequence is now recognised as a file boundary in
  the parser, the pre-filter and unit splitting alike.

  Relatedly, `--- ` and `+++ ` were treated as path lines even *inside* a hunk,
  where they are content: a removed `-- note` in SQL, Lua or Haskell renders as
  `--- note`, so it was dropped from the hunk and its `+++` twin renamed the file
  to the comment's own text. Path lines are now only read outside a hunk.

- **A suppression comment with a written reason silently stopped suppressing.**
  `// diffmind-ignore false positive, validated upstream` parsed every word of
  the reason as a rule ID, matched nothing, and let the finding straight
  through. Rule IDs are now read up to the first word that is not shaped like
  one, and the rest is treated as prose, so a bare reason means "all rules" and
  `// diffmind-ignore DM002 -- the caller restores it` still scopes to `DM002`.
  A `:` after the marker is punctuation. If you use single-lowercase-word IDs in
  `rules.toml` (`id = "todo"`), prefix or hyphenate them — such a word now reads
  as prose, which over-suppresses on that line rather than under-suppressing.

- **The TUI's evidence pane was empty for cross-file findings.** It planned the
  review units itself to show the hunk behind a finding, but that plan ran
  before unit grouping and triage, both of which change which units exist and
  therefore what IDs findings carry — so a merged unit, exactly the cross-file
  change the code graph exists to link, resolved to nothing. Units now come from
  the analyzer as it reviews them, so the pane cannot disagree with the run.

- **The blast radius collapsed to one caller.** `callers_of` limited reference
  rows and deduplicated afterwards, so a function calling a changed symbol
  several times consumed the entire budget: three callers of six calls each,
  asking for three, returned one. Review context now sees the callers it was
  meant to. Results are also ordered — tightest enclosing scope first — where an
  unordered `LIMIT` previously returned whatever the query planner chose, so the
  same repository could produce different context from run to run.

- **A brace in the model's preamble threw away the whole unit.** JSON extraction
  tried the first `{` or `[` in the output and gave up if it was not valid,
  rather than looking further along — so `Looking at the diff {specifically
  auth.rs}, here is the review: {…}` yielded nothing and the unit was counted
  unparseable. A rejected opener is now skipped; only a genuinely unterminated
  one stops the scan, since anything after it is nested inside it. Affects the
  remote backends (Ollama, OpenAI-compatible), whose decoding cannot be
  constrained and which are therefore the likeliest to add a preamble.

- **`--seed` and `--temperature` were ignored whenever a daemon was running.**
  Neither was part of the daemon protocol, so a served review used whatever
  sampling `diffmind serve` had been started with, and the same diff could
  produce different findings depending on whether a daemon happened to be up.
  Both are now per-request. The daemon also re-asserts that caching is off above
  temperature 0 rather than trusting the client to have done so.

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

### Performance

All three of these ran before a single token was generated, so they were paid on
every review — including one that turned out to be fully cached.

- **The code graph is no longer queried per line.** Deciding whether two units
  were related asked the graph for the enclosing symbol of every line in a
  unit's span, which for a 1500-line unit was 1500 queries. One query for the
  span, resolved in memory: **17x faster** on that unit (122ms → 7ms), with a
  byte-identical result. Still the innermost declaration per line — taking every
  overlapping one would have been cheaper again, but it would make any two units
  inside the same module look related.

- **Referenced symbols resolve in one batch instead of one query per word.**
  Context assembly asked the graph about every distinct word in the diff as it
  encountered it, then looked the survivors up a second time. On a diff with
  ~1600 candidate words that was ~1600 queries; it is now a handful of batched
  ones, **9.7x faster**, resolving the same definitions.

- **Context is assembled once per unit, not twice.** The TUI asks for a unit's
  context when the unit starts, and the analyzer asks again when it builds that
  unit's prompt. The builder now memoises on the chunk text, so the second ask
  costs nothing (3.4ms → 9µs). Keyed by the text itself rather than a hash: a
  collision would hand one unit another's context, and confidently wrong context
  is the failure the whole module exists to prevent.

### Internal

- **End-to-end coverage for `--stdin`.** `apps/tui-cli/tests/stdin_pipeline.rs`
  runs the real binary with a diff on stdin and asserts on stdout and the exit
  code, with inference supplied by a stub `openai-compatible` endpoint rather
  than the bundled model — so no 1.1 GB download is a test dependency, and the
  path exercised is a shipped one rather than a test-only seam.
  The stub answers each request about whichever file it can see in the prompt,
  and records the prompts, so the tests can assert what the model was *shown* and
  not merely what came back.

  This is the coverage whose absence let one defect live in four places at once:
  each copy had unit tests, and none of them tested the four together. Reverting
  all four fixes now fails four of the six new tests.
