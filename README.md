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
| **Latency**    | No network — instant diff intake   | API round-trips add 5–30 seconds            |
| **Offline**    | Works with no internet after setup | Requires connectivity                       |
| **CI/CD**      | Single binary, no runtime deps     | Needs API key management and secrets        |
| **Compliance** | No data residency concerns         | Data may cross jurisdictions                |

---

## Features

- **Security analysis** — hardcoded secrets, injection vectors, disabled auth, insecure data handling
- **Bug detection** — removed variables still in use, commented-out logic, logical errors
- **Quality review** — anti-patterns, dead code, API misuse
- **Performance hints** — inefficient algorithms, memory overhead, unnecessary allocations
- **Maintainability** — naming, readability, complexity
- **Ticket-aware review** — provide a Jira/Linear/GitHub ticket and Diffmind checks if the diff actually implements the requirements (`--ticket`)
- **Local RAG** — indexes your project's symbols so the model understands function and type definitions referenced in the diff (`diffmind index`)
- **Interactive TUI** — ratatui terminal UI with navigable findings and detail panel (`--tui`)
- **CI/CD gate** — pipe any `git diff` via stdin, filter by severity, exits with code 1 on findings
- **JSON output** — machine-readable results for dashboards and tooling (`--format json`)

---

## Installation

### Linux & macOS — one command

```bash
curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash
```

Auto-detects your OS and CPU architecture (Intel or Apple Silicon), downloads the right binary, and installs to `/usr/local/bin`. No dependencies required.

Pin a specific version:

```bash
VERSION=v.x.x curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash
```

### Windows

Download `diffmind-x86_64-pc-windows-msvc.zip` from [GitHub Releases](https://github.com/thinkgrid-labs/diffmind/releases), extract it, and place `diffmind.exe` anywhere on your `PATH`.

### Build from source (Rust)

```bash
git clone https://github.com/thinkgrid-labs/diffmind
cd diffmind
cargo install --path apps/tui-cli
```

### Pre-built binaries

| Platform            | Asset                                       |
| ------------------- | ------------------------------------------- |
| macOS Apple Silicon | `diffmind-aarch64-apple-darwin.tar.gz`      |
| macOS Intel         | `diffmind-x86_64-apple-darwin.tar.gz`       |
| Linux x86_64        | `diffmind-x86_64-unknown-linux-gnu.tar.gz`  |
| Linux ARM64         | `diffmind-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64      | `diffmind-x86_64-pc-windows-msvc.zip`       |

```bash
tar -xzf diffmind-<target>.tar.gz
sudo mv diffmind /usr/local/bin/
diffmind --version
```

---

## Quick Start

```bash
# 1. Download the AI model (one-time setup, ~1.1 GB)
diffmind download

# 2. (Optional) Index your project for context-aware reviews
diffmind index

# 3. Review your current branch against main
diffmind --branch main

# 4. Or review only your last commit
diffmind --last

# 5. Launch the interactive TUI
diffmind --tui
```

---

## AI Model Setup

diffmind downloads GGUF model weights to `~/.diffmind/models/`. All models are **Qwen2.5-Coder** — coding-optimised only, no generic chat models. Inference runs fully on CPU via [candle](https://github.com/huggingface/candle) — no GPU required.

```bash
# Interactive picker with hardware requirements check
diffmind download

# Download a specific model directly
diffmind download --model 3b

# Force re-download after corruption
diffmind download --model 1.5b --force
```

Available models (**Q4_K_M quantisation**):

```
  #    Model                       Size    Min RAM   Description
  ────────────────────────────────────────────────────────────────────────────────
  [1]   Qwen2.5-Coder-0.5B         0.4 GB    2 GB   Fastest — lint-style, CI / low-end hardware
  [2] * Qwen2.5-Coder-1.5B         1.1 GB    4 GB   Recommended — balanced quality and speed
  [3]   Qwen2.5-Coder-3B           2.1 GB    6 GB   Better — deeper reasoning, complex codebases
  [4]   Qwen2.5-Coder-7B           4.7 GB    8 GB   High quality — security & logic analysis
  [5]   Qwen2.5-Coder-14B          9.0 GB   16 GB   Expert — deep code understanding
  [6]   Qwen2.5-Coder-32B         20.0 GB   32 GB   Maximum — near human-level review quality

  * recommended default
```

---

## Usage

### Basic code review

```bash
# Review current branch vs main
diffmind --branch main

# Review only your last commit (fastest)
diffmind --last

# Review specific files or directories
diffmind src/auth/ src/payments/

# Use a larger model for deeper analysis
diffmind --model 3b --branch main

# Debug: see raw model output
diffmind --model 3b --branch main --debug
```

### Ticket-aware review

Provide the user story or acceptance criteria from your Jira / Linear / GitHub ticket. diffmind checks that the diff actually implements what was asked — missing or incomplete requirements are flagged as `compliance` findings.

```bash
# Pass a ticket file
diffmind --ticket ticket.md --branch main

# Or paste acceptance criteria inline
diffmind --ticket "User can reset password via email link.
Acceptance criteria:
- Reset link expires after 1 hour
- Link is single-use
- Confirmation email sent after reset"

# Combine with other options
diffmind --branch feature/auth --ticket ticket.md --model 3b --format json
```

### Interactive TUI

Navigate findings in a full-screen terminal UI:

```bash
diffmind --tui
diffmind --tui --branch staging --model 3b
```

| Key       | Action           |
| --------- | ---------------- |
| `a`       | Run analysis     |
| `j` / `↓` | Next finding     |
| `k` / `↑` | Previous finding |
| `q`       | Quit             |

### Stdin / pipe mode

Pipe any `git diff` output for flexible integration:

```bash
git diff main...HEAD | diffmind --stdin

# High-severity only
git diff main...HEAD | diffmind --stdin --min-severity high

# JSON output for tooling
git diff main...HEAD | diffmind --stdin --format json | jq '.[] | select(.severity == "high")'
```

### Local symbol indexing (RAG)

Build a symbol index so the model understands definitions of functions and types referenced in your diff:

```bash
# Build or refresh the symbol index
diffmind index

# Stored at .diffmind/symbols.json in your project root
```

Supported languages: TypeScript, JavaScript, Go, Python, Rust.

---

## All Options

```
Usage: diffmind [OPTIONS] [FILES]... [COMMAND]

Commands:
  download  Download or refresh the local AI model files
  index     Build a symbol index for context-aware reviews

Arguments:
  [FILES]...  Specific files or directories to review (optional)

Options:
  -b, --branch <BRANCH>              Base branch to diff against [default: main]
  -m, --model <MODEL>                Model size: 0.5b, 1.5b, 3b, 7b, 14b, 32b [default: 1.5b]
  -l, --last                         Review the last commit only (HEAD~1..HEAD)
  -t, --tui                          Launch the interactive ratatui TUI
      --stdin                        Read diff from stdin
      --ticket <FILE_OR_TEXT>        User story / acceptance criteria (file path or inline text)
      --min-severity <LEVEL>         Minimum severity to report: low, medium, high [default: low]
  -f, --format <FORMAT>              Output format: text or json [default: text]
      --max-tokens <N>               Max output tokens per diff chunk [default: 1024]
      --debug                        Print raw model output and token counts to stderr
  -h, --help                         Print help
  -V, --version                      Print version
```

---

## CI/CD Integration

diffmind is designed for CI pipelines. No API keys needed. Cache the model between runs.

### GitHub Actions

```yaml
- name: Cache diffmind model
  uses: actions/cache@v4
  with:
    path: ~/.diffmind/models
    key: diffmind-models-1.5b

- name: Install diffmind
  run: |
    curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash

- name: Download model (if not cached)
  run: diffmind download --model 1.5b

- name: AI code review
  run: git diff origin/main...HEAD | diffmind --stdin --min-severity high
```

### Git pre-push hook

```bash
#!/bin/sh
# .git/hooks/pre-push
git diff origin/main...HEAD | diffmind --stdin --min-severity high
```

---

## How It Works

1. **Diff capture** — runs `git diff <branch>...HEAD` (or reads stdin) and splits per-file
2. **Deterministic rules** — fast, regex-based checks run before the model: commented-out code blocks, removed variable declarations still in use, and other high-confidence patterns
3. **Symbol context (RAG)** — if `.diffmind/symbols.json` exists, relevant function/type definitions are prepended as context
4. **Chunked inference** — each file diff is independently passed to the local GGUF model; the model generates a JSON array of findings
5. **Early exit** — token generation stops as soon as the JSON array is syntactically complete
6. **Output** — coloured findings printed to stdout (text) or emitted as a JSON array (`--format json`)

---

## Project Structure

```
diffmind/
├── Cargo.toml                  # Workspace root
├── install.sh                  # One-line installer for Linux / macOS
├── packages/
│   └── core-engine/            # Rust inference library (candle + GGUF + deterministic rules)
│       └── src/lib.rs
└── apps/
    └── tui-cli/                # diffmind binary
        └── src/
            ├── main.rs         # Entry point, TUI + static runner
            ├── cli.rs          # Clap argument definitions
            ├── download.rs     # Model download, interactive picker, hardware check
            ├── git.rs          # git diff integration
            ├── indexer.rs      # Symbol indexer (Local RAG)
            └── rag.rs          # RAG context builder
```

---

## Roadmap

The items below are planned or under consideration. Contributions welcome — open an issue to discuss before starting anything large.

### Near-term

- [ ] `--output <file>` — write Markdown or HTML report to disk
- [ ] Incremental model updates — version-check HuggingFace before re-download
- [ ] `diffmind install-hooks` — one command to install a `pre-push` git hook that blocks on High severity findings
- [ ] **Watch mode** (`diffmind watch`) — re-review staged files automatically on each `git add`, no manual invocation needed
- [ ] **Custom rule file** (`.diffmind/rules.toml`) — team-defined regex patterns that run before the model, zero inference cost

### Medium-term

- [ ] **Daemon / server mode** (`diffmind serve`) — keep the model loaded in memory between invocations so subsequent reviews are near-instant. Uses an idle timeout (configurable, default 10 min) — the model is automatically unloaded when not in use so it does not consume RAM while you are away from your desk. Same pattern as `rust-analyzer` or `ssh-agent`.
- [x] **Streaming output** — print findings as each chunk completes rather than waiting for the full diff to finish
- [ ] **SARIF output** (`--format sarif`) — upload to GitHub Code Scanning and get inline PR annotations in the GitHub UI, no extra tooling required
- [ ] **Auto-fix patches** (`diffmind fix`) — convert `suggested_fix` fields into `.patch` files and apply them interactively with `git apply`
- [ ] **PR description generator** (`diffmind describe`) — generate a structured PR title + body from the diff using the same local model
- [ ] **Commit message suggester** (`diffmind commit`) — review staged changes and suggest a conventional commit message

### Concepts & Future Ideas

- [ ] **Hotspot awareness** — inject `git log` change frequency per file into the prompt so the model flags instability patterns in high-churn areas
- [ ] **Cross-file impact analysis** — extend the RAG symbol index to detect callers of deleted or renamed functions across the entire project
- [ ] **Review history & trends** (`diffmind stats`) — store findings in `.diffmind/history/` and surface patterns over time ("60% of High findings are in `auth/`")
- [ ] **VS Code / JetBrains extension** — call the daemon, display findings in the Problems panel with squiggles on diff lines
- [ ] **Fine-tuned review model** — a smaller model trained specifically on code review tasks, trading general capability for faster, more accurate review output

---

## Contributing

Issues, bug reports, and pull requests are welcome at [github.com/thinkgrid-labs/diffmind](https://github.com/thinkgrid-labs/diffmind).

---

## License

MIT — see [LICENSE](LICENSE).

---

> **Diffmind** — AI-powered local code review. Security analysis, bug detection, and code quality feedback in your terminal. Private by design. Free forever.

✨ Support the Local-First Movement
If you believe code reviews should be private and fast, consider contributing to the diffmind core.

Built with ❤️ by Tech Lead, for Tech Leads.
