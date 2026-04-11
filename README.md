**Diffmind** is a high-performance, local-first AI security and code review agent designed specifically for modern engineering teams. It features a state-of-the-art **Dual-Engine Architecture** (Native + Wasm), providing expert-level code reviews with **zero latency**, **total data privacy**, and support for large 3B+ parameter models.

---

## ⚡ Why Diffmind?

Standard AI review tools often require sending your proprietary source code to third-party APIs. **Diffmind changed the game.**

- 🔒 **Absolute Privacy**: Your code never leaves your machine. Analysis happens locally via our hybrid inference engine.
- 🚀 **Zero Latency**: No network round-trips. Get instant feedback on your git diffs as you work.
- 🛠️ **Expert-Level Insight**: Powered by **Qwen2.5-Coder (1.5B/3B)**, an LLM specifically optimized for deep reasoning in TypeScript, NestJS, and React Native environments.
- 🏎️ **Native Performance**: Leverages N-API bindings to bypass memory limits and deliver lightning-fast reviews.

---

## 🔍 Comprehensive Review Coverage

Diffmind acts as a Senior Software Engineer for your feature branches, analyzing four critical dimensions:

- **🛡️ Security**: Detects hardcoded secrets, injection risks, and insecure data flow.
- **💎 Quality**: Identifies logical bugs, anti-patterns, and API misuse in NestJS and React Native.
- **⚡ Performance**: Highlights inefficient loops, memory overhead, and hydration bottlenecks.
- **📖 Maintainability**: Suggests better naming, enforces readability, and flags architectural complexity.

---

---

## 🏎️ Dual-Engine Architecture: Why it matters

Diffmind v0.4.0 introduces a state-of-the-art **Hybrid Inference Engine**. Most local AI tools force a choice between portability (Wasm) and performance (Native). **Diffmind gives you both.**

### 1. The Native Engine (N-API)
For power users and large codebases.
- **Bypasses the 4GB Limit**: Traditional WebAssembly runtimes are restricted to a 4GB memory heap. Diffmind Native Engine (built with Rust & `napi-rs`) handles models of any size (3B, 7B+) directly on your system RAM.
- **Direct Hardware Access**: Optimized CPU instructions (AVX, SIMD) are accessed natively for up to 3x faster inference than Wasm.
- **The 3B Powerhouse**: Enables the **Qwen2.5-Coder-3B** model, which offers significantly deeper security reasoning.

### 2. The Wasm Fallback
For universal portability and zero-setup environments.
- **No Binaries Required**: Runs entirely within the Node.js V8 engine using WebAssembly.
- **Universal**: Perfect for 1.5B and 0.5B models where portability is more important than raw speed.

### 3. The Intelligent Router
You don't have to choose. The Diffmind CLI features a smart router that:
1. Attempts to load the **Native Engine** for maximum speed.
2. If the native binary is missing or incompatible, it **silently falls back to Wasm**.
3. It intelligently warns you if a chosen model (like 3B) requires Native but only Wasm is available.

---

## 🤖 Supported Models

| Model ID | Name | Size | Optimized For | Engine |
|----------|------|------|---------------|--------|
| `1.5b` (Default) | Qwen2.5-Coder-1.5B | ~1.1 GB | General reviews, quick feedback | Wasm / Native |
| `3b` | Qwen2.5-Coder-3B | ~2.1 GB | Deep security logic, complex refactors | **Native Recommended** |

---

## 🏗️ Project Structure

- **`packages/core-engine`**: The unified Rust inference logic (powered by HuggingFace Candle).
- **`packages/core-native`**: High-performance Node.js native addon (C++ equivalent speed).
- **`packages/core-wasm`**: Portable WebAssembly bindings for universal execution.
- **`apps/local-cli`**: Orchestration, git integration, and the **Intelligent Router**.

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
# Check if model exists and download if missing (defaults to 1.5b)
diffmind download

# Download the high-performance 3B model
diffmind download --model 3b

# Force a fresh download
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
- [x] **v0.4.0**: Dual-Engine Stabilization (Native + Wasm, 3B Model Support)
- [ ] **v0.5.0**: VS Code Extension (Real-time IDE feedback)
- [ ] **Custom Rule Engine**: Team-specific standards and security baselines.
- [ ] **Multi-Language Support**: Expanding deep-reasoning capabilities to Go, Python, Rust, and Java.

---

## ⚙️ Requirements & Limitations

### Hardware Requirements
- **RAM**: 8GB recommended (4GB minimum). The model itself occupies ~2.2GB.
- **CPU**: Modern x64 or ARM64 processor. Apple Silicon (M1/M2/M3) provides exceptional performance via Wasm SIMD.
- **Disk**: ~2.5GB free space for the local model download. You can pre-fetch this using `diffmind download`.

### Current Limitations
- **Large Models (3B+)**: While WebAssembly runtimes are often limited to 4GB of RAM, Diffmind's **Native Engine** bypasses this limit, enabling high-quality reviews with the 3B parameter model on machines with 8GB+ RAM.
- **Language Focus**: While the underlying model (Qwen2.5-Coder) is multi-lingual, the current review persona is optimized for **TypeScript, JavaScript, NestJS, and React Native**.

---

## 🛡️ License

Distributed under the MIT License. See `LICENSE` for more information.

---

## ✨ Support the Local-First Movement

If you believe code reviews should be private and fast, consider contributing to the diffmind core.

*Built with ❤️ by Tech Lead, for Tech Leads.*
