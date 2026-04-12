# diffmind

**Local-first AI code review agent — on-device inference, no cloud required.**

Diffmind runs a quantized [Qwen2.5-Coder](https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF) model directly on your machine. Your code never leaves your environment. No API keys. No subscriptions. No network round-trips.

Ships as a **single self-contained Rust binary** with an optional interactive [ratatui](https://ratatui.rs) TUI.

---

## Why diffmind?

|             | diffmind                           | Cloud AI review                  |
| ----------- | ---------------------------------- | -------------------------------- |
| **Privacy** | Code stays on your machine         | Code sent to third-party servers |
| **Latency** | No network — instant diff intake   | API round-trips add seconds      |
| **Cost**    | Free after one-time model download | Per-token billing                |
| **Offline** | Works with no internet after setup | Requires connectivity            |
| **CI**      | Single binary, no runtime deps     | Needs API key management         |

---

## Features

- **Security analysis** — hardcoded secrets, injection vectors, insecure data flow
- **Quality review** — logical bugs, anti-patterns, API misuse
- **Performance hints** — inefficient loops, memory overhead, unnecessary allocations
- **Maintainability** — naming, readability, architectural complexity
- **Local RAG** — indexes your project's symbols so the model understands function and type definitions referenced in the diff
- **Interactive TUI** — ratatui-powered terminal UI with navigable findings and detail panel (`--tui`)
- **CI-friendly** — pipe any diff via stdin, filter by severity, exit-code ready

---

## Installation

### Prebuilt binary (recommended)

Download the latest binary for your platform from [GitHub Releases](https://github.com/thinkgrid-labs/diffmind/releases):

| Platform            | Asset                                       |
| ------------------- | ------------------------------------------- |
| Linux x86_64        | `diffmind-x86_64-unknown-linux-gnu.tar.gz`  |
| Linux ARM64         | `diffmind-aarch64-unknown-linux-gnu.tar.gz` |
| macOS x86_64        | `diffmind-x86_64-apple-darwin.tar.gz`       |
| macOS Apple Silicon | `diffmind-aarch64-apple-darwin.tar.gz`      |
| Windows x86_64      | `diffmind-x86_64-pc-windows-msvc.zip`       |

```bash
# Linux / macOS example
tar -xzf diffmind-x86_64-unknown-linux-gnu.tar.gz
sudo mv diffmind /usr/local/bin/

# Verify
diffmind --version
```

---

## Quick Start

```bash
# 1. Download the model (one-time setup, ~1.1 GB)
diffmind download

# 2. (Optional) Index your project's symbols for context-aware reviews
#    Run once per project, then re-run when the codebase changes significantly
diffmind index

# 3. Review your current branch against main
diffmind

# 4. Or launch the interactive TUI
diffmind --tui
```

---

## Model Setup

Diffmind downloads GGUF model weights to `~/.diffmind/models/`. All models are **Qwen2.5-Coder** — coding-optimised only, no generic chat models.

```bash
# Interactive picker — shows all models with sizes and hardware requirements
diffmind download

# Skip the picker and download a specific model directly
diffmind download --model 7b

# Force re-download (e.g. after corruption)
diffmind download --model 1.5b --force
```

When no `--model` is given, diffmind shows an interactive list and checks your RAM and free disk space against each model's requirements before downloading.

```
  #    Model                       Size    Min RAM   Description
  ────────────────────────────────────────────────────────────────────────────────
  [1]   Qwen2.5-Coder-0.5B         0.4 GB    2 GB   Fastest — lint-style checks, CI / low-end hardware
  [2] * Qwen2.5-Coder-1.5B         1.1 GB    4 GB   Recommended — balanced quality and speed
  [3]   Qwen2.5-Coder-3B           2.1 GB    6 GB   Better — deeper reasoning, complex codebases
  [4]   Qwen2.5-Coder-7B           4.7 GB    8 GB   High quality — strong security & logic analysis
  [5]   Qwen2.5-Coder-14B          9.0 GB   16 GB   Expert — deep code understanding, workstation
  [6]   Qwen2.5-Coder-32B         20.0 GB   32 GB   Maximum — near human-level review, server-grade

  * recommended default
```

All models use **Q4_K_M quantisation**. CPU only — no GPU required. Inference via [candle](https://github.com/huggingface/candle).

---

## Usage

### Basic review

```bash
# Diff current branch against main (default)
diffmind

# Diff against a different branch
diffmind --branch develop

# Review specific files or directories only
diffmind src/auth/ src/payments/

# Use the 3B model for deeper analysis
diffmind --model 3b
```

### Interactive TUI

Launch the ratatui terminal UI for navigable, interactive results:

```bash
diffmind --tui
diffmind --tui --branch staging --model 3b
```

**TUI keybindings:**

| Key       | Action           |
| --------- | ---------------- |
| `a`       | Run analysis     |
| `j` / `↓` | Next finding     |
| `k` / `↑` | Previous finding |
| `q`       | Quit             |

### Stdin mode (CI / pipe)

Pipe any `git diff` output directly:

```bash
git diff main...HEAD | diffmind --stdin

# With a specific model
git diff main...HEAD | diffmind --stdin --model 3b

# Filter to high-severity only
git diff main...HEAD | diffmind --stdin --min-severity high
```

### Ticket-aware review (`--ticket`)

Provide the user story or acceptance criteria from your Jira / Linear / GitHub ticket and diffmind will check whether the diff actually implements what was asked — in addition to its standard security and quality review.

```bash
# Pass a ticket file
diffmind --ticket ticket.md

# Or paste inline text directly
diffmind --ticket "As a user I want password reset emails so that I can recover my account.
Acceptance criteria:
- Reset link expires after 1 hour
- Link is single-use
- User receives confirmation email after reset"

# Works with all other flags
diffmind --branch feature/auth --ticket ticket.md --model 3b --format json
```

Missing or incorrectly implemented requirements appear as **`[Req]`** findings with category `compliance` — distinct from the standard security/quality findings.

### Symbol indexing (Local RAG)

Build a local symbol index so the model understands the definitions of functions and types referenced in your diff. Run once per project, then keep it updated:

```bash
# Build or refresh the index
diffmind index

# Index is stored at .diffmind/symbols.json in your project root
```

The indexer supports: TypeScript, JavaScript, Go, Python, Rust.

---

## All Options

```
Usage: diffmind [OPTIONS] [FILES]... [COMMAND]

Commands:
  download  Download or refresh the local AI model files
  index     Build a symbol index of the local repository for context-aware reviews
  help      Print help for a subcommand

Arguments:
  [FILES]...  Specific files or directories to review (optional)

Options:
  -b, --branch <BRANCH>              Base branch to diff against [default: main]
  -m, --model <MODEL>                Model size: 1.5b or 3b [default: 1.5b]
  -t, --tui                          Launch interactive ratatui TUI
      --stdin                        Read diff from stdin instead of running git diff
      --ticket <FILE_OR_TEXT>        User story / acceptance criteria to validate against (file path or inline text)
      --min-severity <MIN_SEVERITY>  Minimum severity to report — also sets the CI exit-code threshold [default: low]
  -f, --format <FORMAT>              Output format: text or json [default: text]
      --max-tokens <MAX_TOKENS>      Max output tokens per diff chunk [default: 1024]
  -h, --help                         Print help
  -V, --version                      Print version
```

---

## CI / CD Integration

diffmind works well in CI pipelines. No API keys or network access needed after the model is cached.

### GitHub Actions example

```yaml
- name: Cache diffmind model
  uses: actions/cache@v4
  with:
    path: ~/.diffmind/models
    key: diffmind-models-1.5b

- name: Install diffmind
  run: |
    curl -sSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/diffmind-x86_64-unknown-linux-gnu.tar.gz \
      | tar -xz -C /usr/local/bin

- name: Download model (if not cached)
  run: diffmind download

- name: Review PR diff
  run: git diff origin/main...HEAD | diffmind --stdin --min-severity high
```

### Pre-commit hook

```bash
#!/bin/sh
# .git/hooks/pre-push
git diff origin/main...HEAD | diffmind --stdin --min-severity high
```

---

## Project Structure

```
diffmind/
├── Cargo.toml                  # Workspace root
├── packages/
│   └── core-engine/            # Rust inference library (candle + GGUF)
│       └── src/lib.rs          # ReviewAnalyzer, chunking, JSON parsing
└── apps/
    └── tui-cli/                # diffmind binary
        └── src/
            ├── main.rs         # Entry point, TUI + static dispatch
            ├── cli.rs          # Clap argument definitions
            ├── download.rs     # Model download with progress bar
            ├── git.rs          # git diff integration
            ├── indexer.rs      # Symbol indexer (Local RAG)
            └── rag.rs          # RAG context builder
```

---

## How It Works

1. **Diff capture** — runs `git diff <branch>...HEAD` (or reads stdin) and splits output per file
2. **Symbol context** — if an index exists, relevant function/type definitions are prepended as context
3. **Chunked inference** — each file diff is independently passed to the local GGUF model via [candle](https://github.com/huggingface/candle); the model generates a JSON array of findings
4. **Early exit** — generation stops as soon as the JSON array is syntactically complete (no wasted tokens)
5. **Output** — findings are printed to stdout (static mode) or rendered in the ratatui TUI

---

## Roadmap

- [ ] `--output <file>` to write Markdown or JSON report to disk
- [ ] Incremental model updates (version-check against HuggingFace before re-download)
- [ ] Custom rule file (`.diffmind/rules.toml`) for team-specific review baselines

---

## License

MIT — see [LICENSE](LICENSE).

---

✨ Support the Local-First Movement
If you believe code reviews should be private and fast, consider contributing to the diffmind core.

Built with ❤️ by Tech Lead, for Tech Leads.
