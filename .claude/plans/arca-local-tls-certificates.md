# Local TLS certificates: bring your own, or get working self-signed ones

Status: DONE 2026-09-29 — M1 9522e3c, M2 7ed6ea7, M3 1a27fef, M4 a1e5247; M5 (docs) folded into each milestone.

## Goal

Anyone can run Arca locally over HTTPS and run every test suite, with or
without certificates of their own:

- **With real certificates** (e.g. a Let's Encrypt wildcard for a personal
  domain) dropped in `certs/`: Arca uses them, locally and for
  `bin/test integration`.
- **Without**: the first `bin/arca start --tls` generates a local CA and a
  server certificate, and keeps them valid by itself.
- **Self-contained test suites** never depend on, write to or delete anything
  in `certs/`.

## Problems this fixes

1. `bin/test tls` writes `arca-ca.*` / `arca-server.*` into the user's
   `certs/` and deletes them afterwards: any user file with those names is
   destroyed.
2. `bin/arca start --tls` with an empty `certs/` just fails.
3. `bin/test integration` runs `compose run --rm test`, whose `depends_on`
   recreates the running `arca` container from the base compose config: the
   generated `config.toml` and the certificate mount are dropped, so a server
   started with `--tls --encryption` is silently replaced by a plain,
   unencrypted one before the tests run (pre-existing). Since b41db34 the
   recreated container also uses the stale `:production` image instead of the
   dev one (regression).

## Directory layout

```
certs/                     user certificates (never written by Arca tooling)
certs/local/               generated server material, mounted in containers
    arca-server.crt          server certificate (365 days)
    arca-server.key          server key (0640, group 65532)
    arca-ca.crt              local CA certificate, for clients to trust
certs/local-ca/            never mounted in any container
    arca-ca.crt              local CA certificate (10 years)
    arca-ca.key              local CA private key (0600)
```

Both auto-detects (Arca's `detect_pem_files`, the console entrypoint) scan one
directory without recursing, so `certs/local*/` never interferes with user
files in `certs/`. The CA key is kept out of the mounted directory because
Arca's auto-detect does not skip CA keys (two keys -> startup error) and
because no container needs it.

## Selection rule (every `bin/arca start --tls`, not a one-time flag)

| `certs/` contains                       | Result                                             |
|-----------------------------------------|----------------------------------------------------|
| a certificate + key of the user         | use `certs/` as is; `certs/local*` ignored         |
| nothing of the user's                   | `arca tls ensure` on `certs/local`, then use it    |

"Of the user's" = any `.pem/.crt/.cert/.key` file directly in `certs/`.
`arca tls ensure` (re)generates only what is needed:

- no local CA -> create it (10 years);
- CA expiring within 30 days -> new CA (the only case needing a new trust);
- no server certificate, expiring within 30 days, SANs changed, or not signed
  by the current CA -> new server certificate from the SAME CA (365 days).

So adding real certificates wins at the next start, deleting them falls back
to the generated ones (regenerated if missing), and the user trusts the local
CA once for ten years.

SANs: `localhost,127.0.0.1,::1,arca` plus `ARCA_TLS_SANS` (comma list) from
`docker/.env`.

## Milestones

### M1 - `arca tls ensure` (Rust, TDD)

New subcommand in `arca-server` (`tls_generate.rs` + `cli.rs`):

```
arca tls ensure --output-dir <dir> --ca-dir <dir> [--sans ...]
                [--days 365] [--ca-days 3650] [--renew-within-days 30]
```

Reuses the existing generator and file-mode helpers (`MODE_*`). Prints what it
did (`created CA`, `renewed server certificate: SANs changed`, `up to date`).
`arca tls generate` is unchanged (backward compatible).

Unit tests: fresh dirs; idempotent second run (files untouched); server
renewal on near expiry keeps the CA byte-identical; SAN change renews the
server only; CA near expiry renews both; server not signed by the current CA
(CA replaced by hand) renews the server; file modes; CA key never written to
the output dir.

### M2 - `bin/arca start --tls` uses it

- `bin/lib/compose.sh`: `resolve_tls_certs` applies the selection rule, runs
  `arca tls ensure` through the arca image when needed, and exports
  `ARCA_CERTS_DIR` (`../certs` or `../certs/local`), persisted in `.arca-env`
  so `bin/console` and `bin/test` see the same choice.
- `docker/docker-compose.tls.yml`: mount `${ARCA_CERTS_DIR:-../certs}` for
  arca and the console.
- Local mode uses the `tls-explicit` fragment (known file names); user mode
  keeps auto-detect.

### M3 - self-contained suites are hermetic

`bin/test tls` and `bin/perf-test --tls` generate an ephemeral CA + server
certificate in a named volume (as `tls-permissions` already does), trust that
CA explicitly (`AWS_CA_BUNDLE`), and never touch `certs/`. TLS verification
stays ON: the self-signed "warning" is handled by trusting the test CA, not by
disabling verification.

### M4 - `bin/test integration` tests the server as started

- Runs the test container with `--no-deps`: the running server is never
  recreated. Clear error if no server is running.
- Reads `.arca-env`: plain -> `http://arca:9000`; local TLS ->
  `https://arca:9000` with `certs/local/arca-ca.crt` as CA bundle; user TLS ->
  `https://$ARCA_TLS_HOSTNAME:9000`, the name mapped to the arca container
  (`--add-host`), verified against the system CAs. `ARCA_TLS_HOSTNAME` (in
  `docker/.env`) is required only for user certificates whose SANs are all
  wildcards; otherwise the first DNS SAN is used. Clear error if it cannot be
  determined.
- Fixes the b41db34 regression as a side effect (no recreation at all).

### M5 - documentation and bookkeeping

TLS guide (local CA, trusting it on macOS/Linux, bringing your own
certificates, `ARCA_TLS_SANS` / `ARCA_TLS_HOSTNAME`), Quick Start, AGENTS.md
(commands, TLS gotcha), `docker/.env.example`, CHANGELOG, README test counts,
`bin/docs-build`.

## Deviations from the plan

- Hostname for user certificates: instead of requiring `ARCA_TLS_HOSTNAME`
  for wildcard-only certificates, a wildcard is made concrete
  (`*.example.org` -> `arca.example.org`, which it always covers);
  `ARCA_TLS_HOSTNAME` is only an override.
- The `tls-init` compose service was removed rather than repointed: it was
  the only writer into `certs/` and nothing needs it any more.
- `bin/test images` became `bin/test scripts` (all `tests/scripts/*_test.sh`).

## Found along the way (not fixed here)

- Compression + encryption: reading a compressed object on an encrypted
  server fails with `compressed footer count truncated` / `read chunks:
  early eof` (500). Pre-existing (identical on 903582f); `bin/test
  compression` never enables encryption, so the combination was untested.
- `bin/test tls` (and other self-contained modes) run `down -v` on the same
  compose project as the developer's server, deleting its data volume.
- The encryption test files skip unless `ARCA_ENCRYPTION_ENABLED` is set, so
  `bin/test integration` against an encrypted server still skips them.

## Out of scope

Cluster (`arca tls generate-cluster`) certificates; production deployments
(Kubernetes manifests keep mounting user-provided Secrets).
