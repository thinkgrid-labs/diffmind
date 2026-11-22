# Changelog

## [0.9.3] — 11-22-2026

A stabilization release: security, correctness and reliability hardening, with
no new review features. The headline is that **a review that did not cover the
whole diff can no longer pass as a clean one.**

### Changed — behaviour you may notice

- **Incomplete reviews fail in CI (exit 2).** A unit whose model output could
  not be parsed, or that was too large for the context window, used to be
  counted and then ignored: a review could report "No issues found", exit 0,
  and have looked at none of the code. A review is now *complete*,
  *incomplete* or *failed*, and an incomplete one exits 2 when completeness is
  required. It is required by default when the `CI` environment variable is
  set, and can be set with `--require-complete` / `--allow-incomplete` or
  `[review] require_complete` in `.diffmind/config.toml` (the flags win, so a
  pull request cannot relax a gate the workflow set). Local runs default to
  reporting the skip loudly and letting `--fail-on` decide.
- **A backend failure part-way through a review still exits 2, but now writes
  the report first** — deterministic findings, every unit reviewed before the
  failure, and which units were not — instead of a bare error.
- **Every machine-readable format says whether the review was complete.** JSON
  gains a `completeness` object (`status`, `units_planned`, `units_reviewed`,
  `units_skipped` with reasons, `trustworthy`, …). SARIF sets
  `invocations[0].executionSuccessful` and lists skipped units as
  `toolExecutionNotifications`. Markdown leads with a warning. Existing fields
  are unchanged.
- **The review cache is invalidated once on upgrade.** Keys now cover the exact
  rendered prompt and the backend's identity (see Security), so 0.9.2 entries
  are never reused.
- **GitHub Action: pull requests are reviewed under the base branch's
  `.diffmind/` configuration.** `config.toml`, `rules.toml`, `rules/` and
  `baseline.json` are restored from the merge base before the review, so a PR
  cannot relax the gate that reviews it through those files (`ignore =
  ["**"]`, `min_confidence = 1.0` or a baseline listing its own findings all
  used to pass). Changes to those files are still reviewed and take effect
  once merged; set `config-from: head` for the old behaviour. This needs a
  base branch: on `merge_group`, `push` and other events without one, the
  checkout's own `.diffmind/` still governs unless the `base` input is set.
  It also does not cover the generated-file filter: a PR that marks a file
  `linguist-generated` in `.gitattributes`, or gives it an `@generated`
  banner, still drops that file from review, deterministic checks included.
  The local backend is also pinned unless `args` chooses one. `args` is now
  split like a quoted shell string but never evaluated; an `args` value that
  relied on shell expansion no longer expands. The action requires a complete
  review; set `allow-incomplete: true` to let an incomplete one pass.
- **Local-model reviews take about 10–15 % longer on CPU.** The
  prompt-injection boundary (see Security) adds roughly a quarter more prompt
  text per unit. On a GitHub-hosted Linux x86_64 runner, a two-unit review
  with the bundled 1.5B model went from 232 s to 259 s (+11.8 %, median of
  3). Peak memory is unchanged.
- **The npm wrapper exits 2, not 1, when it cannot run the binary** (missing
  platform package, unsupported platform, spawn failure). 1 means "blocking
  findings".
- **An API key is refused over plain HTTP to a non-loopback host.** Use
  `https://`, a loopback address, or set `DIFFMIND_ALLOW_INSECURE_API_KEY=1`.

### Fixed — security

- **Daemon token from the OS CSPRNG.** `diffmind serve` derived its bearer token
  from the pid, the clock, a stack address and `$USER`. It is now 256 bits from
  the operating system, and the daemon refuses to start if none is available.
- **Daemon token file is private from the first byte.** It was written with the
  umask's permissions and `chmod`ed afterwards (ignoring a `chmod` failure); it
  is now created `0600`, written to a temporary file and renamed into place.
  The token is redacted from `Debug` output.
- **Daemon clients authenticate the daemon.** Every connection starts with a
  challenge (a random nonce answered with an HMAC under the token), and the
  token and diff are sent only if it verifies. A process squatting on a
  crashed daemon's port no longer receives either.
- **The daemon bounds what an unauthenticated peer can cost it.** Requests are
  capped at 16 MiB and must arrive whole within 30 s; previously any local
  process could make it buffer without limit, or hold the (sequential) daemon
  indefinitely by trickling bytes.
- **GitHub Action inputs are no longer interpolated into shell or JavaScript.**
  Every `${{ inputs.* }}` inside a `run:` or `github-script` block was pasted
  into the source before it was parsed, so an input such as `$(…)` executed.
  Inputs now travel as environment variables, are validated (model, severity,
  format, version, base ref via `git check-ref-format`, output path), and are
  passed as argv entries.
- **Review cache entries cannot be planted by a branch.** The cache lives in the
  repository, and every input to a key was public, so a branch could ship an
  entry saying "no findings" for its own change. Keys are now an HMAC under a
  per-user secret in `~/.diffmind/cache-secret`, and each entry records the key
  it was written for.
- **Context assembly no longer follows symlinks out of the repository.** The
  "test file" for `src/auth.ts` could be a committed symlink to any file the
  reviewer can read, quoted into the prompt. Reads are now confined to the
  repository, without `..` or absolute paths, and size-capped.
- **Model output is inert in Markdown.** Findings are rendered into PR comments
  and job summaries; model text derived from planted instructions could emit
  images (fetched on render — an exfiltration channel), HTML, links or
  `@`-mentions. All of it is now escaped. Control characters and bidi
  overrides are stripped from model output in every format.
- **A configured base branch starting with `-` is refused** before it reaches
  `git` as an option.
- **Credentials never reach a label, report, run record or error.** Remote
  backend URLs are shown without `user:password@` or query string, reqwest
  errors are stripped of their URL, and an echoed API key is masked.

### Fixed — correctness and reliability

- **GitHub Action: a failed fetch or diff fails the job.** The action ran
  `git fetch … || true` and then `git diff origin/<base>...HEAD`. With
  `actions/checkout`'s default depth of 1 there is no merge base, so the diff
  failed, diffmind received an empty diff, wrote a clean SARIF report — which
  was uploaded — and the job exited with an unexplained 128. The action now
  deepens a shallow clone until the merge base is reachable, and exits 2 with a
  clear error if it cannot.
- **GitHub Action: `fail-on: none` no longer fails on the first finding.** With
  no `--fail-on`, the CLI defaults the gate to `--min-severity` (default
  `low`). `none` now reports everything and maps "blocking findings" to a pass,
  while still failing a review that did not complete. Abnormal exits (a crash,
  an OOM kill) are reported as 2; a new `status` output carries the exit code.
- **Cache keys cover everything that changes the answer.** Keys were built from
  a hand-maintained list of prompt ingredients that omitted the rendered
  context and rule-set budgets; they are now taken over the exact prompt, the
  sampling options, and the backend's identity — including the pinned GGUF
  digest for the local model and Ollama's model digest for a re-pulled tag.
  OpenAI-compatible servers expose no revision; their identity says
  `revision=unverified`, and `diffmind cache clear` is the remedy if the model
  behind a name changes. The cache is also pruned (it never was).
- **Code-graph freshness no longer trusts mtime alone.** An edit within one
  timestamp tick, or one that restored an old mtime (`cp -p`, `rsync -t`), left
  stale symbols and line ranges in the graph. Freshness now uses mtime, inode
  change time and size, re-hashes "racily clean" files, and compares content.
  A file that stops parsing drops its old symbols instead of keeping them.
- **Context is withheld when the working tree is not the diff's post-image**
  (`--range a..b` with another commit checked out, committed work reviewed with
  uncommitted edits on top), instead of describing whatever now sits at those
  line numbers. The run says so, once, naming a file.
- **Non-ASCII paths** are diffed with `core.quotepath=false`, so context,
  anchoring and generated-file checks find the file.
- **Ambiguous graph relationships are labelled as such.** A caller or
  definition matched by name when several definitions share it is shown as
  "Possible caller/definition", not as a resolved dependency.
- **A corrupt `graph.db` is rebuilt** instead of silently disabling code context
  on every later review. Concurrent indexers wait for each other rather than
  failing with "database is locked", and opening an up-to-date graph no longer
  takes the write lock.
- **Remote backends are bounded.** A 10 s connect timeout separate from the
  generation timeout; responses capped at 8 MiB; HTTP 429/5xx and dropped
  connections retried at most twice with backoff (honouring `Retry-After` up to
  30 s); timeouts and other 4xx not retried; redirects not followed.
- **Prompt-injection boundary.** Diff, surrounding code, ticket text and triage
  previews are fenced with markers the content cannot forge (built in linear
  time), chat-template control tokens such as `<|im_end|>` in repository text
  are broken up so they cannot close the prompt's turn, and the system prompt
  states that fenced text is data, never instructions. Model findings
  must name a file in the unit that produced them, and a model can no longer
  assign a rule id or unit id itself. This reduces, and does not eliminate,
  prompt-injection risk; see the README.
- **`--stdin` diffs are size-capped** at the same 64 MB as git-produced ones.

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
