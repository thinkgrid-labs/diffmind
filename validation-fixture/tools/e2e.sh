#!/usr/bin/env bash
# End-to-end checks of a built diffmind binary with the real 1.5B model.
# Temporary validation tooling for v0.9.3; runs on Linux, macOS and Windows
# (Git Bash). Usage: e2e.sh <path to diffmind binary>
# Every check prints PASS/FAIL; the script exits 1 if any check failed.

set -uo pipefail
B="$1"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
T="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/dm-e2e"
rm -rf "$T"; mkdir -p "$T"
FAILED=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; FAILED=1; }
check() { local name="$1"; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }
PY="$(command -v python3 || command -v python)"
jq_py() { "$PY" -I -c "import json,sys; d=json.load(open(sys.argv[1], encoding='utf-8')); print($2)" "$1"; }
native() { if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
IS_WIN=0; case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) IS_WIN=1 ;; esac

echo "== environment"
uname -a; "$B" --version; nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || true

export DIFFMIND_HOME; DIFFMIND_HOME="$(native "$T/home")"; mkdir -p "$T/home"
echo "== model download + verify"
"$B" download --model 1.5b >"$T/download.log" 2>&1
check "download exits 0" test $? -eq 0
"$B" download --model 1.5b --verify
check "verify exits 0" test $? -eq 0

echo "== fixture with unicode paths"
R="$T/fx"; mkdir -p "$R/données"; cd "$R" || exit 1
git init -q -b main; git config user.email v@example.invalid; git config user.name v
git config core.autocrlf false
cat > "données/prix_ünï.py" <<'PY'
def apply_discount(price, percent):
    return price - price * percent / 100
PY
cat > "données/caller.py" <<'PY'
from données.prix_ünï import apply_discount


def total(x):
    return apply_discount(x, 10)
PY
git add -A; git commit -qm base; git checkout -qb feature
cat > "données/prix_ünï.py" <<'PY'
def apply_discount(price, fraction):
    return price - price * fraction


def run(expr):
    return eval(expr)
PY
git commit -qam change
mkdir -p .diffmind
cat > .diffmind/rules.toml <<'EOF'
[[rule]]
id       = "validation.no-eval"
pattern  = '\beval\('
message  = "eval() executes arbitrary code"
severity = "high"
files    = ["*.py"]
EOF

run_review() { # name, extra args...
  local name="$1"; shift
  local t0 t1
  t0=$(date +%s)
  "$B" -b main --format json -o "$T/$name.json" --fail-on high --require-complete "$@" >"$T/$name.out" 2>"$T/$name.err"
  local code=$?
  t1=$(date +%s)
  echo "$code" >"$T/$name.code"
  echo "   $name: exit=$code wall=$((t1 - t0))s"
  return 0
}
code_of() { cat "$T/$1.code"; }

echo "== review (real model, no cache, in-process)"
run_review r1 --no-cache --no-daemon
check "review exits 1 (deterministic high finding present)" test "$(code_of r1)" -eq 1
check "no panic (exit 101) on review" test "$(code_of r1)" -ne 101
check "completeness is complete" test "$(jq_py "$T/r1.json" 'd["completeness"]["status"]')" = complete
check "deterministic eval finding reported" test "$(jq_py "$T/r1.json" 'sum(1 for f in d["findings"] if "no-eval" in (f.get("rule_id") or ""))')" -ge 1
check "every finding names a file in the diff" test "$(jq_py "$T/r1.json" 'all(f["file"].replace(chr(92),"/") in ("données/prix_ünï.py",) for f in d["findings"])')" = True
echo "   findings: $(jq_py "$T/r1.json" '[(f.get("rule_id"), f["file"], f.get("line"), f.get("severity")) for f in d["findings"]]')"

echo "== cache"
run_review c1
run_review c2
check "second cached run hits every unit" test "$(jq_py "$T/c2.json" 'd["stats"]["units_cached"] == d["stats"]["units"] and d["stats"]["units"] > 0')" = True
check "cached findings equal uncached" test "$(jq_py "$T/c1.json" 'sorted((f["file"],f.get("line"),f.get("issue")) for f in d["findings"])')" = "$(jq_py "$T/c2.json" 'sorted((f["file"],f.get("line"),f.get("issue")) for f in d["findings"])')"
n=0
for f in .diffmind/cache/*; do
  [ -f "$f" ] || continue
  printf 'garbage{not json' >"$f"; n=$((n + 1))
done
echo "   corrupted $n cache entries"
run_review c3
check "corrupted cache: review still completes" test "$(jq_py "$T/c3.json" 'd["completeness"]["status"]')" = complete
check "corrupted cache: entries are misses" test "$(jq_py "$T/c3.json" 'd["stats"]["units_cached"]')" -eq 0
check "corrupted cache: exit code unchanged" test "$(code_of c3)" -eq "$(code_of c1)"

echo "== corrupt model in an isolated home"
CH="$T/corrupt-home"; mkdir -p "$CH/.diffmind/models"
cp "$T/home/.diffmind/models/tokenizer.json" "$CH/.diffmind/models/"
head -c 1048576 "$T/home/.diffmind/models/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf" >"$CH/.diffmind/models/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf"
DIFFMIND_HOME="$(native "$CH")" "$B" -b main --format json -o "$T/bad.json" --no-cache --no-daemon --require-complete >"$T/bad.out" 2>"$T/bad.err"
code=$?
echo "   exit=$code; stderr: $(tr '\n' ' ' <"$T/bad.err" | cut -c1-300)"
check "corrupt model: exit 2" test "$code" -eq 2
check "corrupt model: no panic" bash -c "! grep -qi 'panicked' '$T/bad.err'"
check "corrupt model: not reported clean" bash -c "! grep -qi 'no issues found' '$T/bad.out' '$T/bad.err'"
DIFFMIND_HOME="$(native "$CH")" "$B" download --model 1.5b --verify >"$T/badverify.out" 2>&1
check "corrupt model: --verify fails" test $? -ne 0

echo "== daemon"
"$B" serve --idle-timeout 90 >"$T/serve.out" 2>"$T/serve.err" &
SERVE_PID=$!
up=0
for _ in $(seq 1 180); do
  if [ -f "$T/home/.diffmind/daemon.json" ] && "$B" serve --status >/dev/null 2>&1; then up=1; break; fi
  sleep 1
done
check "daemon starts" test "$up" -eq 1
"$B" serve --status
if [ "$IS_WIN" = 1 ]; then
  icacls "$(native "$T/home/.diffmind/daemon.json")" || true
  icacls "$(native "$T/home/.diffmind/cache-secret")" || true
else
  ls -l "$T/home/.diffmind/daemon.json" "$T/home/.diffmind/cache-secret"
  check "daemon.json is 0600" test "$(stat -c %a "$T/home/.diffmind/daemon.json" 2>/dev/null || stat -f %Lp "$T/home/.diffmind/daemon.json")" = 600
fi
"$PY" -I "$HERE/daemon_probe.py" "$T/home/.diffmind/daemon.json"
check "daemon authentication probe" test $? -eq 0
run_review d1 --no-cache
run_review d2 --no-cache
check "review through daemon exits 1" test "$(code_of d1)" -eq 1
check "daemon review complete" test "$(jq_py "$T/d1.json" 'd["completeness"]["status"]')" = complete
check "daemon findings equal in-process findings" test "$(jq_py "$T/d1.json" 'sorted((f["file"],f.get("line"),f.get("rule_id")) for f in d["findings"])')" = "$(jq_py "$T/r1.json" 'sorted((f["file"],f.get("line"),f.get("rule_id")) for f in d["findings"])')"
TOKEN_HEAD="$("$PY" -I -c "import json,sys;print(json.load(open(sys.argv[1]))['token'][:16])" "$T/home/.diffmind/daemon.json")"
check "token never appears in daemon or client logs" bash -c "! grep -rqF '$TOKEN_HEAD' '$T'/serve.out '$T'/serve.err '$T'/d1.err '$T'/d1.out"
"$B" serve --stop
stopped=0
for _ in $(seq 1 30); do if ! kill -0 "$SERVE_PID" 2>/dev/null; then stopped=1; break; fi; sleep 1; done
check "daemon stops on --stop" test "$stopped" -eq 1
check "daemon.json removed on stop" test ! -f "$T/home/.diffmind/daemon.json"

echo "== npm wrapper exit codes"
if command -v node >/dev/null 2>&1; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) P=linux-x64 ;; Linux-aarch64) P=linux-arm64 ;;
    Darwin-x86_64) P=darwin-x64 ;; Darwin-arm64) P=darwin-arm64 ;;
    *) P=win32-x64 ;;
  esac
  N="$T/npm/node_modules"; mkdir -p "$N/diffmind/bin" "$N/@diffmind/cli-$P/bin"
  cp "$ROOT/npm/cli/bin/diffmind.js" "$N/diffmind/bin/"
  cp "$ROOT/npm/platform/$P/package.json" "$N/@diffmind/cli-$P/"
  if [ "$IS_WIN" = 1 ]; then cp "$B" "$N/@diffmind/cli-$P/bin/diffmind.exe"; else cp "$B" "$N/@diffmind/cli-$P/bin/diffmind"; fi
  node "$N/diffmind/bin/diffmind.js" --version; check "npm: --version exits 0" test $? -eq 0
  node "$N/diffmind/bin/diffmind.js" --definitely-not-a-flag >/dev/null 2>&1; check "npm: bad flag exits 2" test $? -eq 2
  node "$N/diffmind/bin/diffmind.js" -b main --format json -o "$T/npm.json" --no-daemon --fail-on high >/dev/null 2>&1
  check "npm: blocking review passes exit 1 through" test $? -eq 1
  rm -rf "$N/@diffmind"
  node "$N/diffmind/bin/diffmind.js" --version >/dev/null 2>&1; check "npm: missing platform package exits 2" test $? -eq 2
else
  echo "SKIP npm checks: node not found"
fi

echo "== done"
exit "$FAILED"
