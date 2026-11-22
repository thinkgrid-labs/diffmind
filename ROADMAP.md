# Roadmap

This is the plan after v0.9.3. It is a statement of direction, not a set of
promises. Milestones have no dates, and their scope may change. Discussion and
pull requests are welcome in
[issues](https://github.com/thinkgrid-labs/diffmind/issues).

Two things will not change: **the built-in local model stays the default**, and
hosted models, when they arrive, are opt-in.

## Shipped

Highlights of what exists today. Details are in the [CHANGELOG](CHANGELOG.md).

- **v0.9.3** (stabilization): incomplete reviews fail in CI (exit 2) and every
  report says whether the review was complete; the GitHub Action reviews pull
  requests under the base branch's configuration and no longer interpolates
  inputs into shell; HMAC-keyed review cache; content-checked code-graph
  freshness; bounded remote backends; prompt-injection boundary; hardened daemon
  authentication.
- **v0.9.0–v0.9.2**: tree-sitter code graph (13 languages) with callers and
  definitions as review context; interactive TUI with accept / dismiss / wrong
  verdicts and `diffmind stats`; scoped written rule sets (`.diffmind/rules/*.md`)
  with severity ceilings and drop reporting; filtering of generated files, docs
  and agent config.
- **Earlier**: GitHub Action with SARIF, Ollama and OpenAI-compatible backends,
  JSON-constrained decoding, baselines, inline suppressions, regex rules, git
  hooks and pre-commit integration, npm and shell installers, daemon mode.

## Next milestones

### v0.9.4: Gate integrity and first-run experience

**Goal.** Close the known gaps in the CI gate, and make the first five minutes
work without reading the source.

**Main features**

- Derive the base for `merge_group` events, so the base-branch configuration
  protection also covers merge queues.
- Stop a pull request from removing its own files from review through
  generated-file markers, or at minimum report those files in the completeness
  summary.
- Tighter completeness accounting: a unit only counts as reviewed when the
  model's answer is actually usable, and files skipped by triage are visible
  in the completeness summary.
- Repository config cannot silently send code to a remote backend: a
  `[backend]` from a committed `config.toml` needs explicit confirmation, and
  the API-key variable is restricted.
- Small action and CLI fixes: clear stale report files before a run, paginate
  the PR-comment lookup, give `serve --status` a meaningful exit code.
- A troubleshooting page and clearer errors for the common first-run failures
  (no model, no merge base, unsupported platform, backend unreachable).

**Developer impact.** Teams can trust that a green check means the change was
reviewed under the configuration they chose.
**Priority.** P0.
**Dependencies.** None.
**Risk.** Low. The changes are narrow, but each touches the gate's exit
behaviour and needs action-harness coverage.
**Acceptance criteria.** Each fixed gap has a regression test in the Rust suite
or the action harness. CI is green on Linux, macOS and Windows. The README's
qualifications about the gate are updated to match.

### v0.10.0: Review quality you can measure, and faster CPU reviews

**Goal.** Make findings more trustworthy, and show the evidence publicly.

**Main features**

- **Gate on the tier you trust.** An option to fail only on deterministic
  findings (`DM001`, `DM002`, `custom.*`), or to set separate thresholds for
  deterministic and model findings. Today `--fail-on` applies to both.
- **A public review-quality benchmark.** A fixture set of planted defects and
  clean changes across the graph languages, run in CI, reporting detection and
  false-positive rates per model size.
- **Learn from verdicts.** Findings marked *wrong* in the TUI feed back into
  later reviews instead of only being counted.
- **Recover CPU time.** Shorten the prompt-injection boundary text and reuse
  identical prompt prefixes, to win back the ~12% v0.9.3 added on CPU.
- Explicit owner-only permissions for private files on Windows.

**Developer impact.** Fewer false alarms in CI, a clear way to block only on
deterministic findings, and published numbers to judge the tool by.
**Priority.** P1.
**Dependencies.** The benchmark should land first, so changes to prompts and
thresholds can be measured.
**Risk.** Medium. Quality work can regress without a benchmark, and prompt
changes invalidate the review cache.
**Acceptance criteria.** Benchmark results are published in the repository and
reproducible with one command. A deterministic-only gate is documented and
tested. Median CPU review time is at or below v0.9.2 on the benchmark.

### v0.11.0: Opt-in hosted models

**Goal.** Let teams spend money on the diffs that deserve a stronger model,
without changing the default.

**Main features**

- A built-in Anthropic backend, and an opt-in `claude-cli` backend that reuses
  an existing subscription. API-key authentication from the environment only.
- Real token counts and an estimated cost per run, shown in `diffmind stats`.
- **Cheap model first.** The local model triages every hunk, and only the
  risky ones go to the hosted model.
- Privacy controls: hosted backends need explicit opt-in, and paths can be
  excluded from remote review.

**Developer impact.** Better findings on high-risk changes at a predictable
cost, while day-to-day review stays local and free.
**Priority.** P1.
**Dependencies.** Triage quality from v0.10.0. Without a measured triage pass,
routing is guesswork.
**Risk.** Medium. Provider APIs change. Code leaves the machine, so the docs
and defaults must make that unmistakable.
**Acceptance criteria.** Hosted backends are off unless configured. Each one is
covered by tests against a recorded or stub server. The providers page
documents exactly what is sent. The local default and its tests are unchanged.

## Later (not scheduled)

- **Auto-fix patches** (`diffmind fix`), applied only with explicit approval.
  This waits on stronger backends, because small-model patches are not safe
  enough to apply.
- **Editor integrations** (VS Code, JetBrains) that talk to the daemon.
- **More languages** in the code graph, one `LANGUAGES` entry at a time.
- **More install channels**: Homebrew tap, Scoop manifest, musl Linux builds.
- Better PR experience: review annotations beyond SARIF, and per-finding
  feedback from the PR.

## Not planned

- Making a hosted model the default.
- Requiring an account, a hosted index or a server to use Diffmind.
