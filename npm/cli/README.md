# diffmind

Local-first AI code review for your git diffs. Runs entirely on your machine — no cloud, no API keys, no subscription.

This package is a thin installer for the [diffmind](https://github.com/thinkgrid-labs/diffmind) binary. The actual executable is Rust, shipped as a prebuilt binary in a per-platform package; npm downloads only the one matching your OS and CPU.

```bash
npx @diffmind/cli --help

# or install globally
npm install -g @diffmind/cli
```

## Quick start

```bash
# 1. Download a model (one-time; pick the size that suits your hardware)
diffmind download

# 2. Review the current branch against main
diffmind --branch main
```

`diffmind download` with no arguments shows an interactive picker with the RAM and disk requirements for each model, from Qwen2.5-Coder-0.5B up to 32B. Pick whatever your machine can run — bigger models give better reviews.

## Supported platforms

| Platform | Package |
| --- | --- |
| macOS Apple Silicon | `@diffmind/cli-darwin-arm64` |
| macOS Intel | `@diffmind/cli-darwin-x64` |
| Linux x86_64 | `@diffmind/cli-linux-x64` |
| Linux ARM64 | `@diffmind/cli-linux-arm64` |
| Windows x86_64 | `@diffmind/cli-win32-x64` |

Linux binaries are glibc-linked. On musl (Alpine), use a glibc base image or build from source.

## Not using npm?

npm is one of several install paths, and the others skip Node entirely:

```bash
# macOS / Linux
curl -fsSL https://github.com/thinkgrid-labs/diffmind/releases/latest/download/install.sh | bash

# From source
cargo install --git https://github.com/thinkgrid-labs/diffmind diffmind
```

Full documentation, model list, TUI keybindings, CI usage and `.diffmind/rules.toml` reference: **https://github.com/thinkgrid-labs/diffmind**

## License

MIT
