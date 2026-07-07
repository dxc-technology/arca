# Local-time log viewing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `bin/arca logs`, `bin/console logs`, and `bin/cluster logs` display local wall-clock time instead of the raw UTC timestamps Arca's tracing subscriber writes, without changing what's actually stored/emitted in the log record.

**Architecture:** One shared stdin→stdout filter function (`localize_log_timestamps`) added to `bin/lib/compose.sh`, piped onto the tail end of each script's existing `docker compose logs ...` command. The filter finds a UTC ISO-8601 timestamp anywhere in each line and rewrites it to local time; lines without one pass through unchanged. A small helper (`_utc_to_local_timestamp`) does the actual single-timestamp conversion, branching once on GNU vs BSD `date` semantics.

**Tech Stack:** Bash (existing `bin/` scripts), POSIX/GNU/BSD `date`.

## Global Constraints

- No changes to Rust code, Cargo dependencies, Docker images, or compose files (spec: "Scope" section).
- The stored/emitted log record stays UTC — this is a display-only conversion in the viewer (spec: "Problem" and "Scope").
- Must work under both GNU coreutils `date` (Linux) and BSD `date` (macOS) (spec: "Design").
- Must work under `-f`/follow streaming, not just batch output (spec: "Design").
- Non-matching lines (e.g. console's nginx log lines) must pass through byte-for-byte unchanged (spec: "Design").
- If a matched timestamp fails to convert, leave that occurrence as the original UTC string rather than corrupting the line (spec: "Design").
- No automated test harness exists for `bin/` shell scripts in this repo; verification is manual, run directly via shell commands (spec: "Testing").

---

### Task 1: UTC→local single-timestamp conversion helper

**Files:**
- Modify: `bin/lib/compose.sh` (append a new `# --- Log viewing ---` section at the end of the file, after `wait_for_grpc_receiver`)

**Interfaces:**
- Consumes: nothing (leaf helper)
- Produces: `_utc_to_local_timestamp(ts)` — takes one UTC ISO-8601 timestamp string (`YYYY-MM-DDTHH:MM:SS(.fraction)?Z`), prints local wall-clock time as `YYYY-MM-DD HH:MM:SS` to stdout. On parse failure, prints the original `ts` unchanged. Also produces the module-level flag `_DATE_IS_GNU` (`true`/`false`), read-only after being set once at source time.

- [ ] **Step 1: Add the date-flavor detection and helper function**

Append to the end of `bin/lib/compose.sh`:

```bash

# --- Log viewing ---

# Detect once whether `date` is GNU coreutils (Linux) or BSD date (macOS).
# The two need different invocations to convert a UTC timestamp to local
# time, and this only needs to run once per script invocation.
if date --version >/dev/null 2>&1; then
    _DATE_IS_GNU=true
else
    _DATE_IS_GNU=false
fi

# Converts a single UTC ISO-8601 timestamp (e.g. "2026-07-07T14:23:01.123456Z"
# or "2026-07-07T14:23:01Z") to local wall-clock time formatted as
# "YYYY-MM-DD HH:MM:SS". Falls back to printing the input unchanged if
# parsing fails for any reason.
_utc_to_local_timestamp() {
    local ts="$1"
    local base="${ts%.*}"   # drop fractional seconds, if any
    base="${base%Z}"        # drop trailing Z, if still present (no fractional part)

    if $_DATE_IS_GNU; then
        date -d "${base}Z" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || printf '%s' "$ts"
    else
        local epoch
        epoch="$(TZ=UTC date -j -f '%Y-%m-%dT%H:%M:%S' "$base" '+%s' 2>/dev/null)" || { printf '%s' "$ts"; return; }
        date -j -f '%s' "$epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || printf '%s' "$ts"
    fi
}
```

- [ ] **Step 2: Verify the helper against an independently-computed expected value**

This test derives both the UTC input and the expected local output directly
from a fixed epoch using `date` primitives that are *not* the ones
`_utc_to_local_timestamp` uses internally (BSD's `-r` / GNU's `-d @`, with a
`||` fallback so the same one-liner works on either platform), so it's a
real check of the helper's UTC-string parsing path, not a circular one.

Run:

```bash
bash -c '
source bin/lib/compose.sh
epoch=1751894401
utc_input="$(date -u -r "$epoch" "+%Y-%m-%dT%H:%M:%SZ" 2>/dev/null || date -u -d "@$epoch" "+%Y-%m-%dT%H:%M:%SZ")"
expected="$(date -r "$epoch" "+%Y-%m-%d %H:%M:%S" 2>/dev/null || date -d "@$epoch" "+%Y-%m-%d %H:%M:%S")"
actual="$(_utc_to_local_timestamp "$utc_input")"
echo "input=$utc_input"
echo "expected=$expected"
echo "actual=$actual"
[[ "$actual" == "$expected" ]] && echo PASS || echo FAIL
'
```

Expected output: the last line is `PASS`, and `actual` equals `expected`.

- [ ] **Step 3: Verify the fractional-seconds variant also works**

Run:

```bash
bash -c '
source bin/lib/compose.sh
epoch=1751894401
utc_input="$(date -u -r "$epoch" "+%Y-%m-%dT%H:%M:%S.123456Z" 2>/dev/null || date -u -d "@$epoch" "+%Y-%m-%dT%H:%M:%S.123456Z")"
expected="$(date -r "$epoch" "+%Y-%m-%d %H:%M:%S" 2>/dev/null || date -d "@$epoch" "+%Y-%m-%d %H:%M:%S")"
actual="$(_utc_to_local_timestamp "$utc_input")"
[[ "$actual" == "$expected" ]] && echo PASS || echo FAIL
'
```

Expected output: `PASS`.

- [ ] **Step 4: Verify the failure fallback**

Run:

```bash
bash -c '
source bin/lib/compose.sh
actual="$(_utc_to_local_timestamp "not-a-timestamp")"
[[ "$actual" == "not-a-timestamp" ]] && echo PASS || echo FAIL
'
```

Expected output: `PASS`.

- [ ] **Step 5: Commit**

```bash
git add bin/lib/compose.sh
git commit -m "$(cat <<'EOF'
Add UTC-to-local timestamp conversion helper to compose.sh

Foundation for converting Arca's UTC log timestamps to local wall-clock
time in bin/arca|console/cluster logs, without touching the stored log
format. Handles both GNU and BSD date semantics.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Line-level log filter

**Files:**
- Modify: `bin/lib/compose.sh` (append immediately after `_utc_to_local_timestamp`, same `# --- Log viewing ---` section)

**Interfaces:**
- Consumes: `_utc_to_local_timestamp(ts)` from Task 1
- Produces: `localize_log_timestamps` — a filter with no arguments; reads lines from stdin, writes the converted lines to stdout, one per input line.

- [ ] **Step 1: Add the filter function**

Append to `bin/lib/compose.sh`, directly after `_utc_to_local_timestamp`'s closing `}`:

```bash

# Reads docker compose log lines from stdin and rewrites any UTC ISO-8601
# timestamp found in each line to local wall-clock time. Lines without a
# matching timestamp (e.g. nginx access/error log lines) pass through
# unchanged. Used by `bin/arca logs`, `bin/console logs`, `bin/cluster logs`.
localize_log_timestamps() {
    local ts_re='[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z'
    local line
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ $ts_re ]]; then
            local match="${BASH_REMATCH[0]}"
            local local_ts
            local_ts="$(_utc_to_local_timestamp "$match")"
            line="${line/$match/$local_ts}"
        fi
        printf '%s\n' "$line"
    done
}
```

- [ ] **Step 2: Verify a matching line gets converted and a non-matching line passes through unchanged**

```bash
bash -c '
source bin/lib/compose.sh
nginx_line="192.168.1.1 - - [07/Jul/2026:14:23:01 +0000] \"GET / HTTP/1.1\" 200"
arca_line="2026-07-07T14:23:01.123456Z  INFO arca_server: Starting Arca"
output="$(printf "%s\n%s\n" "$arca_line" "$nginx_line" | localize_log_timestamps)"
out_line1="$(printf "%s" "$output" | sed -n "1p")"
out_line2="$(printf "%s" "$output" | sed -n "2p")"

pass=true
[[ "$out_line2" == "$nginx_line" ]] || { echo "FAIL: nginx line changed: $out_line2"; pass=false; }
[[ "$out_line1" == *"Z"* ]] && { echo "FAIL: arca line still has a Z suffix: $out_line1"; pass=false; }
[[ "$out_line1" == *"INFO arca_server: Starting Arca" ]] || { echo "FAIL: arca line lost its message: $out_line1"; pass=false; }
$pass && echo PASS
'
```

Expected output: `PASS` (no `FAIL:` lines).

- [ ] **Step 3: Verify streaming (line-buffered) behavior**

This confirms the filter emits each converted line as soon as it arrives,
rather than buffering until EOF — required for `-f`/follow mode.

```bash
bash -c '
source bin/lib/compose.sh
mkfifo /tmp/localize_test_fifo 2>/dev/null || true
( localize_log_timestamps < /tmp/localize_test_fifo > /tmp/localize_test_out & echo $! > /tmp/localize_test_pid )
exec 3>/tmp/localize_test_fifo
echo "2026-07-07T14:23:01Z first line" >&3
sleep 0.3
first_seen="$(cat /tmp/localize_test_out 2>/dev/null || true)"
echo "2026-07-07T14:23:02Z second line" >&3
exec 3>&-
sleep 0.3
kill "$(cat /tmp/localize_test_pid)" 2>/dev/null || true
rm -f /tmp/localize_test_fifo /tmp/localize_test_pid
final="$(cat /tmp/localize_test_out)"
rm -f /tmp/localize_test_out
[[ -n "$first_seen" ]] && echo "PASS: got output before second line was written" || echo "FAIL: no output appeared until after the second write"
echo "--- final output ---"
echo "$final"
'
```

Expected output: `PASS: got output before second line was written`, followed by both converted lines (no `Z` suffix, times unchanged otherwise) under `--- final output ---`.

- [ ] **Step 4: Commit**

```bash
git add bin/lib/compose.sh
git commit -m "$(cat <<'EOF'
Add localize_log_timestamps line filter to compose.sh

Streaming stdin-to-stdout filter that rewrites UTC timestamps to local
time in each log line, passing through anything that doesn't match
(e.g. nginx log lines) unchanged. Not yet wired into any bin/ script.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Wire the filter into the three log viewers

**Files:**
- Modify: `bin/arca:220`
- Modify: `bin/console:117`
- Modify: `bin/cluster:30` (add `source`) and `bin/cluster:113`

**Interfaces:**
- Consumes: `localize_log_timestamps` from Task 2
- Produces: nothing further downstream (terminal integration point)

- [ ] **Step 1: Wire `bin/arca`**

In `bin/arca`, change (currently line 220):

```bash
    $COMPOSE logs "${args[@]+"${args[@]}"}" arca
```

to:

```bash
    $COMPOSE logs "${args[@]+"${args[@]}"}" arca | localize_log_timestamps
```

- [ ] **Step 2: Wire `bin/console`**

In `bin/console`, change (currently line 117):

```bash
    $COMPOSE logs "${args[@]+"${args[@]}"}" console
```

to:

```bash
    $COMPOSE logs "${args[@]+"${args[@]}"}" console | localize_log_timestamps
```

- [ ] **Step 3: Wire `bin/cluster`**

`bin/cluster` doesn't source `bin/lib/compose.sh` today (it's intentionally
independent — see the comment at the top of the file). Add the source line
so it can reuse `localize_log_timestamps`, then pipe through it.

In `bin/cluster`, change (currently line 30):

```bash
cd "$(git rev-parse --show-toplevel)"
```

to:

```bash
cd "$(git rev-parse --show-toplevel)"
source bin/lib/compose.sh
```

Then change (currently line 113):

```bash
    $COMPOSE logs "${args[@]+"${args[@]}"}"
```

to:

```bash
    $COMPOSE logs "${args[@]+"${args[@]}"}" | localize_log_timestamps
```

- [ ] **Step 4: Syntax-check all three scripts**

```bash
bash -n bin/arca && bash -n bin/console && bash -n bin/cluster && echo PASS
```

Expected output: `PASS` (no syntax errors printed).

- [ ] **Step 5: End-to-end verification against a running server**

```bash
bin/arca start -d --build --dev
sleep 3
bin/arca logs | tail -5
```

Expected output: the last few startup log lines (e.g. `Starting Arca`,
`Metadata backend ready`), each prefixed with a timestamp in
`YYYY-MM-DD HH:MM:SS` form — no `T` separator, no trailing `Z`, and matching
your local wall-clock time rather than UTC.

- [ ] **Step 6: Tear down**

```bash
bin/arca stop
```

- [ ] **Step 7: Commit**

```bash
git add bin/arca bin/console bin/cluster
git commit -m "$(cat <<'EOF'
Show local time in bin/arca|console|cluster logs

Pipes docker compose logs through the new localize_log_timestamps
filter so interactive log viewing shows local wall-clock time instead
of Arca's raw UTC timestamps. The stored/emitted log record is
untouched -- this only affects what these three commands print.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)"
```
