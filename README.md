# Diffmind — a code review gate you can actually keep

[![CI](https://github.com/thinkgrid-labs/diffmind/actions/workflows/ci.yml/badge.svg)](https://github.com/thinkgrid-labs/diffmind/actions/workflows/ci.yml)
[![Latest Release](https://img.shields.io/github/v/release/thinkgrid-labs/diffmind)](https://github.com/thinkgrid-labs/diffmind/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

### *Local AI code review, in your terminal. Run it as a CI gate, a git hook, or an interactive review. One Rust binary, no API key.*

**Diffmind reviews a `git diff` and reports security issues, bugs and quality
problems** — on your machine, with no API key and no network. All three ways of
running it share one engine, so what fails your build is the same thing you read
in the terminal.

It does not look at the diff alone. Diffmind builds **its own code graph** of
your repository — tree-sitter, 13 languages, a SQLite file inside your repo — so
a changed function is checked against the code that calls it, not just the
twenty lines around it. Other tools sell this index as a separate product. Here
it is one command, `diffmind index`, and it is free.

The model is a setting, not a fixed part. **A local model is the default and
stays the default** — bundled, offline, free every run. Point diffmind at your
own Ollama or vLLM server if you want a bigger one, and frontier models are
coming as an opt-in, for the diffs worth paying for.

The hard part of an automated reviewer is not finding problems. It is finding
them *the same way twice*, keeping false alarms low enough that nobody deletes
the job, and staying cheap enough to run on every push. That is what this is
built around.

---

## When diffmind is the right tool

Use it when you need review that is:

- **Repeatable** — greedy decoding at a fixed seed, with a constrained JSON
  decoder. The same diff gets the same review every time. A gate that flags a
  line on Tuesday but not on Wednesday gets deleted within a month.
- **Free to run** — no per-token bill, so it can run on every push, on every
  fork's PR, and on a repo with no budget.
- **Offline, no secrets** — nothing leaves the machine. No API key in CI, no bot
  account, no outside company to get approved first.
- **Persistent** — a baseline, inline suppressions, fixed rule IDs and a
  recorded accept/wrong ratio. A team carries review debt for years, so it needs
  somewhere to live.

### About model size

Diffmind is not a model. It is everything around one — the diff parser, the code
graph, the context budget, the suppression system, the run history, the gate.
**The default 1.5B model is not as smart as a frontier model, and does not
pretend to be.** But the model is the easy part to swap. The rest is the part you
did not want to build.

So model size is a setting, not a limit. Run the bundled model on all several
hundred diffs for free. Point at a 14B on your own machine for the ones that
matter. When hosted models arrive, pay only for the diff that is worth it.
What never changes is where findings are stored, how you silence them, and how
the gate behaves.

---

## Two tiers of trust

Findings are not all worth the same, and the output says which is which.

**Deterministic findings** — no model involved. Commented-out code (`DM001`), a
declaration deleted while something still uses it (`DM002`), and your own regex
rules (`custom.<slug>`). These match a pattern or they don't: they cost nothing,
never change between runs, and you can block on them without worrying.

**Model findings** (`DM900.<category>`) — the model's opinion on a hunk, after
being shown the function it sits in, the callers of what changed, the
definitions it refers to, and the file's tests. How good they are depends on the
model: the default 1.5B catches obvious mistakes, like a linter; a 14B behind
Ollama reads more like a teammate. Both are worth reading. Neither is worth
trusting blindly, which is why interactive review records when they are wrong.

The split is on purpose. **Set `--fail-on` for the tier you trust** — many teams
block on high-severity deterministic findings and let model findings show up
without failing the build.

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
diffmind --tui             # same review, stepped through in the terminal
```

```bash
diffmind --last                  # just the last commit
diffmind --staged                # just what's staged
diffmind v1.2.0..HEAD            # an explicit revision range
diffmind src/auth/               # just these paths
diffmind v1.2.0..HEAD src/api/   # a range, narrowed to paths
diffmind --tui                   # interactive review
```

The base branch is read from `origin/HEAD`, falling back to whichever of
`main` / `master` / `develop` / `trunk` exists — it is not assumed to be `main`.
A bare `a..b` argument is recognised as a revision range without `--range`; the
detection is strict, so `diffmind ../lib` and a file genuinely named `a..b` are
still treated as paths.

**There is really only one command.** `diffmind` on its own does the review.
Everything else is either setup you run once (`download`, `index`,
`install-hooks`, `rules init`) or upkeep you run now and then (`baseline`,
`stats`, `cache`, `serve`). You do not have to configure anything before the
first review works.

---

## The code graph

Most diff reviewers only see a hunk and a filename. Diffmind indexes the
repository first, so when it reviews a changed function it also sees the code
that calls it. That is the difference between "this line looks wrong" and "this
signature change breaks three callers".

`diffmind index` scans the repo with tree-sitter and saves definitions and
references into `.diffmind/graph.db`, a normal SQLite file. Each review asks it
four questions:

| Question | What you get |
| --- | --- |
| Which function contains this line? | The whole function, so a hunk is never read alone |
| What does this hunk define? | The symbols that changed |
| **What calls this symbol?** | Who breaks if it changes — a regex index cannot answer this at all |
| Where is this name defined? | The helpers and types the hunk uses |

Three things keep it reliable:

- **It does not store code.** A definition only stores a line range; the source
  is read from your files when needed. So the database can never show a snippet
  that no longer matches the file being reviewed.
- **It only re-reads what changed.** Re-indexing checks file times and re-parses
  just those files. `node_modules`, `target`, `dist`, `.venv` and similar are
  skipped. `diffmind index --rebuild` starts fresh, and if the schema changes it
  rebuilds itself instead of reading a format it does not understand.
- **It stays local.** No server, no hosted index, no account, nothing to log in
  to. It is gitignored by default, and you can rebuild it any time.

Reference patterns stay narrow on purpose — function calls, constructors, type
names. If it recorded every identifier, "what calls this" would return half the
repo, and an answer that includes everything is as useless as no answer.

### Languages

Two things depend on the language, and only one of them limits you.

**The review works on any text diff** — any language, plus config files, SQL
migrations and shell scripts. The model reads the hunk, and the fixed rules and
your regex rules run on added lines no matter the file type.

**Only the code graph needs a parser.** Its four questions are answered by
tree-sitter, and diffmind ships 13 of them:

| Language | Extensions |
| ---------- | -------------------------------- |
| Rust       | `.rs` |
| TypeScript | `.ts` `.mts` `.cts` |
| TSX        | `.tsx` |
| JavaScript | `.js` `.jsx` `.mjs` `.cjs` |
| Python     | `.py` `.pyi` |
| Go         | `.go` |
| Java       | `.java` |
| C#         | `.cs` |
| Ruby       | `.rb` `.rake` |
| PHP        | `.php` |
| C          | `.c` `.h` |
| C++        | `.cpp` `.cc` `.cxx` `.hpp` `.hh` |
| Scala      | `.scala` `.sc` |

Each of these gets definitions, references and callers. Files not in the table
are still reviewed — you get the hunk, the test file if it follows the usual
naming, and your rule sets — but without caller information, so a changed
signature is judged on its own instead of against the code that uses it.

Adding a language is one entry in the `LANGS` table in
[`apps/tui-cli/src/graph/extract.rs`](apps/tui-cli/src/graph/extract.rs). Pull
requests welcome.

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

`1` and `2` are different on purpose, so a crash never looks like a failed
review.

### Git hooks

```bash
diffmind install-hooks --hook pre-push --min-severity high
diffmind install-hooks --hook pre-commit
```

The hook it writes exits 0 if diffmind is not installed, so it never blocks a
teammate who has not set it up, and it will not overwrite a hook it did not
write unless you pass `--force`. Also available through
[pre-commit](https://pre-commit.com) — `repo: https://github.com/thinkgrid-labs/diffmind`, `id: diffmind`.

---

## Interactive review — `diffmind --tui`

The CI gate is public, so it has to be careful about what it reports. This runs
on your own machine, so it can show you everything.

It starts reviewing as soon as it opens. Each finding shows the **exact hunk the
model read** and the **context it was given** — because if you cannot check a
finding, sooner or later you stop reading them.

```text
┌Status────────────────────────────────────────────────────────────────────────┐
│ diffmind  9 findings — a accept · d dismiss · w wrong                        │
└──────────────────────────────────────────────────────────────────────────────┘
┌Findings (9)────────────────┐┌Detail──────────────────────────────────────────┐
│> HIGH src/auth/token.rs:47 ││ Severity   HIGH                                │
│  HIGH …pi/handlers.rs:214  ││ Category   Security                            │
│✓ MED  src/db/pool.rs:112   ││ Location   src/auth/token.rs:47                │
│✗ MED  src/api/users.rs:88  ││ Rule       DM900.security                      │
│· LOW  src/util/fmt.rs:12   ││ Confidence 78%                                 │
│  LOW  src/db/pool.rs:130   ││                                                │
│                            ││ Issue                                          │
│                            ││ validate_token() now returns true for an       │
│                            ││ empty string. login() and refresh() both       │
│                            ││ gate on it.                                    │
│                            ││                                                │
│                            ││ Suggested fix                                  │
│                            ││ Reject empty input before the length check.    │
│                            ││                                                │
│                            ││ The diff it reviewed                           │
│                            ││  -pub fn validate_token(t: &str) -> bool {     │
│                            ││  +pub fn validate_token(t: &str, strict: bool) │
│                            ││  -    !t.is_empty()                            │
│                            ││  +    t.len() >= 0                             │
│                            ││                                                │
│                            ││ Context it was given                           │
│                            ││  callers: login() src/auth/session.rs:22       │
│                            ││           refresh() src/auth/session.rs:40     │
│                            ││                                                │
│                            ││ Suppress with:  // diffmind-ignore-next-line   │
│                            ││                 DM900.security                 │
└────────────────────────────┘└────────────────────────────────────────────────┘
 [j/k] Move · [a] Accept+copy · [d] Dismiss · [w] Wrong · [r] Re-run · [q] Quit
```

The left panel lists the findings, marked with the answer you gave (`✓` accept,
`✗` wrong, `·` dismiss). The right panel shows the proof. This is not a summary
of a review that ran somewhere else — it is the review, and the keys below save
straight to `.diffmind/`.

| Key | Action |
| --------------- | -------------------------------------------------------- |
| `j` / `k`       | Move through findings |
| `a`             | Accept — records the verdict and copies a review comment |
| `d`             | Dismiss — read, not worth raising |
| `w`             | Wrong — the finding was incorrect |
| `PgUp` / `PgDn` | Scroll the detail pane |
| `r`             | Re-run |
| `q`             | Quit |

Your answers are saved the moment you press the key, so closing the terminal
halfway through loses nothing. Accept copies the comment using **OSC 52**, the
terminal's own clipboard feature — no extra tool needed, and it works over SSH.
tmux and screen need clipboard passthrough turned on; if the copy fails you are
told, so you never think you copied something when you did not.

---

## Keeping the gate alive

Noise is what kills tools like this. One false alarm that nobody can turn off
is how a review job ends up deleted. So every finding has a fixed rule ID, shown
in the output, and you can turn off exactly that one thing.

### Inline

```js
// diffmind-ignore-next-line DM001
// const legacy = oldPath();

const x = 1; // diffmind-ignore

/* diffmind-ignore-file DM900.maintainability */
```

If you list no rule IDs, everything at that spot is silenced. `DM900` silences
every model category at once, so you do not have to list them one by one.

### Baseline

Adding a reviewer to an existing codebase should not mean fixing everything
first.

```bash
diffmind baseline create     # record today's findings as accepted
diffmind baseline show
diffmind baseline clear
```

Commit `.diffmind/baseline.json`, and later runs report only new issues. The
baseline matches on the content of the finding, not the line number, so editing
unrelated lines above it does not break it.

### Checking whether it is worth keeping

Every review is saved to `.diffmind/runs/<sha>/`. `diffmind stats` reads them
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

The **accept-to-wrong ratio** is the number that tells you whether to keep using
it. A team that cannot measure the noise will just quietly stop running the
tool. The answers come from interactive review. Dismissals do not count against
it: deciding a correct point is not worth raising is not the tool being wrong.

Saved runs are overwritten when the same commit is reviewed again. Your answers
are only ever added to, and survive `diffmind stats --clear`, because the ratio
only means something over months.

Diffmind writes its own `.diffmind/.gitignore` covering `runs/`, `cache/`,
`models/`, `graph.db` and `daemon.json`, so your review notes stay private, while
`rules/`, `rules.toml`, `config.toml` and `baseline.json` can still be committed.
Your repository's own `.gitignore` is never touched.

---

## Your team's standards

### Written rules — `.diffmind/rules/*.md`

For rules that need judgement instead of a pattern. They live in the repo and
the model reads them on every review, so the things your team keeps repeating in
PR comments become a file anyone can read, instead of knowledge only the senior
people carry.

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

`scope` is a glob for the files the rule set applies to (leave it out for the
whole repo). `severity` is a **maximum** for findings from that rule set — it can
lower a finding's severity but never raise it. `id` defaults to the filename.
Create a starter file with `diffmind rules init`, and see what loads with
`diffmind rules list`.

A finding from a rule set gets the ID `rulebook.<id>` and can be silenced like
any other. If the model credits a rule set that does not apply to that file, the
credit is dropped — a small model will make up a name that sounds right, and a
made-up name could never be silenced.

Rule text goes in the *unchanging* part of the prompt, and reviews are grouped by
which rule sets apply, so every review in a group starts with an identical
prefix. That is what makes prompt-prefix caching possible later. A rule set that
fails to parse is reported and skipped, never ignored quietly.

### Pattern rules — `.diffmind/rules.toml`

Regex, checked against added lines before the model runs: instant, always the
same, and free.

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

**The local model is the default and stays the default.** Everything else is
opt-in. There are three levels, in this order on purpose:

| Level | What runs | Same result every time | Cost per run | Status |
| --- | --- | --- | --- | --- |
| **Built-in local** | Qwen2.5-Coder GGUF inside diffmind, Metal or CPU | ✅ pinned weights, greedy + constrained decoding | free | **default** |
| **Your own server** | Ollama, vLLM, LM Studio, anything OpenAI-compatible | ✗ | free — your own hardware | shipped |
| **Frontier, hosted** | your key, your provider | ✗ | per token | planned, opt-in |

Making a hosted model the default would break the whole idea. No key, no
network and no per-token bill is exactly why this can run on every push, on
every fork's PR, and in a repo with no budget. Hosted models are for the diff
you *choose* to spend money on — never the normal case.

Models download to `~/.diffmind/models/` — all Qwen2.5-Coder, Q4_K_M quantised.
Inference runs on the Apple Silicon GPU via Metal where available, and on CPU
(with Accelerate/BLAS) everywhere else.

Every download is **locked to a fixed commit and checked against a SHA-256 that
ships inside the binary** before the file is put in place. The write is atomic,
so an interrupted download cannot leave a half-written file that looks fine. If
the weights are not byte-for-byte what this build expects, they are rejected
instead of loaded. `diffmind download --verify` re-checks a model you already
have. This is why "the same diff gives the same review" holds on other machines
too: a model name points at exact bytes, not at whatever a branch happens to
contain today.

| Model | Size | Min RAM | Use for |
| -------------------- | ------- | ------- | ----------------------------------- |
| Qwen2.5-Coder-0.5B   | 0.5 GB  | 2 GB    | CI on small runners, lint-grade     |
| Qwen2.5-Coder-1.5B ★ | 1.1 GB  | 4 GB    | Default — balanced                  |
| Qwen2.5-Coder-7B     | 4.7 GB  | 10 GB   | Noticeably better security analysis |
| Qwen2.5-Coder-32B    | 19.9 GB | 40 GB   | Maximum, if you have the machine    |

`3b` and `14b` also exist. `diffmind download` gives an interactive picker with
a hardware check; `--model 1.5b --verify` checks an existing download.

### Your own model server

"Local" does not have to mean "inside diffmind". If you already run Ollama, vLLM
or LM Studio, just point diffmind at it. Your code still never leaves your
machine or your network, and you get a bigger model than diffmind would ever
ship as a 20 GB download.

```bash
diffmind --backend ollama --backend-model qwen2.5-coder:14b

# anything speaking the OpenAI chat API — vLLM, LM Studio, llama.cpp, LiteLLM
diffmind --backend openai-compatible --backend-url http://localhost:8000/v1 --backend-model my-model
```

API keys are read from the environment only (`DIFFMIND_API_KEY`, override with
`--backend-api-key-env`) — never from the config file, which gets committed.

> **What a bigger model costs you.** The constrained JSON decoder plugs directly
> into the built-in sampler, so a remote server cannot use it. You get better
> answers, but you lose the guarantee that the same diff gives the same review.
> Token counts from remote servers are estimates, marked `~`. Same-result-every-time
> only applies to the built-in local model.

### Frontier backends — planned, opt-in

The plan is small and specific: a built-in Anthropic backend, an opt-in
`claude-cli` backend that reuses a subscription you already pay for, and real
token counts from remote servers instead of estimates. Same rules as everything
else here — added on top, off by default, and clear that you give up the
same-result guarantee.

The part that makes it worth paying for is **letting the cheap model decide what
the expensive model reads**. The local model sorts several hundred hunks for
free, then the frontier model looks closely at the few that actually carry risk.
That is very different from sending every diff to an API, and it is the only
version anyone keeps paying for after the first bill.

### Daemon mode

Without this, every run pays the model loading cost again — a few seconds, every
time.

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

1. **Parse** — the diff is turned into per-file hunks with correct before and
   after line numbers.
2. **Filter** — lockfiles, `linguist-generated` paths, `@generated` banners,
   minified bundles, assets, snapshots, your `ignore` globs and whitespace-only
   hunks are dropped. This is free, usually removes most of a real branch, and
   the counts are shown instead of hidden:
   `312 hunks → 74 reviewable (238 filtered: lockfiles, generated, formatting)`.
   Whitespace inside a string still counts as a real change, and indentation is
   never dropped in Python or YAML.
3. **Fixed rules** — `DM001`, `DM002` and your regex rules. No model involved.
4. **Context** — built for each review from `.diffmind/graph.db`: the function
   the hunk is in, the callers of every changed symbol, the definitions it
   refers to, and the matching test file. There is a size limit, so context does
   not grow as the repo grows. Code is read from your files, so a snippet can
   never disagree with the file being reviewed.
5. **Sort by risk** — on large diffs, a quick first pass decides which files
   actually carry risk.
6. **Group** — hunks are grouped by area of the file rather than cut wherever a
   line limit ran out, so related hunks are read together, and editing one
   function only re-reviews that function. If a changed symbol and one of its
   changed callers both appear, they are reviewed together as one. Group size
   matches the model's *real* context window, read from the GGUF file.
7. **Constrained decoding** — before each token, the sampler checks a JSON state
   machine, so the model cannot write an intro sentence, an unbalanced brace or a
   cut-off string. Output that hits the token limit is repaired, not thrown away.
8. **Line matching** — findings that point at a file not in the diff are dropped,
   and line numbers that are slightly off snap to the nearest changed line.
9. **Silencing** — inline comments, the baseline and `--min-confidence` are
   applied, then duplicates are removed and the results sorted.

Step 4 is [the code graph](#the-code-graph). It is the step that separates this
from a reviewer that only ever sees the diff.

---

## Configuration

`.diffmind/config.toml`. A CLI flag beats the config file, and the config file
beats the default. Keys diffmind does not recognise are reported, not ignored.

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

### The options you will actually use

| Flag | What it does |
| ----------------------- | -------------------------------------------------- |
| `-b, --branch`          | Base branch [default: the repo's default branch] |
| `-m, --model`           | `0.5b` … `32b` [default: `1.5b`] |
| `-l, --last` / `--staged` / `--range` / `--stdin` | What to review |
| `-t, --tui`             | Start an interactive review |
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

Two extras come along because the model is already loaded: `diffmind describe`
(a PR title and summary) and `diffmind commit` (a conventional commit message
for staged changes). Useful, but review is the product — nothing else here is
built around them.

---

## Roadmap

- **Frontier models, opt-in** — a built-in Anthropic backend and a `claude-cli`
  backend, with the local model still the default
- **Cheap model picks what the strong model reads**, plus prompt-prefix caching —
  this is what makes a paid model affordable
- **Auto-fix patches** (`diffmind fix`) — waiting on those backends, because a
  1.5B's patches are not safe enough to apply
- **Learn from your answers** — marking a finding wrong should stop that kind of
  finding next time, not just count it
- More languages in the code graph, one `LANGS` entry at a time
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
