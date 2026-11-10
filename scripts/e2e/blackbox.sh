#!/usr/bin/env bash
# Black-box checks of an installed HyperOx trio. Prints PASS/FAIL/WARN/SKIP per
# check; exits 1 if any check FAILed (WARN and SKIP never fail the run).
# Env: HC (h0x-core), HRT (h0x), HX (h0x-ade) = paths to the binaries; the caller
# has already isolated HOME, XDG_*, APPDATA, H0X_CORE_SESSION and friends.
# E2E_SANDBOX=1: running inside an OpenShell sandbox (adds the policy checks,
# skips what needs a display or systemd).
set -uo pipefail

: "${HC:?}" "${HRT:?}" "${HX:?}"
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) os=windows ;;
esac
# Native-looking (C:/...) path for programs the core starts; same path elsewhere.
if [ "$os" = windows ]; then scripts=$(cygpath -m "$here"); else scripts=$here; fi
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
# The bash the core starts: by full path, because on Windows a bare `bash` may be WSL's.
if [ "$os" = windows ]; then bashx=$(cygpath -m "$(command -v bash)"); else bashx=bash; fi

fails=0
passes=0
pass() { echo "PASS  $1"; passes=$((passes + 1)); }
fail() { echo "FAIL  $1${2:+  -- $2}"; fails=$((fails + 1)); }
warn() { echo "WARN  $1  -- $2"; }
skip() { echo "SKIP  $1  -- $2"; }
tailc() { printf '%s' "$1" | tail -c 400 | tr '\n' ' '; }
# ok NAME CMD...: PASS when CMD succeeds.
ok() { local n=$1 out; shift; if out=$("$@" 2>&1); then pass "$n"; else fail "$n" "$(tailc "$out")"; fi; }
# poll SECS CMD...: true as soon as CMD succeeds, false after SECS.
poll() { local end=$((SECONDS + $1)); shift; while [ "$SECONDS" -lt "$end" ]; do "$@" >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }
# has CMD...: the output of the rest matches the pattern in $PAT (extended regex).
hc_has() { local pat=$1; shift; "$HC" "$@" 2>&1 | grep -Eq -- "$pat"; }
hc_has_f() { local lit=$1; shift; "$HC" "$@" 2>&1 | grep -Fq -- "$lit"; }
# expect NAME SECS PATTERN HC-ARGS...: PASS once the output matches within SECS.
expect() {
  local n=$1 secs=$2 pat=$3; shift 3
  if poll "$secs" hc_has "$pat" "$@"; then pass "$n"; else fail "$n" "no match for /$pat/ in: $(tailc "$("$HC" "$@" 2>&1)")"; fi
}

cleanup() {
  "$HC" server stop >/dev/null 2>&1
  [ -n "${rt_pid:-}" ] && "$HRT" --session e2e server stop >/dev/null 2>&1
  [ -n "${app_pid:-}" ] && kill "$app_pid" >/dev/null 2>&1
  true
}
trap cleanup EXIT

echo "== h0x-core"
v=$("$HC" --version 2>&1)
case $v in
  "h0x-core "[0-9]*.[0-9]*.[0-9]*) pass "core --version ($v)" ;;
  *) fail "core --version" "$v" ;;
esac
ok "core --help" bash -c "\"$HC\" --help | grep -q 'run programs and coding agents'"

# --- server lifecycle in the isolated session
ok "server start" "$HC" server start
expect "server status shows running" 15 'running|"pid"|"version"' server status
ok "session list" "$HC" ls

# --- terminals: run --bg, ls, send, read, keys, wait, kill
ok "run --bg interactive shell" "$HC" run --bg --name sh1 -- "$bashx" --noprofile --norc -i
expect "ls lists sh1" 10 '"name": *"sh1"' ls
sleep 1
ok "send" "$HC" send sh1 'echo $((40+2))'
expect "read shows the sent command's output" 15 '^[[:space:]]*42[[:space:]]*$' read sh1
ok "keys (typed one by one)" "$HC" keys sh1 e c h o space k 7 enter
expect "read shows the keys' output" 15 '^[[:space:]]*k7[[:space:]]*$' read sh1
ok "run --bg slow program" "$HC" run --bg --name slow -- "$bashx" "$scripts/slow.sh"
w=$("$HC" wait slow --output ready-x --timeout 30 2>&1)
case $w in *'"reason": "output"'* | *'"reason":"output"'*) pass "wait --output returns at the output" ;; *) fail "wait --output" "$(tailc "$w")" ;; esac
ok "kill" "$HC" kill slow
if [ -n "${E2E_SANDBOX:-}" ]; then
  # Finding: OpenShell's seccomp filter answers EPERM to kill(-pgid), which is how the core ends a
  # terminal (killpg); a plain kill(pid) works. So `kill` has no effect in the sandbox: WARN, then
  # end the program the plain way so the rest of the run can go on.
  if poll 20 hc_has '"state": *"finished"' get slow; then pass "killed terminal is finished";
  else
    warn "killed terminal is finished" "h0x-core kill (killpg) is denied inside the OpenShell sandbox (EPERM); plain kill works"
    kill "$(ps -eo pid,args | awk '$2=="sleep"{print $1; exit}')" 2>/dev/null
  fi
else
  expect "killed terminal is finished" 20 '"state": *"finished"' get slow
fi
if poll 15 "$HC" rm slow; then pass "rm"; else fail "rm" "$(tailc "$("$HC" rm slow 2>&1)")"; fi
ok "kill shell" "$HC" kill sh1
x=$("$HC" exec "$bashx" -c 'echo exec-out; exit 7' 2>&1); rc=$?
if [ "$rc" = 7 ] && printf '%s' "$x" | grep -q exec-out; then pass "exec prints output and returns the exit code"; else fail "exec" "rc=$rc $(tailc "$x")"; fi
"$HC" get no-such-terminal >/dev/null 2>&1; rc=$?
if [ "$rc" = 3 ]; then pass "unknown terminal exits 3 (not_found)"; else fail "unknown terminal exit code" "rc=$rc, want 3"; fi
"$HC" bogus-subcommand >/dev/null 2>&1; rc=$?
if [ "$rc" = 2 ]; then pass "unknown subcommand exits 2 (usage)"; else fail "unknown subcommand exit code" "rc=$rc, want 2"; fi

# --- OSC 7501 status: working -> blocked kind=question -> records -> done
echo "== OSC 7501 status"
ok "run --bg osc reporter" "$HC" run --bg --name osc -- "$bashx" "$scripts/osc.sh"
expect "status working" 15 '"state": *"working"' get osc
expect "status msg decoded" 5 '"msg": *"building"' get osc
expect "status blocked, kind question" 15 '"kind": *"question"' get osc
expect "blocked state" 5 '"state": *"blocked"' get osc
expect "title with bidi/zero-width controls stripped" 20 '"title": *"abcd"' get osc
if hc_has_f "$(printf 'fam \xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x91\xa7')" get osc; then
  pass "ZWJ emoji joiners kept in a title"
else fail "ZWJ emoji joiners kept in a title" "family emoji bytes not found"; fi
if hc_has_f "$(printf '\xe2\x80\xae')" get osc || hc_has_f "$(printf '\xe2\x80\x8b')" get osc; then
  fail "no bidi override or ZWSP survives" "a control character is still in the record"
else pass "no bidi override or ZWSP survives"; fi
if hc_has 'w/bel' get osc; then fail "report with a BEL in its title is dropped" "record w/bel exists"; else pass "report with a BEL in its title is dropped"; fi
expect "status done" 25 '"state": *"done"' get osc
expect "done message" 5 '"msg": *"all done"' get osc
expect "the reporter has finished" 20 '"state": *"finished"' get osc
w=$("$HC" wait osc --timeout 20 2>&1)
case $w in *'"reason": "exit"'* | *'"reason":"exit"'*) pass "wait returns reason exit once the program ended" ;; *) fail "wait after exit" "$(tailc "$w")" ;; esac
ls_all=$("$HC" ls --all 2>&1)
if printf '%s' "$ls_all" | grep -Eq '"status": *"working"'; then fail "working is gone after done" "ls --all still shows working"; else pass "working is gone after done"; fi
if printf '%s' "$ls_all" | grep -Eq '"status": *"done"'; then pass "ls --all shows status done"; else fail "ls --all shows status done" "$(tailc "$ls_all")"; fi

# --- draft hold through a real attach (needs a pty: Linux and macOS)
echo "== draft hold"
if [ "$os" = windows ]; then
  skip "draft hold through a real attach" "no pty driver on Windows runners; unit-tested in the core"
elif ! command -v python3 >/dev/null 2>&1; then
  skip "draft hold through a real attach" "python3 not found"
else
  "$HC" run --bg --name d1 -- cat >/dev/null 2>&1
  sleep 1
  out=$(python3 "$here/draft.py" "$HC" d1 2>&1)
  hold=$(printf '%s' "$out" | sed -n 's/^hold=\([0-9]*\).*/\1/p')
  if [ -n "$hold" ] && [ "$hold" -ge 2 ] && [ "$hold" -le 10 ]; then pass "send is held while a person's draft is fresh (${hold}s), then lands"; else fail "draft hold" "$(tailc "$out")"; fi
  expect "the held note landed after the draft" 10 'helloNOTE' read d1
  "$HC" kill d1 >/dev/null 2>&1
fi

# --- keep_awake: toggled in config.toml, shown by `server status` while working
echo "== keep_awake"
cfg=$XDG_CONFIG_HOME/h0x-core/config.toml
mkdir -p "$(dirname "$cfg")"
awake() { "$HC" server status 2>&1 | grep -Eq '"keep_awake": *true'; }
released() { ! awake; }
if [ -n "${E2E_SANDBOX:-}" ] || { [ "$os" = linux ] && ! command -v systemd-inhibit >/dev/null 2>&1; }; then
  skip "keep_awake" "no systemd-inhibit here"
else
  printf 'keep_awake = true\n' > "$cfg"
  "$HC" run --bg --name kw -- "$bashx" "$scripts/work.sh" 20 >/dev/null 2>&1
  if poll 15 awake; then pass "keep_awake on: server status shows keep_awake while working"; else fail "keep_awake on" "$(tailc "$("$HC" server status 2>&1)")"; fi
  printf 'keep_awake = false\n' > "$cfg"
  if poll 15 released; then pass "keep_awake off again: the block is released"; else fail "keep_awake off" "still held after the setting was turned off"; fi
  "$HC" kill kw >/dev/null 2>&1
  rm -f "$cfg"
fi

# --- a closed pipe is a quiet exit 141, not a panic
# head exits at once; the core writes a second later, into a closed pipe.
{ sleep 1; "$HC" --skill; } | head -c 0 >/dev/null; rc=${PIPESTATUS[0]}
if [ "$rc" = 141 ]; then pass "broken pipe exits 141"; elif [ "$os" = windows ]; then warn "broken pipe exit code" "rc=$rc under Git Bash (a pipe there may accept the write); the exit is unit-tested"; else fail "broken pipe exit code" "rc=$rc, want 141"; fi

out=$("$HC" server stop 2>&1)
case $out in *'"stopped": true'* | *'"stopped":true'*) pass "server stop" ;; *) fail "server stop" "$(tailc "$out")" ;; esac

# --- the runtime (h0x)
echo "== h0x (runtime)"
v=$("$HRT" --version 2>&1)
case $v in "h0x "[0-9]*) pass "runtime --version ($v)" ;; *) fail "runtime --version" "$v" ;; esac
ok "runtime --help" bash -c "\"$HRT\" --help | grep -q 'Usage: h0x'"
out=$("$HRT" config check 2>&1); case $out in *"config: ok"*) pass "runtime config check" ;; *) fail "runtime config check" "$out" ;; esac
out=$("$HRT" --session e2e status 2>&1); case $out in *"not running"*) pass "runtime status: not running yet" ;; *) fail "runtime status before start" "$(tailc "$out")" ;; esac
ok "runtime session list" "$HRT" session list
out=$("$HRT" machine list 2>&1); case $out in *"No saved SSH machines"*) pass "runtime machine list (none)" ;; *) fail "runtime machine list" "$(tailc "$out")" ;; esac
ok "runtime --default-config" bash -c "\"$HRT\" --default-config | grep -q '# h0x configuration'"
"$HRT" bogus >/dev/null 2>&1; rc=$?
if [ "$rc" = 2 ]; then pass "runtime unknown command exits 2"; else fail "runtime unknown command" "rc=$rc, want 2"; fi
"$HRT" server --session e2e >"$E2E_ROOT/rt-server.log" 2>&1 &
rt_pid=$!
if poll 30 bash -c "\"$HRT\" --session e2e status 2>&1 | grep -q 'status: running'"; then pass "runtime headless server runs"; else fail "runtime headless server" "$(tailc "$(cat "$E2E_ROOT/rt-server.log")")"; fi
out=$("$HRT" --session e2e workspace list 2>&1); case $out in *workspace_list*) pass "runtime workspace list over the socket API" ;; *) fail "runtime workspace list" "$(tailc "$out")" ;; esac
out=$("$HRT" --session e2e tab list 2>&1); case $out in *tab_list*) pass "runtime tab list over the socket API" ;; *) fail "runtime tab list" "$(tailc "$out")" ;; esac
"$HRT" --session e2e server stop >/dev/null 2>&1; rc=$?
if [ "$rc" = 0 ]; then pass "runtime server stop"; else fail "runtime server stop" "rc=$rc"; fi
sleep 1
"$HRT" --session e2e server stop >/dev/null 2>&1; rc=$?
if [ "$rc" = 1 ]; then pass "runtime second stop exits 1 (not running)"; else fail "runtime second stop" "rc=$rc, want 1"; fi
rt_pid=

# --- the app (h0x-ade)
echo "== h0x-ade (app)"
v=$("$HX" --version 2>&1)
case $v in "h0x-ade "[0-9]*) pass "app --version ($v)" ;; *) fail "app --version" "$v" ;; esac
ok "app --help" bash -c "\"$HX\" --help | grep -q 'the HyperOx desktop app'"
out=$("$HX" commands 2>&1); case $out in "["*) pass "app commands lists JSON" ;; *) fail "app commands" "$(tailc "$out")" ;; esac
if [ -n "${E2E_SANDBOX:-}" ]; then
  skip "app headless launch" "no display inside the sandbox"
else
  # Headless: Linux under xvfb; Windows and macOS runners may refuse a window, so a failure there is a WARN.
  if [ "$os" = linux ]; then
    if command -v xvfb-run >/dev/null 2>&1; then xvfb-run -a "$HX" >"$E2E_ROOT/app.log" 2>&1 & app_pid=$!; else app_pid=; fi
  else
    "$HX" >"$E2E_ROOT/app.log" 2>&1 & app_pid=$!
  fi
  if [ -z "$app_pid" ]; then
    skip "app headless launch" "xvfb-run not installed"
  else
    sleep 15
    if kill -0 "$app_pid" 2>/dev/null && "$HX" status >"$E2E_ROOT/app-status.log" 2>&1; then
      pass "app launches headless and answers status"
      if grep -Eq '"errors": *\[\]' "$E2E_ROOT/app-status.log"; then pass "app status reports no errors"; else warn "app status errors" "$(tailc "$(cat "$E2E_ROOT/app-status.log")")"; fi
    elif [ "$os" = linux ]; then
      fail "app launches headless" "$(tailc "$(cat "$E2E_ROOT/app.log" "$E2E_ROOT/app-status.log" 2>/dev/null)")"
    else
      warn "app headless launch" "not available on this runner: $(tailc "$(cat "$E2E_ROOT/app.log" "$E2E_ROOT/app-status.log" 2>/dev/null)")"
    fi
    kill "$app_pid" >/dev/null 2>&1
    [ "$os" = windows ] && taskkill //F //IM h0x-ade.exe >/dev/null 2>&1
    [ "$os" != windows ] && pkill -f "$HX" >/dev/null 2>&1
    app_pid=
  fi
fi

# --- sandbox policy (only inside an OpenShell sandbox)
if [ -n "${E2E_SANDBOX:-}" ]; then
  echo "== sandbox policy"
  if touch /etc/h0x-e2e-outside 2>/dev/null; then fail "write outside the workspace is denied" "wrote /etc/h0x-e2e-outside"; else pass "write outside the workspace is denied"; fi
  if curl -fsS -m 15 -o /dev/null https://example.com 2>/dev/null; then fail "network to a non-allowed host is denied" "reached example.com"; else pass "network to a non-allowed host is denied"; fi
  if curl -fsS -m 30 -o /dev/null https://github.com 2>/dev/null; then pass "network to github.com is allowed"; else fail "network to github.com is allowed" "cannot reach github.com"; fi
fi

echo "SUMMARY: $passes passed, $fails failed"
[ "$fails" = 0 ]
