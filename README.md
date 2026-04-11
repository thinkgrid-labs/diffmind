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

## 📖 Usage Examples

### Analyze against a specific branch
```bash
diffmind --branch develop
```

### CI/CD Integration (JSON output)
```bash
diffmind --format json --min-severity high
```

### Custom Diff (Stdin)
```bash
git diff main...HEAD | diffmind --stdin
```

---

---

## 📊 Output Options

| Option | Description | Default |
|--------|-------------|---------|
| `--branch, -b` | Base branch to compare against | `main` |
| `--format, -f` | Output format (`markdown` or `json`) | `markdown` |
| `--min-severity` | Minimum severity level (`high`, `medium`, `low`) | `low` |
| `--stdin` | Read diff from stdin | `false` |
| `--output, -o` | Write report to a specific file | `stdout` |

---

## 🗺️ Roadmap

I am committed to making Diffmind the ultimate local-first AI companion for developers. As a developer, I am building the foundation for a privacy-first engineering future, and I am actively looking for contributors to join this mission!

- [ ] **Custom Rule Engine**: Define project-specific review standards using simple YAML.
- [ ] **VS Code Extension**: Get real-time AI feedback directly in your editor as you code.
- [ ] **Local RAG Integration**: Context-aware reviews that understand your entire repository's architecture.
- [ ] **Multi-Language Support**: Expanding beyond TypeScript to Go, Python, and Rust.
- [ ] **Advanced CI Guards**: Pre-built action templates to block PRs with high-severity findings globally.

---

## 🛡️ License

Distributed under the MIT License. See `LICENSE` for more information.

---

## ✨ Support the Local-First Movement

If you believe code reviews should be private and fast, consider contributing to the diffmind core.

*Built with ❤️ by Tech Leads, for Tech Leads.*
