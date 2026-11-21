#!/usr/bin/env bash
# Tests for scripts/action/review.sh — the GitHub Action's review step.
#
# Runs against real git repositories built in a temp dir, with a stub in place
# of the diffmind binary that records the argv and stdin it was given. No model
# is downloaded and nothing touches the network.
#
#   bash scripts/action/test-review.sh
#
# Adversarial inputs here only ever try to create marker files inside the
# sandbox; the assertion is that the marker never appears.

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/review.sh"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/diffmind-action-test.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); printf '  ok    %s\n' "$1"; }
failed() { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n' "$1"; [ -n "${2:-}" ] && printf '        %s\n' "$2"; }

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
export GIT_CONFIG_NOSYSTEM=1 HOME="$SANDBOX/home"
mkdir -p "$HOME"
git config --global init.defaultBranch main
git config --global protocol.file.allow always
git config --global advice.detachedHead false

# ── Stub diffmind ────────────────────────────────────────────────────────────

STUB_DIR="$SANDBOX/stub"
mkdir -p "$STUB_DIR"
cat >"$STUB_DIR/diffmind" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = "--help" ]; then
  if [ "${STUB_SUPPORTS_REQUIRE_COMPLETE:-1}" = "1" ]; then
    echo "      --require-complete   Fail (exit 2) when part of the diff was not reviewed"
  fi
  if [ "${STUB_SUPPORTS_BACKEND:-1}" = "1" ]; then
    echo "      --backend <KIND>     Inference backend"
  fi
  exit 0
fi
# Record the configuration diffmind would load from the working tree.
cat .diffmind/config.toml >"$STUB_LOG.config" 2>/dev/null || : >"$STUB_LOG.config"
if [ -e .diffmind/baseline.json ]; then echo present >"$STUB_LOG.baseline"; fi
: >"$STUB_LOG.argv"
for a in "$@"; do printf '%s\n' "$a" >>"$STUB_LOG.argv"; done
case " $* " in
  *" --stdin "*) cat >"$STUB_LOG.stdin" ;;
esac
# Write the report where asked, so output parsing is exercised.
out=""
prev=""
for a in "$@"; do
  [ "$prev" = "--output" ] && out="$a"
  prev="$a"
done
if [ -n "$out" ] && [ -n "${STUB_REPORT:-}" ]; then
  printf '%s' "$STUB_REPORT" >"$out"
fi
exit "${STUB_EXIT:-0}"
STUB
chmod +x "$STUB_DIR/diffmind"

# ── Fixture repositories ─────────────────────────────────────────────────────

ORIGIN="$SANDBOX/origin.git"
SEED="$SANDBOX/seed"

git init -q --bare "$ORIGIN"
# What GitHub allows and actions/checkout relies on: fetching a commit by sha.
git -C "$ORIGIN" config uploadpack.allowReachableSHA1InWant true
git -C "$ORIGIN" config uploadpack.allowAnySHA1InWant true

git init -q "$SEED"
(
  cd "$SEED"
  printf 'fn base() {}\n' >base.rs
  mkdir -p .diffmind
  printf '[review]\nfail_on = "high"\n' >.diffmind/config.toml
  printf 'old\n' >to_delete.txt
  printf 'keep\n' >to_rename.txt
  git add . && git commit -qm "base"
  # Enough history that a depth-1 checkout cannot see the merge base.
  for i in $(seq 1 60); do
    printf 'fn base_%s() {}\n' "$i" >>base.rs
    git commit -qam "base $i"
  done
  git remote add origin "$ORIGIN"
  git push -q origin main

  git checkout -qb "feature/ünïcode-branch"
  printf 'fn pr_change() { unsafe_thing(); }\n' >pr.rs
  # The PR also tries to switch its own gate off.
  printf '[review]\nignore = ["**"]\nmin_confidence = 1.0\n' >.diffmind/config.toml
  printf '{"entries":[]}\n' >.diffmind/baseline.json
  git rm -q to_delete.txt
  git mv to_rename.txt renamed.txt
  git add . && git commit -qm "pr change"
  git push -q origin "feature/ünïcode-branch"

  # Base moves on after the PR branched: these lines must NOT be reviewed.
  git checkout -q main
  printf 'fn base_only_after_branch() {}\n' >>base.rs
  git commit -qam "base moves on"
  git push -q origin main

  # GitHub's refs/pull/N/merge: base tip merged with the PR head.
  git checkout -q -b pr-merge main
  git merge -q --no-ff -m "Merge PR" "feature/ünïcode-branch"
  git push -q origin pr-merge:refs/pull/1/merge

  # A ref with no shared history at all.
  git checkout -q --orphan orphan
  git rm -rqf . >/dev/null
  printf 'x\n' >x.txt
  git add . && git commit -qm "orphan"
  git push -q origin orphan
)

# actions/checkout for a pull_request event: depth 1, detached at the merge ref.
checkout_pr() {
  local dir="$1" ref="${2:-refs/pull/1/merge}"
  rm -rf "$dir"
  git init -q "$dir"
  git -C "$dir" remote add origin "file://$ORIGIN"
  git -C "$dir" fetch -q --depth=1 origin "+$ref:refs/remotes/pull/1/merge"
  git -C "$dir" checkout -q --detach refs/remotes/pull/1/merge
}

# Run the script in a checkout. Extra env assignments come first.
run_review() {
  local dir="$1"
  shift
  (
    cd "$dir" || exit 99
    env -u DM_BASE -u DM_ARGS -u DM_FAIL_ON -u DM_OUTPUT -u DM_MODEL \
      DIFFMIND_BIN="$STUB_DIR/diffmind" \
      STUB_LOG="$SANDBOX/last" \
      GITHUB_OUTPUT="$SANDBOX/gh_output" \
      RUNNER_TEMP="$SANDBOX" \
      GITHUB_EVENT_NAME=pull_request \
      "$@" bash "$SCRIPT"
  ) >"$SANDBOX/log" 2>&1
}

reset_records() { rm -f "$SANDBOX"/last.* "$SANDBOX/gh_output" "$SANDBOX"/PWNED*; }
argv_has() { grep -qxF -- "$1" "$SANDBOX/last.argv" 2>/dev/null; }
output_of() { grep "^$1=" "$SANDBOX/gh_output" 2>/dev/null | tail -n1 | cut -d= -f2-; }
no_markers() { ! ls "$SANDBOX"/PWNED* >/dev/null 2>&1 && ! ls "$WORK"/PWNED* >/dev/null 2>&1; }

WORK="$SANDBOX/work"

echo "review.sh"

# 1. Shallow PR checkout: the merge base is found by deepening, and the diff is
#    the PR's change only.
reset_records
checkout_pr "$WORK"
if run_review "$WORK" GITHUB_BASE_REF=main; then
  if grep -q 'pr_change' "$SANDBOX/last.stdin" && ! grep -q 'base_only_after_branch' "$SANDBOX/last.stdin"; then
    pass "depth-1 PR checkout reviews exactly the PR's changes"
  else
    failed "depth-1 PR checkout reviews exactly the PR's changes" "stdin: $(head -c 300 "$SANDBOX/last.stdin" 2>/dev/null)"
  fi
else
  failed "depth-1 PR checkout reviews exactly the PR's changes" "$(tail -5 "$SANDBOX/log")"
fi

# 2. Deleted and renamed files reach the diff.
if grep -q 'deleted file mode' "$SANDBOX/last.stdin" 2>/dev/null && grep -q 'renamed.txt' "$SANDBOX/last.stdin" 2>/dev/null; then
  pass "deleted and renamed files are part of the reviewed diff"
else
  failed "deleted and renamed files are part of the reviewed diff"
fi

# 3. A base branch that cannot be fetched fails the job; it is never reviewed
#    as an empty diff.
reset_records
checkout_pr "$WORK"
run_review "$WORK" DM_BASE=does-not-exist
st=$?
if [ "$st" -eq 2 ] && [ ! -f "$SANDBOX/last.argv" ] && [ "$(output_of status)" = "2" ]; then
  pass "an unfetchable base fails with exit 2 and never runs a review"
else
  failed "an unfetchable base fails with exit 2 and never runs a review" "exit $st; $(tail -3 "$SANDBOX/log")"
fi

# 4. No shared history: refuse rather than diff against nothing.
reset_records
checkout_pr "$WORK"
run_review "$WORK" DM_BASE=orphan
st=$?
if [ "$st" -eq 2 ] && [ ! -f "$SANDBOX/last.argv" ]; then
  pass "a base with no merge base fails with exit 2"
else
  failed "a base with no merge base fails with exit 2" "exit $st"
fi

# 5. A unicode branch name is accepted as a base.
reset_records
checkout_pr "$WORK" "refs/heads/feature/ünïcode-branch"
run_review "$WORK" "DM_BASE=feature/ünïcode-branch"
st=$?
if [ "$st" -eq 0 ] && [ -f "$SANDBOX/last.argv" ]; then
  pass "a unicode base branch name is handled"
else
  failed "a unicode base branch name is handled" "exit $st; $(tail -3 "$SANDBOX/log")"
fi

# 6. Base == HEAD: an empty diff is a real, successful "nothing to review".
reset_records
checkout_pr "$WORK" "refs/heads/main"
run_review "$WORK" DM_BASE=main
st=$?
if [ "$st" -eq 0 ] && [ -f "$SANDBOX/last.stdin" ] && [ ! -s "$SANDBOX/last.stdin" ] && grep -q 'nothing to review' "$SANDBOX/log"; then
  pass "an empty diff from a successful git diff is reported, not hidden"
else
  failed "an empty diff from a successful git diff is reported, not hidden" "exit $st"
fi

# 7. Injection attempts never execute. Each tries to create a marker file.
checkout_pr "$WORK"
for case_ in \
  "DM_ARGS=\$(touch $SANDBOX/PWNED1) \`touch $SANDBOX/PWNED2\` ; touch $SANDBOX/PWNED3 && touch $SANDBOX/PWNED4 | touch $SANDBOX/PWNED5" \
  "DM_OUTPUT=x\"; touch $SANDBOX/PWNED6; \"" \
  "DM_OUTPUT=\$(touch $SANDBOX/PWNED7)" \
  "DM_TICKET=\$(touch $SANDBOX/PWNED8)'; touch $SANDBOX/PWNED9; '" \
  "DM_ARGS=--ticket \"\$(touch $SANDBOX/PWNED10)\""; do
  reset_records
  run_review "$WORK" GITHUB_BASE_REF=main "$case_"
  if no_markers; then
    pass "no execution: ${case_%%=*}=$(printf '%s' "${case_#*=}" | head -c 40)…"
  else
    failed "no execution: ${case_%%=*}" "a marker file was created"
  fi
done

# The literal text of an injected argument arrives as data.
reset_records
run_review "$WORK" GITHUB_BASE_REF=main "DM_ARGS=--ticket '\$(id)'"
if argv_has '$(id)'; then
  pass "quoted args are passed through literally"
else
  failed "quoted args are passed through literally" "argv: $(tr '\n' ' ' <"$SANDBOX/last.argv" 2>/dev/null)"
fi

# 8. Invalid values are rejected before anything runs.
for case_ in \
  "DM_MODEL=1.5b; id" \
  "DM_MODEL=../../etc" \
  "DM_FAIL_ON=high --no-baseline" \
  "DM_FORMAT=sarif\$(id)" \
  "DM_MIN_SEVERITY=critical" \
  "DM_BASE=main; touch $SANDBOX/PWNED11" \
  "DM_BASE=--upload-pack=touch $SANDBOX/PWNED12" \
  "DM_BASE=a..b" \
  "DM_OUTPUT=--fail-on=low" \
  "DM_ARGS=--ticket 'unterminated"; do
  reset_records
  run_review "$WORK" GITHUB_BASE_REF=main "$case_"
  st=$?
  if [ "$st" -eq 2 ] && [ ! -f "$SANDBOX/last.argv" ] && no_markers; then
    pass "rejected: $(printf '%s' "$case_" | head -c 50)"
  else
    failed "rejected: $case_" "exit $st"
  fi
done

# 9. fail-on: none never fails on findings — and passes no gate the CLI
#    would default to --min-severity.
reset_records
run_review "$WORK" GITHUB_BASE_REF=main DM_FAIL_ON=none STUB_EXIT=1
st=$?
if [ "$st" -eq 0 ] && ! argv_has --fail-on; then
  pass "fail-on none turns 'blocking findings' into a pass"
else
  failed "fail-on none turns 'blocking findings' into a pass" "exit $st"
fi

# ...but it never hides a failed run.
reset_records
run_review "$WORK" GITHUB_BASE_REF=main DM_FAIL_ON=none STUB_EXIT=2
st=$?
if [ "$st" -eq 2 ] && [ "$(output_of status)" = "2" ]; then
  pass "fail-on none still fails when the review did not complete"
else
  failed "fail-on none still fails when the review did not complete" "exit $st"
fi

# 10. Exit codes propagate; abnormal ones normalise to 2.
for pair in "0:0" "1:1" "2:2" "134:2" "137:2"; do
  reset_records
  run_review "$WORK" GITHUB_BASE_REF=main "STUB_EXIT=${pair%%:*}"
  st=$?
  if [ "$st" -eq "${pair##*:}" ]; then
    pass "diffmind exit ${pair%%:*} → action exit ${pair##*:}"
  else
    failed "diffmind exit ${pair%%:*} → action exit ${pair##*:}" "got $st"
  fi
done

# 11. The repository's config cannot choose the backend; the workflow can.
reset_records
run_review "$WORK" GITHUB_BASE_REF=main
if argv_has --backend && argv_has local; then
  pass "the local backend is pinned against repository config"
else
  failed "the local backend is pinned against repository config"
fi
reset_records
run_review "$WORK" GITHUB_BASE_REF=main "DM_ARGS=--backend ollama --backend-model m"
if [ "$(grep -cxF -- --backend "$SANDBOX/last.argv")" -eq 1 ] && argv_has ollama; then
  pass "a backend chosen in args is respected and not duplicated"
else
  failed "a backend chosen in args is respected and not duplicated"
fi

# 12. Strict completeness is requested when the binary supports it.
reset_records
run_review "$WORK" GITHUB_BASE_REF=main
if argv_has --require-complete; then
  pass "--require-complete is passed by default"
else
  failed "--require-complete is passed by default"
fi
reset_records
run_review "$WORK" GITHUB_BASE_REF=main DM_ALLOW_INCOMPLETE=true
if ! argv_has --require-complete; then
  pass "allow-incomplete: true omits --require-complete"
else
  failed "allow-incomplete: true omits --require-complete"
fi
reset_records
run_review "$WORK" GITHUB_BASE_REF=main STUB_SUPPORTS_REQUIRE_COMPLETE=0
st=$?
if [ "$st" -eq 0 ] && ! argv_has --require-complete && grep -q 'cannot report incomplete' "$SANDBOX/log"; then
  pass "an older binary is not handed a flag it would reject"
else
  failed "an older binary is not handed a flag it would reject" "exit $st"
fi

# 13. Outputs: findings counted from the report; a corrupt report warns.
reset_records
run_review "$WORK" GITHUB_BASE_REF=main \
  'STUB_REPORT={"runs":[{"results":[{},{},{}]}]}' "DM_OUTPUT=$SANDBOX/r.sarif"
if [ "$(output_of findings)" = "3" ] && [ "$(output_of report)" = "$SANDBOX/r.sarif" ]; then
  pass "findings and report outputs are set from the SARIF report"
else
  failed "findings and report outputs are set from the SARIF report" "findings=$(output_of findings)"
fi
reset_records
run_review "$WORK" GITHUB_BASE_REF=main 'STUB_REPORT={not json' "DM_OUTPUT=$SANDBOX/r.sarif"
if [ "$(output_of findings)" = "0" ] && grep -q 'could not count findings' "$SANDBOX/log"; then
  pass "an unreadable report is flagged rather than silently counted as zero"
else
  failed "an unreadable report is flagged rather than silently counted as zero"
fi

# 14. The PR is reviewed under the base's .diffmind/ configuration, so it
#     cannot relax the gate reviewing it; its config edits are still in the diff.
reset_records
checkout_pr "$WORK"
run_review "$WORK" GITHUB_BASE_REF=main
if grep -q 'fail_on = "high"' "$SANDBOX/last.config" 2>/dev/null && ! grep -q 'ignore' "$SANDBOX/last.config" \
  && [ ! -f "$SANDBOX/last.baseline" ] && grep -q '^+ignore = \["\*\*"\]' "$SANDBOX/last.stdin"; then
  pass "PR config edits are reviewed but do not govern the PR's own review"
else
  failed "PR config edits are reviewed but do not govern the PR's own review" \
    "config: $(tr '\n' ' ' <"$SANDBOX/last.config" 2>/dev/null)"
fi
reset_records
checkout_pr "$WORK"
run_review "$WORK" GITHUB_BASE_REF=main DM_CONFIG_FROM=head
if grep -q 'ignore' "$SANDBOX/last.config" 2>/dev/null && [ -f "$SANDBOX/last.baseline" ]; then
  pass "config-from: head uses the PR's own configuration"
else
  failed "config-from: head uses the PR's own configuration"
fi
reset_records
run_review "$WORK" GITHUB_BASE_REF=main DM_CONFIG_FROM=elsewhere
if [ $? -eq 2 ] && [ ! -f "$SANDBOX/last.argv" ]; then
  pass "an invalid config-from is rejected"
else
  failed "an invalid config-from is rejected"
fi

# 15. A binary without --backend (older than v0.7.1) is not handed it.
reset_records
checkout_pr "$WORK"
run_review "$WORK" GITHUB_BASE_REF=main STUB_SUPPORTS_BACKEND=0
if [ -f "$SANDBOX/last.argv" ] && ! argv_has --backend; then
  pass "--backend is only passed to a binary that supports it"
else
  failed "--backend is only passed to a binary that supports it"
fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
