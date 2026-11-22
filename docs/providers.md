# Models and providers

Diffmind separates the reviewer from the model. The diff parsing, filtering,
code graph, fixed rules, suppressions, baseline and exit codes are the same
whichever model you choose. Only the model findings (`DM900.*` and
`rulebook.*`) change.

## At a glance

| Provider | Transport | Authentication | How to select it | Status |
| --- | --- | --- | --- | --- |
| **Built-in local model** (Qwen2.5-Coder GGUF, run in-process with [candle](https://github.com/huggingface/candle)) | none: runs inside the `diffmind` process | none | default (`--backend local`) | **Default.** Tested in CI and with the real 1.5B model on Linux, macOS and Windows |
| **Ollama** | HTTP, Ollama's native `/api/chat` | none by default; optional bearer token from an environment variable | `--backend ollama --backend-model <name>` | Supported. Covered by tests against a stub server |
| **OpenAI-compatible server** (vLLM, LM Studio, llama.cpp server, LiteLLM, …) | HTTP, `POST <url>/chat/completions` | optional bearer token from an environment variable | `--backend openai-compatible --backend-url <url> --backend-model <name>` | Supported protocol. Covered by tests against a stub server; individual servers are not tested by the project |
| Hosted APIs (OpenAI, Anthropic, others) | n/a | n/a | n/a | **No built-in backend.** A hosted endpoint that speaks the OpenAI chat-completions format *may* work through `openai-compatible`, but none is tested or supported, and Diffmind requests JSON mode (`response_format: json_object`), which not every provider accepts |
| `claude-cli` or other CLI-based models | n/a | n/a | n/a | **Planned**, not implemented. See the [roadmap](../ROADMAP.md) |
| OAuth / account sign-in | n/a | n/a | n/a | **Not supported.** Remote backends take an API key from an environment variable, nothing else |

## The default: the built-in local model

```bash
diffmind download            # interactive picker with a RAM check
diffmind download --model 7b # or choose a size directly
diffmind                     # review with the default 1.5B model
```

- **Needs a one-time download.** Nothing runs until a model is on disk. The
  default 1.5B model is 1.1 GB, downloaded from Hugging Face to
  `~/.diffmind/models/`. Installing and downloading need the internet. Reviews
  after that do not.
- **Pinned bytes.** Each model is locked to a fixed Hugging Face commit and
  checked against a SHA-256 compiled into the binary. A file that does not match
  is rejected, and an interrupted download never leaves a half-written file in
  place. `diffmind download --model 1.5b --verify` re-checks an existing file.
- **Hardware.** macOS builds include Metal (GPU) and Accelerate (CPU math);
  `--device auto` picks Metal when it is available. Linux and Windows builds run
  on the CPU. There is no CUDA support. v0.9.3 release validation ran on CPU
  only; Metal was not re-tested for that release.
- **Repeatable on one machine.** With the defaults (temperature 0, fixed seed),
  the local backend uses greedy decoding and a JSON-constrained sampler. In
  v0.9.3 release validation, the same diff gave byte-identical findings on
  repeat runs on the same host. Across different hardware the *findings* were
  the same but wording and, occasionally, a line number differed. Treat
  reproducibility as per-machine.

| Model | Download | Min RAM | Use for |
| --- | --- | --- | --- |
| `0.5b` | 0.5 GB | 2 GB | Small CI runners; lint-grade |
| `1.5b` ★ | 1.1 GB | 4 GB | Default |
| `3b` | 2.1 GB | 6 GB | |
| `7b` | 4.7 GB | 10 GB | Noticeably better security analysis |
| `14b` | 9.0 GB | 18 GB | |
| `32b` | 19.9 GB | 40 GB | Best local quality, if you have the machine |

**What to expect from the default model.** A 1.5B model catches obvious
mistakes but also produces false positives, including high-severity ones on
harmless code. On CPU it is slow: in v0.9.3 validation, a two-unit review took
roughly 45 seconds on a GitHub-hosted Linux ARM64 runner and two to six
minutes on Linux x86_64 and Windows runners. Metal was not benchmarked.
Bigger models are better and slower. If you gate CI on model findings, start
with the action's `fail-on: none` (or a high CLI `--fail-on`) and read the
results for a while first.

## Your own model server

Use this when you already run a bigger model on your machine or your network.

```bash
# Ollama (default URL http://localhost:11434)
diffmind --backend ollama --backend-model qwen2.5-coder:14b

# Any server that speaks the OpenAI chat-completions API
# (default URL http://localhost:8000/v1)
diffmind --backend openai-compatible \
  --backend-url http://localhost:8000/v1 --backend-model my-model
```

Or in `.diffmind/config.toml`:

```toml
[backend]
kind           = "ollama"              # local | ollama | openai-compatible
url            = "http://localhost:11434"
model          = "qwen2.5-coder:14b"
api_key_env    = "DIFFMIND_API_KEY"    # name of the variable, never the key
context_tokens = 32768                 # optional: the model's context window
timeout_secs   = 300                   # optional: per-request timeout
```

### Where your code goes

**A remote backend receives your code.** Each request carries the diff hunks
plus the context Diffmind assembled: the enclosing function, callers,
definitions, the matching test file and any matching rule sets. If the URL is
`localhost`, that stays on your machine. If it is another host, it leaves your
machine, and what happens to it depends on that host. "Local-first" describes
the default, not every configuration.

The backend can be chosen by a committed `.diffmind/config.toml`, including the
URL and the name of the environment variable sent as the API key. Before
running Diffmind or its git hooks in a repository you do not trust, read that
file or pass `--backend local`. The GitHub Action pins the local backend for
this reason.

### Credentials

- API keys are read **only from the environment**: `DIFFMIND_API_KEY`, or the
  variable named by `--backend-api-key-env` / `api_key_env`. They are never
  read from `config.toml`, which is meant to be committed.
- A key is sent as a bearer token. Diffmind refuses to send it over plain
  `http://` to a non-loopback host. Use `https://`, or set
  `DIFFMIND_ALLOW_INSECURE_API_KEY=1` if you accept the risk.
- Backend URLs are shown in run records and errors without `user:password@` or
  query strings, and an echoed key is masked. Do not put credentials in the URL
  itself; the cleartext refusal covers the API key only.

### Behaviour and limits

- **Bounded requests.** 10-second connect timeout, 300-second request timeout
  (`--backend-timeout`), responses capped at 8 MiB. HTTP 429, 500, 502, 503,
  504 and dropped
  connections are retried at most twice with backoff (honouring `Retry-After`
  up to 30 s). Timeouts and other 4xx errors are not retried. Redirects are not
  followed.
- **No constrained decoding.** The JSON-constrained sampler only exists inside
  the built-in backend. Remote backends are asked for JSON mode
  (Ollama `format: json`, OpenAI-style `response_format: json_object`), which
  is weaker. Output that cannot be parsed marks that unit unreviewed, and the
  review is reported as incomplete.
- **No same-answer guarantee.** Diffmind sends temperature 0 and a fixed seed,
  but whether a server honours them is up to the server.
- **Estimated token counts.** Remote token counts are estimates, shown with `~`.
- **Cache identity.** Cached results are keyed on the exact prompt and the
  backend identity. For Ollama that includes the model digest, so re-pulling a
  tag invalidates old entries. OpenAI-compatible servers report no revision. If
  the weights behind a model name change, run `diffmind cache clear`.

## Choosing a model: cost, latency and quality

| | Built-in local | Your own server | Hosted API (via `openai-compatible`, unsupported) |
| --- | --- | --- | --- |
| Per-run cost | none (your CPU time) | none (your hardware) | per token, set by the provider |
| Latency | seconds to minutes per review on CPU, depending on runner and model size | depends on your hardware | depends on the provider |
| Quality | limited by model size (0.5B–32B) | whatever you run | potentially much higher |
| Code leaves the machine | no | only if the server is remote | yes |
| Repeatable output | per machine | not guaranteed | not guaranteed |

## In the GitHub Action

The action pins the built-in local backend so that a pull request cannot point
the review at a server of its choosing. To use your own server in CI, pass
`--backend …` through the action's `args` input and keep the key in a
repository secret exposed as an environment variable. See
[CI and GitHub Actions](ci.md).
