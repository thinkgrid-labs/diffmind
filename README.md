# 🧠 Diffmind — Local-First AI Code Review Agent

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Build Status](https://img.shields.io/badge/build-passing-brightgreen.svg)]()
[![Privacy: Offline](https://img.shields.io/badge/Privacy-100%25_Offline-blueviolet.svg)]()

**Diffmind** is a high-performance, local-first AI security and code review agent designed specifically for modern TypeScript engineering teams. It executes entirely on-device using a shared WebAssembly core, providing expert-level code reviews with **zero latency** and **total data privacy**.

---

## ⚡ Why Diffmind?

Standard AI review tools often require sending your proprietary source code to third-party APIs. **Diffmind changed the game.**

- 🔒 **Absolute Privacy**: Your code never leaves your machine. Analysis happens locally via Wasm-powered inference.
- 🚀 **Zero Latency**: No network round-trips. Get instant feedback on your git diffs as you work.
- 🛠️ **Expert-Level Insight**: Powered by **Qwen2.5-Coder-3B**, an LLM specifically optimized for deep reasoning in TypeScript, NestJS, and React Native environments.
- 📉 **Reduced Costs**: Eliminate expensive per-seat developer tool subscriptions.

---

## 🔍 Comprehensive Review Coverage

Diffmind acts as a Senior Software Engineer for your feature branches, analyzing four critical dimensions:

- **🛡️ Security**: Detects hardcoded secrets, injection risks, and insecure data flow.
- **💎 Quality**: Identifies logical bugs, anti-patterns, and API misuse in NestJS and React Native.
- **⚡ Performance**: Highlights inefficient loops, memory overhead, and hydration bottlenecks.
- **📖 Maintainability**: Suggests better naming, enforces readability, and flags architectural complexity.

---

## 🏗️ Technical Architecture

Diffmind is built as a high-performance **TypeScript Monorepo**:

- **`packages/core-wasm`**: The heavy-lifting Rust engine using [Candle](https://github.com/huggingface/candle) for GGUF inference, compiled to specialized WebAssembly.
- **`apps/local-cli`**: A sleek Node.js interface that orchestrates git diff capture and model management.
- **`packages/shared-types`**: Unified data structures for cross-engine consistency.

---

## 🚀 Quick Start

No API keys, no cloud sign-ups. Just run it.

### Via npx (Recommended)
Analyze your current changes against the `main` branch instantly:
```bash
npx @diffmind/cli --branch main
```

### Global Installation
```bash
npm install -g @diffmind/cli

# Run anywhere
diffmind --branch main
```

---

## 🛠️ Commands

Diffmind provides a few specialized commands to manage your local AI environment.

### 1. `review` (Default)
Run the AI code review against your git changes.
```bash
diffmind --branch main
```

### 2. `index`
Build a symbol index of your local repository. This enables **Local RAG**, allowing the AI to understand the definitions of functions and types referenced in your diff.
```bash
diffmind index
```

### 3. `download`
Explicitly manage or refresh your local AI model files.
```bash
# Check if model exists and download if missing
diffmind download

# Force a fresh download (useful if original download was interrupted)
diffmind download --force
```

---

## 📖 Usage Examples

### Analyze against a specific branch
```bash
diffmind --branch develop
```

### CI/CD Integration (JSON output)
```bash
diffmind --format json --min-severity high
```

### Business-Aware Reviews
Provide requirements or ticket descriptions to verify that the code actually meets the business criteria:
```bash
diffmind --branch develop --context ticket.md
```

### Architectural-Aware Reviews (Local RAG)
Build an index of your project's symbols to allow the AI to "look up" function and type definitions referenced in your diff:
```bash
# 1. Build the index (only needed once or after large changes)
diffmind index

# 2. Run your review — context is automatically retrieved
diffmind --branch develop
```

### Custom Diff (Stdin)
```bash
git diff main...HEAD | diffmind --stdin
```

---

## 📊 Output Options

| Option | Description | Default |
|--------|-------------|---------|
| `--branch, -b` | Base branch to compare against | `main` |
| `--format, -f` | Output format (`markdown` or `json`) | `markdown` |
| `--context, -c` | Business context (ticket text or file path) | `""` |
| `--min-severity` | Minimum severity level (`high`, `medium`, `low`) | `low` |
| `--stdin` | Read diff from stdin | `false` |
| `--output, -o` | Write report to a specific file | `stdout` |

---

## 🗺️ Roadmap

I am committed to making Diffmind the ultimate local-first AI companion for developers. As a developer, I am building the foundation for a privacy-first engineering future, and I am actively looking for contributors to join this mission!

- [x] **v0.3.x**: Local RAG Integration (Symbol Indexing & Semantic Context)
- [ ] **v0.4.0**: VS Code Extension (Real-time IDE feedback)
- [ ] **v0.5.0**: Custom Rule Engine (Team-specific standards)
- [ ] **Advanced CI Guards**: Ready-to-use GitHub Action templates to enforce security baselines and block high-severity PRs automatically.
- [ ] **Multi-Language Support**: Expanding the deep-reasoning review capabilities to Go, Python, Rust, and Java.

---

## ⚙️ Requirements & Limitations

### Hardware Requirements
- **RAM**: 8GB recommended (4GB minimum). The model itself occupies ~2.2GB.
- **CPU**: Modern x64 or ARM64 processor. Apple Silicon (M1/M2/M3) provides exceptional performance via Wasm SIMD.
- **Disk**: ~2.5GB free space for the local model download. You can pre-fetch this using `diffmind download`.

### Current Limitations
- **Wasm Memory**: Due to the 32-bit architecture of default WebAssembly runtimes, the heap is limited to 4GB. Diffmind automatically chunks large diffs to handle this, but extremely massive changes may see reduced context awareness.
- **Language Focus**: While the underlying model (Qwen2.5-Coder) is multi-lingual, the current review persona is optimized for **TypeScript, JavaScript, NestJS, and React Native**.

---

## 🛡️ License

Distributed under the MIT License. See `LICENSE` for more information.

---

## ✨ Support the Local-First Movement

If you believe code reviews should be private and fast, consider contributing to the diffmind core.

*Built with ❤️ by Tech Lead, for Tech Leads.*
