# CI, GitHub Actions and git hooks

Diffmind is a normal CLI with stable exit codes, so it runs in any CI system.
For GitHub there is a composite action that installs the binary, caches the
model, reviews the pull request and publishes the result.

## GitHub Actions

```yaml
name: diffmind
on: pull_request

permissions:
  contents: read
  security-events: write   # only for SARIF upload

jobs:
  review:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: thinkgrid-labs/diffmind@v0.9.3
        with:
          version: v0.9.3   # the binary; defaults to "latest" otherwise
          model: 1.5b
          fail-on: high
```

**Pin both the action and `version`.** `uses: …@v0.9.3` selects the action's
scripts; the `version` input selects which release binary those scripts
install, and it defaults to `latest`. Without it, a new release changes the
binary under a pinned action.

What the action does:

1. Restores `~/.diffmind/models` from the Actions cache (keyed on OS and model),
   installs the binary with the release `install.sh`, and downloads the model
   if the cache missed.
2. Deepens a shallow clone until the merge base with the base branch is
   reachable, so the default `actions/checkout` depth of 1 works. If it cannot
   find the merge base it fails with exit 2 rather than review an empty diff.
3. Reviews `origin/<base>...HEAD` under the base branch's `.diffmind/`
   configuration (see [What a pull request can change](#what-a-pull-request-can-change)).
4. Writes the report and, depending on inputs, uploads SARIF to code scanning
   (inline annotations on the diff), adds a Markdown job summary, or posts and
   then updates a single PR comment (on `pull_request` events; on very busy
   PRs a duplicate comment is possible).

### Inputs

| Input | Default | Meaning |
| --- | --- | --- |
| `version` | `latest` | Release tag of the binary to install, e.g. `v0.9.3` |
| `model` | `1.5b` | `0.5b`, `1.5b`, `3b`, `7b`, `14b`, `32b` |
| `base` | PR base branch | Base ref to diff against. Set it for `push` or `merge_group` events |
| `min-severity` | `low` | Lowest severity reported |
| `fail-on` | `high` | Severity that fails the job; `none` never fails on findings |
| `format` | `sarif` | `text`, `json`, `sarif`, `markdown` |
| `output` | `diffmind.sarif` | Report path |
| `upload-sarif` | `true` | Upload to GitHub code scanning (needs `format: sarif`) |
| `comment` | `false` | Post the report as a PR comment (needs `format: markdown`) |
| `ticket` | | Acceptance criteria (path or text) to check the diff against |
| `args` | | Extra CLI flags, split like a quoted shell string but never evaluated |
| `config-from` | `base` | `base` reviews under the merge base's `.diffmind/`; `head` uses the PR's |
| `allow-incomplete` | `false` | Let the job pass when part of the diff was not reviewed |

Outputs: `findings` (count; filled in for `sarif` and `json` formats),
`report` (path), `status` (0, 1 or 2).

For a PR comment instead of annotations:

```yaml
      - uses: thinkgrid-labs/diffmind@v0.9.3
        with:
          version: v0.9.3
          format: markdown
          output: diffmind.md
          comment: true
          fail-on: none
```

The action runs on Linux and macOS runners. It does not support Windows
runners, because it installs through `install.sh`.

### Permissions

Grant only what the outputs you use need: `contents: read` always,
`security-events: write` for SARIF upload, `pull-requests: write` for comments.
The action uses the workflow's built-in `GITHUB_TOKEN` for those uploads. It
needs no extra secret or bot account with the default local model.

On pull requests from forks, GitHub gives the workflow a read-only token, so
SARIF upload and comments are expected to be unavailable there. The review
itself still runs and gates the job.

### Runtime and cost

The default model runs on the runner's CPU. On a GitHub-hosted Linux x86_64
runner a two-unit review with the 1.5B model took two to six minutes in v0.9.3
validation (about 45 seconds on a Linux ARM64 runner), plus the model download
when the cache is cold. There is
no per-token bill, but runner minutes apply. The `0.5b` model is faster and
weaker.

### What a pull request can change

A pull request can edit `.diffmind/` like any other file. To stop a PR from
relaxing the gate that reviews it, the action (from v0.9.3) restores
`config.toml`, `rules.toml`, `rules/` and `baseline.json` from the merge base
before reviewing, and pins the built-in local backend unless `args` chooses
another one. Edits to those files take effect once merged. Note that they are
not themselves reviewed by default: rule-set `.md` files are filtered as
documentation and `.diffmind/*.toml` as tool configuration, so review them by
hand.

This protection has limits:

- **It needs a base branch.** On `merge_group`, `push` and other events without
  one, the checkout's own `.diffmind/` is used unless you set the `base` input.
- **Generated-file markers are not covered.** A PR can mark a file
  `linguist-generated` in `.gitattributes`, or add an `@generated` banner, and
  that file is skipped entirely, including by the fixed rules.
- **The model reads the code under review.** Diffmind fences repository text as
  data, discards model findings about files outside the unit being reviewed,
  and escapes model output in Markdown and terminal output. That reduces prompt
  injection; it does not prevent it. A comment in the diff can still talk a
  small model out of reporting something.

- **Triage can skip files.** On diffs touching six or more files, a first pass
  may skip files it judges low-risk; the review still counts as complete. Add
  `args: --triage off` to review every file.

So: review changes to `.diffmind/` and `.gitattributes` the way you review
changes to CI, and prefer blocking on findings you can verify.

## Any other CI

```bash
git diff origin/main...HEAD \
  | diffmind --stdin --format sarif --output diffmind.sarif --fail-on high
```

GitLab, Jenkins, Buildkite and others can use the same command. Cache
`~/.diffmind/models` between runs to avoid re-downloading the model.

### Exit codes

| Exit | Meaning |
| --- | --- |
| `0` | Review complete; nothing at or above `--fail-on` |
| `1` | Review complete; findings at or above `--fail-on` |
| `2` | Diffmind failed (bad flag, missing model, backend down), **or** part of the diff was not reviewed and the run requires a complete review |

A unit is left unreviewed when the model's answer cannot be parsed or the unit
does not fit the context window. When the `CI` environment variable is set,
that fails the run by default. Override it with `--require-complete` /
`--allow-incomplete` or `[review] require_complete`. JSON output carries a
`completeness` object and SARIF sets `invocations[0].executionSuccessful`, so a
consumer can tell "no findings" from "not looked at".

If the backend fails part-way through, Diffmind still writes the report
(deterministic findings plus every unit reviewed before the failure) and then
exits 2.

### What `--fail-on` gates

`--fail-on` is a severity threshold. It applies to every finding shown,
deterministic and model findings alike. There is no option yet to gate only on
deterministic findings (`DM001`, `DM002`, `custom.*`). If model false positives
fail your builds, use a higher threshold, a larger model, inline suppressions
or a baseline. In the GitHub Action, `fail-on: none` makes the review advisory
(the CLI's `--fail-on` accepts only `low`, `medium` or `high`).

## Git hooks

```bash
diffmind install-hooks --hook pre-push --min-severity high
diffmind install-hooks --hook pre-commit
```

The generated hook exits 0 when `diffmind` is not installed, so it does not
block a teammate who has not installed it. If the binary is installed but no
model has been downloaded, the hook fails and blocks. It does not overwrite a
hook it did not write unless you pass `--force`.

### pre-commit framework

```yaml
repos:
  - repo: https://github.com/thinkgrid-labs/diffmind
    rev: v0.9.3
    hooks:
      - id: diffmind            # staged changes, blocks on high
      # - id: diffmind-push     # whole branch, pre-push stage
```

The hooks use `language: system`, so install the binary separately first. A
third hook, `diffmind-rules-only`, runs with `--max-tokens 1 --triage off` to
keep generation short. It still loads the model, and the token limit is raised
to Diffmind's minimum of 64, so model findings are still possible.
