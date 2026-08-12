# Diffmind — a code review gate you can actually keep

[![CI](https://github.com/thinkgrid-labs/diffmind/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/diffmind/actions/workflows/ci.yml)
[![Latest Release](https://img.shields.io/github/v/release/thinkgrid-labs/diffmind)](https://github.com/thinkgrid-labs/diffmind/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

**Diffmind reviews a `git diff` and reports security issues, bugs and quality
problems** — from a single binary, on your machine, with no API key and no
network. It runs as a CI gate, a git hook, or an interactive terminal cockpit
for whoever has to review the branch.

The hard part of an automated reviewer is not producing findings. It is
producing them *the same way twice*, keeping false positives from accumulating
until someone deletes the job, and doing it cheaply enough to run on every push.
That is what this is built around.

---

## When diffmind is the right tool

Reach for it when you need review that is:

- **Reproducible** — greedy decoding at a fixed seed with a constrained JSON
  decoder. The same diff reviews the same way every time. A gate that flags a
  line on Tuesday and not on Wednesday gets deleted within a month.
- **Free per run** — no per-token bill, so it can run on every push, on every
  fork's PR, on a repo with no budget.
- **Offline and secretless** — nothing leaves the machine. No API key in CI, no
  bot account, no third-party data processor to get approved.
- **Stateful** — a baseline, inline suppressions, stable rule IDs and a
  recorded accept/wrong ratio. Review debt that a team carries for two years
  needs somewhere to live.

### When to use something else

Diffmind runs local models by default. **It is not smarter than a frontier
model, and does not try to be.** If you want the deepest possible read of one
tricky diff and you have an interactive agent and a budget, use those — they
will find things diffmind will not.

The intended shape is both: an agent when you are thinking hard about a single
change, and diffmind on the other several hundred, unattended, for free.

---

## Two tiers of trust

Findings are not all worth the same, and the output says which is which.

**Deterministic findings** — no model involved. Commented-out code (`DM001`),
a declaration removed while still referenced (`DM002`), your own regex rules
(`custom.<slug>`). These are pattern-true: they cost nothing, never vary, and
you can gate on them without thinking about it.

**Model findings** (`DM900.<category>`) — a model's judgement on a hunk, given
the enclosing function, the callers of what changed, referenced definitions and
the file's tests. Depth scales with the model you run: the default 1.5B is a
lint-grade reviewer that catches obvious mistakes; a 14B behind Ollama reads
much more like a colleague. Both are worth reading. Neither is worth trusting
blindly, which is why the cockpit records when they are wrong.

The split is deliberate. **Set `--fail-on` for the tier you trust** — many teams
gate on high-severity deterministic findings and let model findings report
without blocking.

---

## Install

```bash
# Linux & macOS — auto-detects arch, verifies the SHA-256, installs to /usr/local/bin
curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash

# npm — the binary ships in a per-platform optional dependency
npm install -g @diffmind/cli

# from source
cargo install --path apps/tui-cli
```

To pin a version, the variable goes on the **`bash`** side of the pipe:
`curl -fsSL … | VERSION=v0.9.0 bash`.

Windows: download `diffmind-x86_64-pc-windows-msvc.zip` from
[Releases](https://github.com/thinkgrid-labs/diffmind/releases) and put
`diffmind.exe` on your `PATH`. Linux binaries are glibc-linked; on musl
(Alpine) use a glibc base image or build from source.

---

## Quick start

```bash
diffmind download          # one-time model download (~1.1 GB)
diffmind index             # build the code graph — recommended, big context win
diffmind                   # review this branch against the repo's default branch
```

```bash
diffmind --last                  # just the last commit
diffmind --staged                # just what's staged
diffmind v1.2.0..HEAD            # an explicit revision range
diffmind src/auth/               # just these paths
diffmind v1.2.0..HEAD src/api/   # a range, narrowed to paths
diffmind --tui                   # the reviewer's cockpit
```

The base branch is read from `origin/HEAD`, falling back to whichever of
`main` / `master` / `develop` / `trunk` exists — it is not assumed to be `main`.
A bare `a..b` argument is recognised as a revision range without `--range`; the
detection is strict, so `diffmind ../lib` and a file genuinely named `a..b` are
still treated as paths.

---

## The gate

### GitHub Action

```yaml
- uses: thinkgrid-labs/diffmind@v0.9.0
  with:
    model: 1.5b
    fail-on: high
```

Caches the model, installs the binary, reviews the PR diff, and uploads SARIF so
findings appear inline on the diff — no bot account, no token, no secret. For a
PR comment instead, set `format: markdown`, `comment: true`, `fail-on: none`.

Requires `permissions: { security-events: write }` for SARIF, or
`pull-requests: write` for comments.

### Any other CI

```bash
git diff origin/main...HEAD | diffmind --stdin --format sarif --output diffmind.sarif --fail-on high
```

| Exit | Meaning |
| ---- | ------------------------------------------------ |
| `0`  | Clean, or nothing at or above `--fail-on`        |
| `1`  | Findings at or above `--fail-on`                 |
| `2`  | diffmind itself failed (bad flag, missing model) |

`1` and `2` are distinct, so a crashed binary never looks like a failed review.

### Git hooks

```bash
diffmind install-hooks --hook pre-push --min-severity high
diffmind install-hooks --hook pre-commit
```

The generated hook exits 0 when diffmind isn't installed, so it never blocks a
teammate who hasn't set it up, and refuses to overwrite a hook it didn't write
unless you pass `--force`. Also available through
[pre-commit](https://pre-commit.com) — `repo: https://github.com/thinkgrid-labs/diffmind`, `id: diffmind`.

---

## The cockpit — `diffmind --tui`

The gate is public and must be conservative. The cockpit is private, so it can
afford to show you more.

Analysis starts on launch. Each finding shows the **actual hunk the model
reviewed** and the **context it was given** — a finding you cannot check is one
you will eventually stop reading.

| Key | Action |
| --------------- | -------------------------------------------------------- |
| `j` / `k`       | Move through findings |
| `a`             | Accept — records the verdict and copies a review comment |
| `d`             | Dismiss — read, not worth raising |
| `w`             | Wrong — the finding was incorrect |
| `PgUp` / `PgDn` | Scroll the detail pane |
| `r`             | Re-run |
| `q`             | Quit |

Verdicts are written through immediately, so closing the terminal mid-triage
loses nothing. Accept copies via **OSC 52**, the terminal's own clipboard
escape — no dependency, and it works over SSH. tmux and screen need clipboard
passthrough; if the copy fails you are told, so you never believe you have
copied something you have not.

---

## Keeping the gate alive

Noise is this category's failure mode. One false positive that cannot be
silenced is how a review job gets deleted. Every finding therefore carries a
stable rule ID, shown in the output, so you can silence exactly one thing.

### Inline

```js
// diffmind-ignore-next-line DM001
// const legacy = oldPath();

const x = 1; // diffmind-ignore

/* diffmind-ignore-file DM900.maintainability */
```

Listing no rule IDs suppresses everything at that location. `DM900` suppresses
every model category without listing each one.

### Baseline

Adopting a reviewer on an existing codebase shouldn't mean fixing everything
first.

```bash
diffmind baseline create     # record today's findings as accepted
diffmind baseline show
diffmind baseline clear
```

Commit `.diffmind/baseline.json`; future runs report only new issues. The
baseline keys on a content fingerprint rather than a line number, so it survives
unrelated edits above the finding.

### Measuring whether it earns its keep

Every review is filed to `.diffmind/runs/<sha>/`. `diffmind stats` reads them
back:

```
  Runs             34
  Median findings  3
  Median time      6.2s
  Cache hits       61%

  Verdicts         71 accepted · 44 dismissed · 12 wrong
  Accept : wrong   5.9:1  (on target)

  Most often wrong
      7  DM900.maintainability
      3  rulebook.house-style
```

The **accept-to-wrong ratio** is the number that decides whether to keep running
it — a reviewer who cannot measure noise will just quietly stop running the
reviewer. Verdicts come from the cockpit. Dismissals are excluded: choosing not
to raise a correct observation is not the tool being wrong.

Run snapshots are overwritten when a sha is reviewed again; verdicts are
append-only and survive `diffmind stats --clear`, because the ratio is only
meaningful over months.

Diffmind writes `.diffmind/.gitignore` covering `runs/`, `cache/`, `models/`,
`graph.db` and `daemon.json` — your review notes stay private, while `rules/`,
`rules.toml`, `config.toml` and `baseline.json` remain committable. Your
repository's own `.gitignore` is never touched.

---

## Your team's standards

### Prose rules — `.diffmind/rules/*.md`

Rules that need judgement rather than a pattern. Committed to the repo and read
by the model on every review, so review culture becomes a versioned artifact
instead of tacit knowledge.

```markdown
---
scope: ["src/api/**/*.ts"]
severity: high
---

# API conventions

- Public handlers must return `ApiError`, never a bare string.
- Any new endpoint needs a corresponding entry in `openapi.yaml`.
- Reject changes that widen a response struct without a version bump.
```

`scope` globs the files a rule set governs (omit for the whole repo); `severity`
is a **ceiling** for findings attributed to it, never a promotion; `id` defaults
to the file stem. Scaffold with `diffmind rules init`, check what loads with
`diffmind rules list`.

A finding attributed to a rule set gets the ID `rulebook.<id>` and suppresses
like any other. An attribution naming a rule set that does not govern that file
is discarded — a small model will invent a plausible name, and an invented one
could never be suppressed.

Rule bodies go in the *stable* half of the prompt and units are grouped by which
rule sets govern them, so every unit in a group sends a byte-identical prefix.
That is what keeps prompt-prefix caching possible. A rule set that fails to
parse is reported and skipped, never silently ignored.

### Pattern rules — `.diffmind/rules.toml`

Regex, matched against added lines before the model runs: instant,
deterministic, zero inference cost.

```toml
[[rule]]
id       = "no-hardcoded-password"
pattern  = "password\\s*=\\s*[\"'][^\"']+[\"']"
message  = "Hardcoded password — use a secrets manager"
fix      = "Use a secrets manager"
severity = "high"        # high | medium | low                            (default medium)
category = "security"    # security | quality | performance | maintainability
files    = ["*.ts", "**/generated/**"]
```

Only `pattern` and `message` are required; `id` defaults to a slug of the
message.

---

## Models and backends

**The local model is the default and stays the default.** Everything else here
is opt-in.

Models download to `~/.diffmind/models/` — all Qwen2.5-Coder, Q4_K_M quantised.
Inference runs on the Apple Silicon GPU via Metal where available, and on CPU
(with Accelerate/BLAS) everywhere else. Downloads are atomic and checksummed, so
an interrupted download cannot leave a truncated file that looks valid.

| Model | Size | Min RAM | Use for |
| -------------------- | ------- | ------- | ----------------------------------- |
| Qwen2.5-Coder-0.5B   | 0.4 GB  | 2 GB    | CI on small runners, lint-grade     |
| Qwen2.5-Coder-1.5B ★ | 1.1 GB  | 4 GB    | Default — balanced                  |
| Qwen2.5-Coder-7B     | 4.7 GB  | 10 GB   | Noticeably better security analysis |
| Qwen2.5-Coder-32B    | 20.0 GB | 40 GB   | Maximum, if you have the machine    |

`3b` and `14b` also exist. `diffmind download` gives an interactive picker with
a hardware check; `--model 1.5b --verify` checks an existing download.

### Your own model server

"Local" doesn't have to mean "in this process". If you already run Ollama, vLLM
or LM Studio, point diffmind at it — your code still never leaves your machine
or your network, and you get a model class diffmind will never ship as a 20 GB
download.

```bash
diffmind --backend ollama --backend-model qwen2.5-coder:14b

# anything speaking the OpenAI chat API — vLLM, LM Studio, llama.cpp, LiteLLM
diffmind --backend openai-compatible --backend-url http://localhost:8000/v1 --backend-model my-model
```

API keys are read from the environment only (`DIFFMIND_API_KEY`, override with
`--backend-api-key-env`) — never from the config file, which gets committed.

> **What you trade for a bigger model.** The constrained JSON decoder hooks the
> bundled sampler directly, so remote backends cannot use it: you get better
> judgement and lose the same-diff-same-result guarantee. Token counts reported
> by remote endpoints are estimates, marked `~`. Reproducibility is a property
> of the local path.

Hosted backends are planned on the same terms — additive, opt-in, never the
default, and subject to the same trade-off above.

### Daemon mode

Every invocation otherwise pays the model-load cost — seconds, every time.

```bash
diffmind serve            # loads the model, unloads after 10 idle minutes
diffmind serve --status
diffmind serve --stop
```

Reviews automatically use a running daemon whose model and device match;
`--no-daemon` opts out. It listens on `127.0.0.1` only and requires a
per-instance token in a `0600` file, so no other user on the machine can submit
work to it.

---

## How it works

1. **Parse** — the diff becomes typed per-file hunks with real pre/post-image
   line numbers.
2. **Pre-filter** — lockfiles, `linguist-generated` paths, `@generated` banners,
   minified bundles, assets, snapshots, your `ignore` globs and whitespace-only
   hunks are dropped. Costs nothing, typically removes most of a real branch,
   and the counts are reported rather than silently applied:
   `312 hunks → 74 reviewable (238 filtered: lockfiles, generated, formatting)`.
   Whitespace inside a string literal counts as content; indentation is never
   dismissed in Python or YAML.
3. **Deterministic detectors** — `DM001`, `DM002` and your regex rules. No model
   involved.
4. **Context** — assembled per unit from `.diffmind/graph.db`: the enclosing
   definition, the callers of every changed symbol, definitions of referenced
   symbols, and the corresponding test file. Bounded by a byte budget, so
   context does not grow with the repository. Bodies are read from the working
   tree, so a snippet can never disagree with the file being reviewed.
5. **Triage** — on large diffs, a cheap first pass decides which files carry real
   risk.
6. **Review units** — hunks are grouped into regions of a file rather than cut
   wherever a line budget ran out, so related hunks are read together and an
   edit in one function only re-reviews that function. When a changed symbol and
   a changed caller both appear, the two are merged into one unit and reviewed
   together. Units are sized to the backend's *actual* context window, read from
   the GGUF metadata.
7. **Constrained decoding** — the sampler consults a JSON state machine before
   committing each token, so the model cannot emit a preamble, an unbalanced
   brace or a truncated string. Output that hits the token cap is repaired
   rather than discarded.
8. **Anchoring** — findings pointing at a file not in the diff are dropped;
   off-by-N line numbers snap to the nearest changed line.
9. **Suppression** — inline directives, the baseline and `--min-confidence` are
   applied, then results are deduplicated and sorted.

The **code graph** behind step 4 is a tree-sitter symbol index over 13
languages — Rust, TypeScript, TSX, JavaScript, Python, Go, Java, C#, Ruby, PHP,
C, C++ and Scala — kept in `.diffmind/graph.db` and updated incrementally by
mtime. Build it with `diffmind index`. Adding a language is one entry in a
table; contributions welcome.

---

## Configuration

`.diffmind/config.toml` — precedence is CLI flag > config file > default.
Unknown keys are reported rather than silently ignored.

```toml
[review]
branch         = "develop"
model          = "3b"
min_severity   = "low"      # what gets reported
fail_on        = "high"     # what fails the build
min_confidence = 0.0        # detectors score 0.9+; unscored model findings 0.5
triage         = "auto"     # two-pass triage on large diffs
cache          = true
temperature    = 0.0        # 0 = greedy and reproducible
max_tokens     = 1024
ignore         = ["**/legacy/**", "*.generated.ts"]   # on top of the built-in noise rules

[backend]
kind        = "local"       # or "ollama" / "openai-compatible"
url         = "http://localhost:11434"
model       = "qwen2.5-coder:14b"
api_key_env = "DIFFMIND_API_KEY"
```

### Options that matter

| Flag | Effect |
| ----------------------- | -------------------------------------------------- |
| `-b, --branch`          | Base branch [default: the repo's default branch] |
| `-m, --model`           | `0.5b` … `32b` [default: `1.5b`] |
| `-l, --last` / `--staged` / `--range` / `--stdin` | What to review |
| `-t, --tui`             | Launch the cockpit |
| `--min-severity`        | Minimum severity to report [default: `low`] |
| `--fail-on`             | Severity causing exit 1 [default: `--min-severity`] |
| `--min-confidence`      | Minimum confidence to report, 0.0–1.0 |
| `-f, --format`          | `text`, `json`, `sarif`, `markdown` |
| `-o, --output`          | Write the report to a file |
| `--ticket <FILE_OR_TEXT>` | Check the diff against acceptance criteria |
| `--backend` / `--backend-url` / `--backend-model` | Use your own model server |
| `--no-cache` / `--no-baseline` / `--no-daemon` | Opt out |

`--triage`, `--temperature`, `--seed`, `--max-tokens`, `--device` and `--debug`
are in `diffmind --help`.

The binary also carries two conveniences that are not the point of the tool:
`diffmind describe` (a PR title and summary) and `diffmind commit` (a
conventional commit message for staged changes). Use them if they're handy; they
are not what diffmind is for.

---

## Roadmap

- Hosted backends — opt-in only, with the local model still the default
- Auto-fix patches (`diffmind fix`) — gated on those backends, since a 1.5B's
  patches are not trustworthy enough to apply
- Cheap-model triage feeding a strong-model deep pass, and prompt-prefix caching
- Learning from verdicts: marking a finding wrong should suppress its kind next
  time, not merely count it
- VS Code / JetBrains extensions, talking to the daemon
- Homebrew tap and scoop manifest

---

## Contributing

Issues and pull requests welcome at
[github.com/thinkgrid-labs/diffmind](https://github.com/thinkgrid-labs/diffmind).

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

MIT — see [LICENSE](LICENSE).

> **Diffmind** — review that runs where your code already is.
