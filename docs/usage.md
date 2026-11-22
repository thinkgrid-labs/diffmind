# Using Diffmind

- [Choosing what to review](#choosing-what-to-review)
- [Interactive review](#interactive-review)
- [Suppressions and baselines](#suppressions-and-baselines)
- [Measuring the noise: `diffmind stats`](#measuring-the-noise-diffmind-stats)
- [Team rules](#team-rules)
- [Configuration](#configuration)
- [Command-line options](#command-line-options)
- [Daemon mode](#daemon-mode)
- [Other commands](#other-commands)

## Choosing what to review

```bash
diffmind                         # this branch against the repo's default branch
diffmind --last                  # just the last commit
diffmind --staged                # just what's staged
diffmind v1.2.0..HEAD            # an explicit revision range
diffmind src/auth/               # just these paths
diffmind v1.2.0..HEAD src/api/   # a range, narrowed to paths
git diff main...HEAD | diffmind --stdin
```

The base branch comes from `origin/HEAD`. If that is not set, Diffmind uses
whichever of `main`, `master`, `develop` or `trunk` exists. A bare `a..b`
argument is treated as a revision range without `--range`. The check is
strict, so `diffmind ../lib` and a file actually named `a..b` are still treated
as paths.

The diff is filtered before review (lockfiles, generated files, minified
bundles, assets, snapshots, documentation, coding-agent and editor config,
ignore files, formatter settings, your `ignore` globs and whitespace-only
hunks), and the counts are printed. `node_modules`, `vendor`, `target`,
`dist`, `build`, `.next` and `.cache` are left out of the diff entirely. Tests are not filtered. Documentation can be
included with `--include-docs`. After filtering, a diff larger than 1.5 MB is
refused. Review specific paths instead.

## Interactive review

`diffmind --tui` runs the same review in a terminal UI. Each finding shows the
hunk the model read and the context it was given, so you can check it.

The layout looks like this (illustrative, not captured output):

```text
┌Findings (9)────────────────┐┌Detail──────────────────────────────────────────┐
│> HIGH src/auth/token.rs:47 ││ Severity   HIGH                                │
│  HIGH …pi/handlers.rs:214  ││ Category   Security                            │
│✓ MED  src/db/pool.rs:112   ││ Location   src/auth/token.rs:47                │
│✗ MED  src/api/users.rs:88  ││ Rule       DM900.security                      │
│· LOW  src/util/fmt.rs:12   ││                                                │
│                            ││ Issue / Suggested fix                          │
│                            ││ The diff it reviewed                           │
│                            ││ Context it was given                           │
│                            ││ Suppress with: // diffmind-ignore-next-line …  │
└────────────────────────────┘└────────────────────────────────────────────────┘
 [j/k] Move · [a] Accept+copy · [d] Dismiss · [w] Wrong · [r] Re-run · [q] Quit
```

| Key | Action |
| --- | --- |
| `j` / `k` | Move through findings |
| `a` | Accept: records the verdict and copies a review comment |
| `d` | Dismiss: correct, but not worth raising |
| `w` | Wrong: the finding is incorrect |
| `PgUp` / `PgDn` | Scroll the detail pane |
| `r` | Re-run |
| `q` | Quit |

Verdicts are saved to `.diffmind/` as soon as you press the key. Accept copies
the comment with OSC 52, the terminal's own clipboard sequence, so it works over
SSH. tmux and screen need clipboard passthrough enabled. The TUI reports a
copy it could not send, but it cannot detect a terminal that silently ignores
OSC 52.

## Suppressions and baselines

Every finding has a stable rule ID, shown in the output:

| ID | Source |
| --- | --- |
| `DM001` | Commented-out code (fixed rule, no model) |
| `DM002` | A declaration removed while something still uses it (fixed rule, no model) |
| `custom.<id>` | Your regex rules in `.diffmind/rules.toml` |
| `rulebook.<id>` | A model finding credited to one of your written rule sets |
| `DM900.<category>` | Other model findings (`security`, `quality`, `performance`, `maintainability`, and `compliance` with `--ticket`) |

### Inline

```js
// diffmind-ignore-next-line DM001
// const legacy = oldPath();

const x = 1; // diffmind-ignore

/* diffmind-ignore-file DM900.maintainability */
```

With no rule IDs, everything at that spot is silenced. `DM900` silences every
model category at once.

### Baseline

```bash
diffmind baseline create     # record today's findings as accepted
diffmind baseline show
diffmind baseline clear
```

Commit `.diffmind/baseline.json` and later runs report only new findings. The
baseline matches on the content of a finding, not its line number, so edits
above it do not break the match.

## Measuring the noise: `diffmind stats`

Every review is saved under `.diffmind/runs/`. `diffmind stats` summarises them,
including the verdicts you gave in the TUI. The output looks like this
(illustrative numbers):

```text
  Runs             34
  Median findings  3
  Median time      6.2s
  Cache hits       61%

  Verdicts         71 accepted · 44 dismissed · 12 wrong
  Accept : wrong   5.9:1

  Most often wrong
      7  DM900.maintainability
      3  rulebook.house-style
```

The accept-to-wrong ratio tells you whether the reviewer is worth keeping.
Dismissals do not count against it. Saved runs are overwritten when the same
commit is reviewed again. Verdicts are only appended, and survive
`diffmind stats --clear`.

Diffmind writes `.diffmind/.gitignore` to keep `runs/`, `cache/`, `models/`,
`graph.db` and `daemon.json` out of git, while `rules/`, `rules.toml`,
`config.toml` and `baseline.json` can be committed. Your repository's own
`.gitignore` is not touched.

## Team rules

```text
.diffmind/
├── config.toml        # settings: model, base branch, gate thresholds
├── rules.toml         # regex rules: no model, same answer every run
└── rules/*.md         # written rules: judgement, read by the model
```

**If a regex can decide it, put it in `rules.toml`.** It costs nothing, never
changes between runs and is safe to block on. Use a written rule only for
judgement.

| | `rules.toml` | `rules/*.md` |
| --- | --- | --- |
| Read by | regex, before the model runs | the model, in the prompt |
| Cost | none | prompt tokens, every review |
| Same answer every run | yes | depends on the model |
| Good for | `next/router` in `app/`, `@ts-ignore`, hardcoded secrets | "is this abstraction earning its keep" |

A complete example, three scoped rule sets plus 13 regex rules for a Next.js
App Router project, is in
[`examples/nextjs-app-router/`](../examples/nextjs-app-router/).

### Pattern rules: `.diffmind/rules.toml`

Checked against added lines before the model runs.

```toml
[[rule]]
id       = "no-hardcoded-password"
pattern  = "password\\s*=\\s*[\"'][^\"']+[\"']"
message  = "Hardcoded password — use a secrets manager"
fix      = "Use a secrets manager"
severity = "high"        # high | medium | low (default medium)
category = "security"    # security | quality | performance | maintainability
files    = ["*.ts", "**/generated/**"]
```

Only `pattern` and `message` are required. `id` defaults to a slug of the
message.

### Written rules: `.diffmind/rules/*.md`

```markdown
---
description: Conventions for the public API layer
scope: ["src/api/**/*.ts", "!src/api/legacy/**"]
severity: high
---

# API conventions

- Public handlers must return `ApiError`, never a bare string.
- Any new endpoint needs a corresponding entry in `openapi.yaml`.
```

| Field | Meaning |
| --- | --- |
| `id` | Suppression handle, `rulebook.<id>`. Defaults to the filename |
| `description` | One line for humans, shown by `rules list`; never sent to the model |
| `scope` | Globs this rule set governs. Omit for the whole repo. A `!` prefix excludes and beats every include |
| `always` | Apply to every file, ignoring `scope` includes (`!` exclusions still apply) |
| `severity` | A **maximum** for findings from this set, and the drop order when the prompt budget is tight |

`globs:` and `alwaysApply:` are accepted as aliases, so a rule ported from
`.cursor/rules/` loads without edits.

```bash
diffmind rules init     # write a starter .diffmind/rules/default.md
diffmind rules list     # what loads, its severity ceiling and its globs
diffmind rules check    # parse errors, duplicate ids, dead globs, bad regex; exits non-zero
```

**Scope your rule sets.** Every rule set that matches a file goes into that
file's prompt, and the prompt has a byte budget shared with the diff and the
code context; the diff always keeps at least half. When the rule sets do not
all fit, whole sets are dropped, lowest `severity` first, and the run prints
which ones and why. If the model credits a rule set that does not govern the
file, the credit is removed.

## Configuration

`.diffmind/config.toml`. A CLI flag beats the config file, which beats the
default. If the file has an unknown key or does not parse, Diffmind prints a
warning and ignores **the whole file** for that run, so check for the warning
after editing it.

```toml
[review]
branch           = "develop"
model            = "3b"
device           = "auto"     # auto | cpu | metal
min_severity     = "low"      # what gets reported
fail_on          = "high"     # what fails the run
min_confidence   = 0.0        # fixed rules score 0.9+; unscored model findings 0.5
triage           = "auto"     # two-pass triage when 6+ files change
cache            = true
temperature      = 0.0        # 0 = greedy
seed             = 42         # optional; a fixed default is used otherwise
max_tokens       = 1024
format           = "text"
ignore           = ["**/legacy/**", "*.generated.ts"]
include_docs     = false
auto_index       = true       # refresh the code graph before each review
require_complete = true       # exit 2 if any unit went unreviewed [default: on in CI]

[backend]
kind        = "local"         # local | ollama | openai-compatible
```

Backend keys (`url`, `model`, `api_key_env`, `context_tokens`, `timeout_secs`)
are described in [Models and providers](providers.md).

## Command-line options

| Flag | What it does |
| --- | --- |
| `-b, --branch` | Base branch [default: the repo's default branch] |
| `-m, --model` | `0.5b`, `1.5b`, `3b`, `7b`, `14b`, `32b` [default: `1.5b`] |
| `-l, --last` / `--staged` / `--range` / `--stdin` | What to review |
| `-t, --tui` | Interactive review |
| `--min-severity` | Lowest severity reported [default: `low`] |
| `--fail-on` | Severity that causes exit 1 [default: `--min-severity`] |
| `--min-confidence` | Lowest confidence reported, 0.0–1.0 |
| `-f, --format` | `text`, `json`, `sarif`, `markdown` |
| `-o, --output` | Write the report to a file |
| `--ticket <FILE_OR_TEXT>` | Check the diff against acceptance criteria |
| `--backend`, `--backend-url`, `--backend-model`, `--backend-api-key-env`, `--backend-context`, `--backend-timeout` | Use your own model server |
| `--require-complete` / `--allow-incomplete` | Whether an unreviewed unit fails the run [default: required in CI] |
| `--include-docs` | Review `.md`, `.rst`, `.txt` too |
| `--no-cache` / `--no-baseline` / `--no-daemon` / `--no-index` | Opt out for this run |

`--triage`, `--temperature`, `--seed`, `--max-tokens`, `--device` and `--debug`
are listed in `diffmind --help`.

## Daemon mode

Loading the model takes a few seconds per run. A daemon keeps it loaded:

```bash
diffmind serve            # unloads after 10 idle minutes (--idle-timeout)
diffmind serve --status
diffmind serve --stop
```

Reviews use a running daemon automatically when its model and device match;
`--no-daemon` opts out. The daemon listens on `127.0.0.1` only and requires a
256-bit random token stored in a private file (mode `0600` on Unix). The client
checks that the daemon holds the token before sending anything. It serves one
request at a time and drops a connection that has not sent a complete request
(at most 16 MiB) within 30 seconds.

`diffmind serve --status` currently exits 0 whether or not a daemon is running,
so check its output rather than its exit code in scripts.

## Other commands

| Command | Purpose |
| --- | --- |
| `diffmind download` | Download or verify a model (`--model`, `--force`, `--verify`) |
| `diffmind index` | Build or refresh the code graph now (`--rebuild` to start over). Reviews do this automatically unless `--no-index` |
| `diffmind cache show` / `clear` | Inspect or clear the review cache |
| `diffmind describe` | Draft a PR title and description from the diff |
| `diffmind commit` | Suggest a conventional commit message for staged changes (`--apply` to commit) |

`describe` and `commit` exist because the model is already loaded. Review is
the product; they are conveniences.
