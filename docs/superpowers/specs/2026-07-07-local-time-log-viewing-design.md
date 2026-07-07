# Local-time log viewing

## Problem

Arca's tracing subscriber timestamps every log line in UTC (e.g.
`2026-07-07T14:23:01.123456Z`). Inside a container there's no local
timezone concept, and that's correct — raw log records should stay
UTC so they correlate cleanly across nodes and with any downstream
JSON/log-aggregation consumption. But when Pietro reads logs
interactively via `bin/arca logs`, `bin/console logs`, or
`bin/cluster logs`, he wants to see local wall-clock time, not UTC.

## Scope

Display-only. No change to what gets written into the log record
(text or JSON), no change to the Rust binary, no change to any Docker
image or compose file. Only the three `logs` subcommands' terminal
output changes.

## Design

Add one filter function, `localize_log_timestamps`, to
`bin/lib/compose.sh` (the shared library already sourced by
`bin/arca`, `bin/console`, and `bin/cluster`):

- Reads stdin line by line (works under `-f`/follow streaming, not
  just batch output).
- For each line, finds the first UTC ISO-8601 timestamp matching
  `YYYY-MM-DDTHH:MM:SS(.fraction)?Z` anywhere in the line (Arca's
  tracing-subscriber default text format) and replaces it with the
  equivalent local time formatted as `YYYY-MM-DD HH:MM:SS`.
- Lines with no match (e.g. the console's nginx access/error log
  lines) pass through unchanged, byte for byte.
- If a matched timestamp fails to convert for any reason, that
  occurrence is left as the original UTC string rather than
  corrupting the line.

A private helper does the actual UTC → local conversion for a single
timestamp string, branching once (cached in a variable) on whether
`date` is GNU coreutils or BSD date, since the two need different
invocations:

- GNU (Linux): `date -d "<ts>Z" '+%Y-%m-%d %H:%M:%S'` — one call,
  GNU's `-d` understands the `Z` suffix as UTC and converts to the
  system's local zone automatically.
- BSD (macOS): two calls — parse the literal wall-clock into an epoch
  under a forced `TZ=UTC` environment (`date -j -f ...`), then format
  that epoch back out under the ambient (local) zone.

Each of the three `cmd_logs` functions changes its final line from
```bash
$COMPOSE logs "${args[@]+"${args[@]}"}" <service>
```
to
```bash
$COMPOSE logs "${args[@]+"${args[@]}"}" <service> | localize_log_timestamps
```

## Testing

Manual: start Arca, run `bin/arca logs -f`, confirm printed
timestamps show local wall-clock time instead of a UTC `Z`-suffixed
stamp, and confirm a non-timestamped line (e.g. a multi-line startup
banner or a console nginx line) is unaffected. No automated test
harness exists for `bin/` shell scripts in this repo; this stays
manually verified, consistent with the rest of `bin/`.

## Out of scope / explicitly rejected

- Changing the Rust tracing subscriber to emit local time directly —
  rejected because the production image is `FROM scratch` (no tzdata,
  no `/etc/localtime`), and `time`/`chrono`'s local-offset detection
  has known soundness caveats in a multi-threaded Tokio process. Also
  would make raw/JSON logs lose their UTC anchor, which is the wrong
  default for anything machine-consumed.
- Mounting the host's `/etc/localtime` into containers — unnecessary
  once the conversion happens in the viewer instead of the app.
