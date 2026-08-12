# Diffmind — Local AI Code Review for the Terminal

[![CI](https://github.com/thinkgrid-labs/diffmind/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/diffmind/actions/workflows/ci.yml)
[![Latest Release](https://img.shields.io/github/v/release/thinkgrid-labs/diffmind)](https://github.com/thinkgrid-labs/diffmind/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

**Diffmind** is a free, open-source AI code review tool that runs entirely on your machine — no cloud, no API keys, no subscription. It analyzes your `git diff` using a local [Qwen2.5-Coder](https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF) model and reports security issues, bugs, and code quality problems directly in your terminal.

Your source code never leaves your environment. Works offline. Ships as a **single self-contained binary** for Linux, macOS, and Windows.

---

## Why Diffmind?

> The only AI code reviewer that keeps your code 100% private.

|                | **Diffmind**                       | Cloud AI review (Copilot, CodeRabbit, etc.) |
| -------------- | ---------------------------------- | ------------------------------------------- |
| **Privacy**    | Code stays on your machine         | Code sent to third-party servers            |
| **Cost**       | Free — one-time model download     | Per-token billing or subscription           |
| **Offline**    | Works with no internet after setup | Requires connectivity                       |
| **CI/CD**      | Single binary, no runtime deps     | Needs API key management and secrets        |
| **Compliance** | No data residency concerns         | Data may cross jurisdictions                |

---

## Features

- **Deterministic pre-filter** — lockfiles, generated files, minified bundles, assets and formatting-only hunks are dropped before the model sees them, and the run tells you exactly what it skipped: `312 hunks → 74 reviewable (238 filtered: lockfiles, generated, formatting)`.
- **Deterministic detectors** — commented-out code, removed-but-still-used declarations, and your own regex rules. These run before the model, cost nothing, and are the findings you can trust unconditionally.
- **Security, bug, performance and maintainability review** by a local model
- **Review standards as markdown** — commit your team's own rules to `.diffmind/rules/`, scoped by path. Prose the model reads, versioned next to the code it governs.
- **Ticket-aware review** — check the diff actually implements the acceptance criteria (`--ticket`)
- **Suppressions** — inline `// diffmind-ignore` comments and a project baseline, so one false positive doesn't get the whole gate deleted
- **SARIF output** — inline PR annotations via GitHub Code Scanning, no bot account or token
- **Pluggable backends** — the bundled GGUF, or your own Ollama / vLLM / LM Studio endpoint
- **Daemon mode** — keep the model resident so reviews are near-instant
- **Local RAG** — feeds the model the *enclosing function* of each hunk, not just the diff
- **Reproducible** — greedy decoding with a fixed seed: the same diff always reviews the same way
- **Reviewer's cockpit** (`--tui`) — analyses on launch, shows the hunk and context behind each finding, and records accept / dismiss / wrong so the signal-to-noise ratio is measured rather than guessed
- JSON / Markdown output, and a proper CI gate

---

## Installation

### Linux & macOS — one command

```bash
curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash
```

Auto-detects your OS and CPU architecture, verifies the SHA-256 checksum, and installs to `/usr/local/bin`.

Pin a specific version — note that the variable goes on the **`bash`** side of the pipe, not the `curl` side:

```bash
curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | VERSION=v0.9.0 bash
```

### Windows

Download `diffmind-x86_64-pc-windows-msvc.zip` from [Releases](https://github.com/thinkgrid-labs/diffmind/releases), extract it, and put `diffmind.exe` on your `PATH`.

### npm

```bash
npx @diffmind/cli --help
npm install -g @diffmind/cli
```

`@diffmind/cli` is a launcher — the binary ships in a per-platform package declared as an optional dependency, so npm downloads only the one matching your machine. Linux binaries are glibc-linked; on musl (Alpine) use a glibc base image or build from source.

### Build from source

```bash
git clone https://github.com/thinkgrid-labs/diffmind
cd diffmind
cargo install --path apps/tui-cli
```

---

## Quick Start

```bash
diffmind download          # one-time model download (~1.1 GB)
diffmind index             # optional: index symbols for context-aware reviews
diffmind                   # review this branch against the repo's default branch
```

`--branch` is no longer assumed to be `main`: diffmind reads the repository's default branch from `origin/HEAD` and falls back to whichever of `main`/`master`/`develop`/`trunk` actually exists.

A bare `a..b` argument is recognised as a revision range without needing `--range`. The detection is strict — both endpoints must resolve as revisions and the string must not name an existing path — so `diffmind ../lib` and a file genuinely called `a..b` are still treated as paths. Paths after a range narrow it: `diffmind v1.2.0..HEAD src/api/`.

```bash
diffmind --last            # just the last commit
diffmind --staged          # just what's staged
diffmind v1.2.0..HEAD      # an explicit revision range
diffmind src/auth/         # just these paths
diffmind --tui             # the reviewer's cockpit
```

---

## AI Model Setup

Models download to `~/.diffmind/models/`. All are **Qwen2.5-Coder**, Q4_K_M quantised. Inference runs on the Apple Silicon GPU via Metal where available, and on CPU (with Accelerate/BLAS) everywhere else.

```bash
diffmind download                      # interactive picker with a hardware check
diffmind download --model 3b
diffmind download --model 1.5b --force # re-download
diffmind download --model 1.5b --verify # check an existing download's checksum
```

| Model                | Size    | Min RAM | Notes                                     |
| -------------------- | ------- | ------- | ----------------------------------------- |
| Qwen2.5-Coder-0.5B   | 0.4 GB  | 2 GB    | Fastest — lint-style, CI, low-end hardware |
| Qwen2.5-Coder-1.5B ★ | 1.1 GB  | 4 GB    | Recommended — balanced                    |
| Qwen2.5-Coder-3B     | 2.1 GB  | 6 GB    | Deeper reasoning                          |
| Qwen2.5-Coder-7B     | 4.7 GB  | 10 GB   | Strong security analysis                  |
| Qwen2.5-Coder-14B    | 9.0 GB  | 18 GB   | Expert                                    |
| Qwen2.5-Coder-32B    | 20.0 GB | 40 GB   | Maximum                                   |

Downloads are atomic and checksummed: an interrupted download can no longer leave a truncated file that looks valid.

---

## Using your own model server

The local GGUF is the default and the point of the tool, but "local" doesn't have to mean "in this process". If you already run Ollama, vLLM, or LM Studio, point diffmind at it — your code still never leaves your machine or your network, and you get a model class diffmind will never ship as a 20 GB download.

```bash
# Ollama (default URL http://localhost:11434)
diffmind --backend ollama --backend-model qwen2.5-coder:14b

# Anything speaking the OpenAI chat API — vLLM, LM Studio, llama.cpp server, LiteLLM
diffmind --backend openai-compatible \
         --backend-url http://localhost:8000/v1 \
         --backend-model my-model
```

API keys are read from the environment only (`DIFFMIND_API_KEY` by default, override with `--backend-api-key-env`) — never from the config file, which gets committed.

---

## Suppressions

Findings carry a stable rule ID, shown in the output, so you can silence exactly one thing.

| Rule ID          | Meaning                                            |
| ---------------- | -------------------------------------------------- |
| `DM001`          | A code block was commented out instead of deleted  |
| `DM002`          | A declaration was removed but is still referenced   |
| `DM900.<category>` | A model-authored finding of that category        |
| `custom.<slug>`  | One of your `.diffmind/rules.toml` rules           |
| `rulebook.<id>`  | A violation of one of your `.diffmind/rules/*.md`  |

### Inline

```js
// diffmind-ignore-next-line DM001
// const legacy = oldPath();

const x = 1; // diffmind-ignore

/* diffmind-ignore-file DM900.maintainability */
```

Listing no rule IDs suppresses everything at that location. `DM900` suppresses every model category without listing each one.

### Baseline

Adopting diffmind on an existing codebase shouldn't mean fixing everything first.

```bash
diffmind baseline create     # record today's findings as accepted
diffmind baseline show
diffmind baseline clear
```

Commit `.diffmind/baseline.json`. Future runs report only new issues. The baseline keys on a content fingerprint rather than a line number, so it survives unrelated edits above the finding.

---

## CI/CD

### GitHub Action

```yaml
- uses: thinkgrid-labs/diffmind@v0.9.0
  with:
    model: 1.5b
    fail-on: high
```

That caches the model, installs the binary, reviews the PR diff, and uploads SARIF so findings appear inline on the diff. For a PR comment instead:

```yaml
- uses: thinkgrid-labs/diffmind@v0.9.0
  with:
    format: markdown
    comment: true
    fail-on: none
```

`permissions: { security-events: write }` is required for the SARIF upload, `pull-requests: write` for comments.

### Manual

```bash
git diff origin/main...HEAD | diffmind --stdin --format sarif --output diffmind.sarif --fail-on high
```

### Exit codes

| Code | Meaning                                          |
| ---- | ------------------------------------------------ |
| `0`  | Clean, or nothing at or above `--fail-on`        |
| `1`  | Findings at or above `--fail-on`                 |
| `2`  | diffmind itself failed (bad flag, missing model) |

`1` and `2` used to be indistinguishable, so a crashed binary looked exactly like a failed review.

### Git hooks

```bash
diffmind install-hooks --hook pre-push --min-severity high
diffmind install-hooks --hook pre-commit
```

The generated hook exits 0 when diffmind isn't installed, so it never blocks a teammate who hasn't set it up. It refuses to overwrite a hook it didn't write unless you pass `--force`.

Or via [pre-commit](https://pre-commit.com):

```yaml
repos:
  - repo: https://github.com/thinkgrid-labs/diffmind
    rev: v0.9.0
    hooks:
      - id: diffmind
```

---

## The cockpit — `diffmind --tui`

```bash
diffmind --tui
```

Analysis starts on launch. Each finding shows the **actual hunk the model
reviewed** and the **context it was given** — a finding you cannot check is one
you will eventually stop reading.

| Key | Action |
| --------- | ------------------------------------------------------------ |
| `j` / `k` | Move through findings |
| `a`       | Accept — records the verdict and copies a review comment |
| `d`       | Dismiss — read, not worth raising |
| `w`       | Wrong — the finding was incorrect |
| `PgUp` / `PgDn` | Scroll the detail pane |
| `r`       | Re-run |
| `q`       | Quit |

Verdicts are written through immediately, so closing the terminal mid-triage
loses nothing. They feed `diffmind stats`.

Accept copies via **OSC 52**, the terminal's own clipboard escape — no
dependency, and it works over SSH. tmux and screen need clipboard passthrough
enabled; if the copy fails you are told, so you never believe you have copied
something you have not.

Because the output is private to you, the tool can afford to be wrong
occasionally: a bad finding costs one keystroke, not an author's afternoon.

---

## Run history

Every review is filed to `.diffmind/runs/<sha>/` — the findings as `run.json`
and `review.md`, plus what the run cost. `diffmind stats` reads them back:

```
  Runs             34
  Median findings  3
  Median time      6.2s
  Median tokens    18420
  Cache hits       61%

  Verdicts         71 accepted · 44 dismissed · 12 wrong
  Accept : wrong   5.9:1  (on target)

  Most often wrong
      7  DM900.maintainability
      3  rulebook.house-style
```

The **accept-to-wrong ratio** is the number that decides whether the tool is
earning its keep — noise is this category's known failure mode, and a reviewer
who cannot measure it will just quietly stop running the reviewer. Verdicts come
from the TUI. Dismissals are excluded from the ratio: choosing not to raise a
correct observation is not the tool being wrong.

Run snapshots are overwritten when a sha is reviewed again; verdicts are
append-only and survive `diffmind stats --clear`, because the ratio is only
meaningful over months.

diffmind writes `.diffmind/.gitignore` covering `runs/`, `cache/`, `models/`,
`symbols.json` and `daemon.json` — your review notes stay private, while
`rules/`, `rules.toml`, `config.toml` and `baseline.json` remain committable.
Your repository's own `.gitignore` is never touched.

---

## Daemon mode

Every invocation otherwise pays the model-load cost — seconds, every time.

```bash
diffmind serve                  # loads the model, unloads after 10 idle minutes
diffmind serve --status
diffmind serve --stop
```

Reviews automatically use a running daemon whose model and device match; `--no-daemon` opts out. It listens on `127.0.0.1` only and requires a per-instance token stored in a `0600` file, so no other user on the machine can submit work to it.

---

## Configuration

`.diffmind/config.toml` — precedence is CLI flag > config file > default.

```toml
[review]
branch        = "develop"
model         = "3b"
min_severity  = "low"      # what gets reported
fail_on       = "high"     # what fails the build
min_confidence = 0.0       # detectors score 0.9+; unscored model findings 0.5
triage        = "auto"     # two-pass triage on large diffs
cache         = true
temperature   = 0.0        # 0 = greedy and reproducible
max_tokens    = 1024
# Extra paths to drop, on top of the built-in noise rules.
ignore        = ["**/legacy/**", "*.generated.ts"]

[backend]
kind        = "local"      # or "ollama" / "openai-compatible"
url         = "http://localhost:11434"
model       = "qwen2.5-coder:14b"
api_key_env = "DIFFMIND_API_KEY"
```

Unknown keys are reported rather than silently ignored.

### Review standards — `.diffmind/rules/*.md`

Rules that need judgement rather than a pattern. Written as prose, committed to
the repo, and read by the model on every review — so a team's review culture
becomes a reviewed artifact instead of tacit knowledge.

```bash
diffmind rules init     # scaffold .diffmind/rules/default.md
diffmind rules list     # what would load, and what each governs
```

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

| Key | Description |
| ---------- | ----------------------------------------------------------------- |
| `scope`    | Globs this rule set governs. Omit to cover the whole repository.  |
| `severity` | **Ceiling** for findings attributed to it — never a promotion.     |
| `id`       | Name used to attribute and suppress. Defaults to the file stem.    |

A finding the model attributes to a rule set gets the rule ID
`rulebook.<id>`, so it suppresses like any other:
`// diffmind-ignore-next-line rulebook.api-conventions`. An attribution naming a
rule set that does not govern that file is discarded — a 1.5B will invent a
plausible name, and an invented one could never be suppressed.

Rule bodies go in the *stable* half of the prompt and units are grouped by which
rule sets govern them, so every unit in a group sends a byte-identical prefix.
That is what keeps prompt-prefix caching possible; scoping rules per file would
otherwise make every prompt unique.

A rule set that fails to parse is reported and skipped, never silently ignored.

### Pattern rules — `.diffmind/rules.toml`

Regex rules run before the model: instant, deterministic, zero inference cost.

```toml
[[rule]]
id       = "no-console"
pattern  = "console\\.log"
message  = "Remove debug logging before merging"
fix      = "Use the structured logger"
severity = "medium"
category = "quality"
files    = ["*.ts", "*.tsx"]

[[rule]]
id       = "no-hardcoded-password"
pattern  = "password\\s*=\\s*[\"'][^\"']+[\"']"
message  = "Hardcoded password — use a secrets manager"
severity = "high"
category = "security"
```

| Field      | Required | Description                                                             |
| ---------- | -------- | ----------------------------------------------------------------------- |
| `pattern`  | ✓        | Regex matched against added lines                                       |
| `message`  | ✓        | Finding description                                                     |
| `id`       |          | Stable ID for suppression and SARIF. Defaults to a slug of the message. |
| `fix`      |          | Remediation hint                                                        |
| `severity` |          | `high`, `medium`, `low` (default `medium`)                              |
| `category` |          | `security`, `quality`, `performance`, `maintainability`                 |
| `files`    |          | Globs: `*.ts`, `**/generated/**`, or an exact path                      |

---

## Other commands

```bash
diffmind describe                 # PR title, summary, and test plan
diffmind commit                   # conventional commit message for staged changes
diffmind commit --apply
diffmind rules init               # scaffold .diffmind/rules/default.md
diffmind rules list               # show which rule sets load
diffmind index                    # build the symbol index
diffmind stats                    # cost and signal over recorded runs
diffmind stats --clear            # drop run snapshots (verdicts are kept)
diffmind cache show               # cache location and size
diffmind cache clear
```

---

## All Options

```
Usage: diffmind [OPTIONS] [FILES]... [COMMAND]

Commands:
  download       Download or refresh the local AI model files
  index          Build a symbol index for context-aware reviews
  describe       Generate a PR title and description
  commit         Suggest a conventional commit message
  baseline       Record current findings as accepted
  rules          Manage the prose rule sets in .diffmind/rules/
  stats          Findings, cost and accept/wrong ratio over recorded runs
  install-hooks  Install git hooks
  serve          Keep the model resident between runs
  cache          Inspect or clear the review cache

Options:
  -b, --branch <BRANCH>          Base branch [default: the repo's default branch]
  -m, --model <MODEL>            0.5b, 1.5b, 3b, 7b, 14b, 32b [default: 1.5b]
  -l, --last                     Review the last commit only
      --staged                   Review staged changes only
      --stdin                    Read the diff from stdin
      --range <RANGE>            Review an explicit revision range, e.g. v1.2.0..HEAD
  -t, --tui                      Launch the interactive TUI
      --ticket <FILE_OR_TEXT>    Acceptance criteria to check against
      --min-severity <LEVEL>     Minimum severity to report [default: low]
      --min-confidence <N>       Minimum confidence to report, 0.0-1.0
      --fail-on <LEVEL>          Severity causing exit 1 [default: --min-severity]
  -f, --format <FORMAT>          text, json, sarif, markdown [default: text]
  -o, --output <FILE>            Write the report to a file
      --max-tokens <N>           Output tokens per chunk [default: 1024]
      --triage <MODE>            auto, on, off [default: auto]
      --temperature <N>          0 is greedy and reproducible [default: 0]
      --seed <N>                 Sampling seed
      --no-cache                 Skip the result cache
      --no-baseline              Ignore .diffmind/baseline.json
      --no-daemon                Don't use a running daemon
      --device <DEVICE>          auto, cpu, metal [default: auto]
      --backend <KIND>           local, ollama, openai-compatible
      --backend-url <URL>        Remote backend base URL
      --backend-model <NAME>     Model name on the remote backend
      --debug                    Print raw model output to stderr
```

---

## How It Works

1. **Parse** — the diff is parsed once into typed per-file hunks with real pre/post-image line numbers.
2. **Pre-filter** — lockfiles, `linguist-generated` paths, files carrying a `@generated` banner, minified bundles, assets, snapshots, your `ignore` globs, and hunks that only change whitespace are dropped. Costs nothing, typically removes most of a real branch, and the counts are reported rather than silently applied. Whitespace inside a string literal counts as content, and indentation is never dismissed in Python or YAML.
3. **Deterministic detectors** — commented-out code, removed-but-used declarations, and your regex rules. No model involved.
4. **Context** — the enclosing function of each hunk (plus definitions of referenced symbols) is pulled from `.diffmind/symbols.json`, assembled per chunk so one file's edit does not invalidate another's cached result.
5. **Triage** — on large diffs, a cheap first pass decides which files carry real risk.
6. **Review units** — hunks are grouped into regions of a file rather than cut wherever a line budget ran out, so related hunks are read together and an edit in one function only re-reviews that function. Units are sized to the backend's *actual* context window, read from the GGUF metadata.
7. **Constrained decoding** — the sampler consults a JSON state machine before committing each token, so the model cannot emit a preamble, an unbalanced brace, or a truncated string. Output that hits the token cap is repaired rather than discarded.
8. **Anchoring** — findings pointing at a file not in the diff are dropped; off-by-N line numbers snap to the nearest changed line.
9. **Suppression** — inline directives, the baseline, and `--min-confidence` are applied, then results are deduplicated and sorted.

---

## Project Structure

```
diffmind/
├── action.yml                  # GitHub Action
├── .pre-commit-hooks.yaml      # pre-commit framework integration
├── install.sh                  # one-line installer (checksum-verified)
├── packages/core-engine/src/
│   ├── analyzer.rs             # orchestration: chunking, triage, cache, anchoring
│   ├── backend/                # candle (local GGUF) and remote (Ollama / OpenAI)
│   ├── detectors.rs            # deterministic rules
│   ├── diff.rs                 # unified-diff parser, chunking, finding anchoring
│   ├── json_guard.rs           # constrained-decoding state machine
│   ├── prefilter.rs            # deterministic noise removal, with counts
│   ├── prompt.rs               # prompt construction
│   ├── rulebook.rs             # prose rule sets (.diffmind/rules/*.md)
│   ├── unit.rs                 # hunks → review units (the cached, reviewed thing)
│   ├── sarif.rs                # SARIF 2.1.0 output
│   └── suppression.rs          # inline directives and baselines
└── apps/tui-cli/src/
    ├── main.rs                 # entry point and review pipeline
    ├── daemon.rs               # diffmind serve
    ├── settings.rs             # CLI + config resolution
    ├── output.rs               # text / JSON / SARIF / Markdown rendering
    ├── indexer.rs              # symbol indexer
    └── tui.rs                  # interactive browser
```

---

## Roadmap

- [ ] Auto-fix patches (`diffmind fix`) — gated on remote backends landing, since a 1.5B's patches are not trustworthy enough to apply
- [ ] Cross-file impact analysis — find callers of deleted or renamed functions
- [ ] VS Code / JetBrains extensions, talking to the daemon
- [ ] Homebrew tap and scoop manifest
- [ ] Fine-tuned review model

---

## Contributing

Issues and pull requests welcome at [github.com/thinkgrid-labs/diffmind](https://github.com/thinkgrid-labs/diffmind).

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

---

## License

MIT — see [LICENSE](LICENSE).

---

> **Diffmind** — AI-powered local code review. Private by design. Free forever.

Built with ❤️ by Tech Lead, for Tech Leads.
