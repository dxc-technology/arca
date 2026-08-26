# Conditional-write atomicity (compare-and-swap) — analysis and fix plan

Status: **APPROVED FOR IMPLEMENTATION** (Pietro, 2026-08-26) — including the
§6 behaviour change and the §4 cluster decision. All open decisions are
resolved; the plan is executable end-to-end (M1→M5) with no further sign-offs
needed. See §10 for the execution run-book.
Review notes integrated 2026-08-26 (If-None-Match semantics, 412-vs-409 on
CMU, replication call-site comments, ha.md mitigation note).
Source report: `CONDITIONAL-WRITE-ATOMICITY.md` (untracked, from an external
integration attempt).

---

## 1. Verdict

The report is **correct**, and the defect is **wider and more severe** than it
describes. Three corrections to the report are noted in §5.

### 1.1 Reproduced

Against a freshly built dev server (`bin/arca start -d --build --dev`, SQLite
backend, no encryption), with a `threading.Barrier` release and a 1 MiB body:

| Case | Runs | Result | Expected |
|---|---|---|---|
| `PutObject` + `If-Match: <current ETag>`, N=5 | 5/5 | **all five writers got 200** | one 200, four 412 |
| `PutObject` + `If-None-Match: *`, N=5, key absent | 3/3 | **all five writers got 200** | one 200, four 412 |

Not "sometimes": every writer won in every run. The object ends as
last-writer-wins and four of the five clients are told their conditioned write
landed.

`If-None-Match: *` was **not** in the original report and is the more damaging
of the two. It is the create-if-absent primitive: distributed locks, leader
election, "claim this work item" markers. Today Arca hands the same lock to
every contender simultaneously.

### 1.2 Root cause

Exactly as the report states, and the file/line references hold:

- Check: `crates/arca-proto/src/handlers/object.rs:485-516` — reads the current
  record with `get_object` and evaluates `check_conditionals`.
- Commit: `crates/arca-proto/src/handlers/object.rs:688` —
  `state.metadata.put_object(&record)`, with no precondition attached.

Nothing between the two re-evaluates anything.

### 1.3 The window is not a race window

This is the part the report understates. The check at `object.rs:485` runs
**before the request body is read** (`request.into_body()` is at line ~558).
The gap between check and commit therefore spans:

1. the entire body upload from the client,
2. the blob write (plus compression / encryption / chunked AEAD),
3. the sidecar write.

For anything but a tiny object the gap is milliseconds to minutes, so the
probability that concurrent writers all pass is ~1, not "timing-dependent". A
1 MiB body was enough for a deterministic 5/5. This is not a narrow race that
needs a stress harness to hit — it is the normal behaviour of concurrent
conditional writes on objects of realistic size.

(The early position of the check is deliberate and useful — see §3.4 — it just
cannot be the authoritative one.)

---

## 2. Full scope: every conditional-write path

The same check-then-commit shape appears on five paths. Only the first two were
reproduced from a client; the rest are the same defect by inspection, with
narrower windows.

| # | Path | Check | Commit | Window | Verified |
|---|---|---|---|---|---|
| 1 | `PutObject` (`If-Match`, `If-None-Match`) | `object.rs:485-516` | `object.rs:688` | full body upload + blob + sidecar | **reproduced** |
| 2 | `CompleteMultipartUpload` (`If-Match`, `If-None-Match`) | `multipart.rs:272-288` | `multipart.rs:455` | XML parse + **full part assembly** (can be GB) | inspection, wide |
| 3 | `DeleteObject` (`If-Match`, `x-amz-if-match-last-modified-time`, `x-amz-if-match-size`) | `object.rs:2069-2115` | `object.rs:2118` | no body — sub-millisecond | inspection, narrow |
| 4 | `DeleteObject` on a specific version (same three headers) | `object.rs:2010-2040` | `object.rs:2043` | no body | inspection, narrow |
| 5 | `DeleteObjects` batch, per-key `ETag`/`LastModifiedTime`/`Size` | `bucket.rs:1940-2010` | same loop iteration | no body | inspection, narrow |

Paths 3-5 could not be made to lose the race from a client in the time
available (the metadata op is enqueued almost immediately after the check), but
the ordering is unguarded: a stale `If-Match` delete that loses the race
destroys an object whose ETag no longer matches, and returns `204`.

### 2.1 Explicitly NOT affected

**`CopyObject` / `UploadPartCopy` copy-source conditionals are fine.**
`check_copy_source_conditionals` (called at `object.rs:876`) evaluates against
`src_record`, and the copy then reads `src_record.blob_id`
(`object.rs:991`, and `object.rs:1323` for `UploadPartCopy`) — the exact blob
the condition was checked against. The copy
reflects a consistent snapshot of the source as of the check, which is what the
conditional asked for. The only residual is that a concurrently deleted source
blob surfaces as a 500 rather than a 404; that is a separate robustness nit, not
this defect, and it is out of scope here.

### 2.2 Impact

`README.md:62` and `README.md:67` advertise conditional requests and
`CompleteMultipartUpload` conditional headers as supported. AWS S3 evaluates
`If-Match` / `If-None-Match` atomically at write time (`If-None-Match` since
Aug 2024, `If-Match` since Nov 2024) and this is the documented basis for S3 as
a coordination primitive. Any client using it against Arca silently degrades to
last-writer-wins while being told otherwise. Nothing in the integration suite
covers conditional `PutObject` at all today — only GET / HEAD / copy variants
exist in `test_phase2.py::TestConditionalHeaders` — which is why this survived
to a third party.

---

## 3. The fix

The report's direction is right: move the authoritative evaluation into the
transaction that installs the new version. Arca happens to make this cheap on
both single-node backends, and impossible cluster-wide without new machinery —
see §4.

### 3.1 Why it is cheap on single node

**SQLite.** `SqliteStore` runs every mutation through a *single* write
connection (`crates/arca-storage/src/sqlite/mod.rs:8`, `conn` field at line 50),
with reads on a separate pool. All `put_object` / `delete_object` transactions
are therefore already totally ordered. Evaluating the precondition inside the
existing `self.conn.call(...)` closure is atomic by construction — **no new
locking**. Better: the unversioned branch already calls `fetch_latest_object`
(`sqlite/metadata.rs:167`), so for that branch the check needs no extra query.

**PostgreSQL.** `put_object` opens its transaction with
`next_object_seq` — `UPDATE object_seq SET value = value + 1 RETURNING value`
(`crates/arca-storage/src/pg/metadata.rs:96-103`, called at line 449 before any
row mutation). That takes a row lock on the single `object_seq` row and holds it
to commit, so **all `put_object` transactions on PG are already globally
serialized**. Placing the precondition check after `next_object_seq` and before
the versioning branch is atomic — again **no new locking**.

One caveat on the PG delete path: `delete_object` reads
`fetch_latest_object` at `pg/metadata.rs:666`, *before* taking
`next_object_seq` at line 679 (and only when a row exists). For
`delete_object_if` the seq must be taken first, unconditionally, so the read
happens under the lock. That burns a seq value on a refused or no-op delete —
a harmless gap the codebase already accepts elsewhere
(`pg/metadata.rs:1146`: "a re-delivery no-op burns the value (harmless gap)").

Net: no per-key lock table, no new serialization primitive, no schema change.

### 3.2 New shared types (`arca-core`)

```rust
// crates/arca-core/src/store/metadata.rs

/// Compare-and-swap preconditions for a write, evaluated inside the same
/// transaction that installs the new version (S3 conditional writes).
#[derive(Debug, Clone, Default)]
pub struct WritePrecondition {
    /// `If-Match`: proceed only if the current latest object exists and its
    /// ETag matches one of the listed values. `*` matches any existing object.
    pub if_match: Option<String>,
    /// `If-None-Match`: proceed only if no current object matches. `*` means
    /// "only if the object does not exist".
    pub if_none_match: Option<String>,
}

/// Preconditions for a conditional delete (S3 adds size and mtime to ETag).
#[derive(Debug, Clone, Default)]
pub struct DeletePrecondition {
    pub if_match: Option<String>,
    pub if_match_last_modified: Option<chrono::DateTime<chrono::Utc>>,
    pub if_match_size: Option<u64>,
}
```

Both get `is_empty()` and a single `evaluate(&self, current: Option<&ObjectRecord>)
-> Result<(), S3ErrorCode>` so the handler's early check and the storage layer's
authoritative check cannot drift apart.

**Prerequisite: move `etag_matches` into `arca-core`.** It currently lives as
`pub(super) fn etag_matches` in `crates/arca-proto/src/handlers/object.rs:355`,
invisible to `arca-storage`. Move it to `arca_core::s3::etag` and have
`arca-proto` call it there. Two independent ETag matchers on the two sides of
the same contract is exactly how this kind of fix rots.

### 3.3 Trait shape (`MetadataStore`)

Add conditional variants and make the existing methods thin delegates, so there
is exactly one implementation of each write per backend:

```rust
/// Same as [`put_object`], but applies the write only if `pre` holds against
/// the current latest record, checked inside the transaction that installs
/// the new version. On a mismatch nothing is written and the error is
/// `ArcaError::S3(PreconditionFailed)` — or `NoSuchKey` when `if_match` was
/// given and the object does not exist.
async fn put_object_if(
    &self,
    record: &ObjectRecord,
    pre: &WritePrecondition,
) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError>;

async fn put_object(
    &self,
    record: &ObjectRecord,
) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError> {
    self.put_object_if(record, &WritePrecondition::default()).await
}

async fn delete_object_if(
    &self,
    bucket: &str,
    key: &str,
    pre: &DeletePrecondition,
) -> Result<Option<ObjectRecord>, ArcaError>;

async fn delete_object(&self, bucket: &str, key: &str)
    -> Result<Option<ObjectRecord>, ArcaError> {
    self.delete_object_if(bucket, key, &DeletePrecondition::default()).await
}
```

`delete_object_version_if` likewise for path 4.

This leaves the ~25 unconditional call sites (`recover.rs:134`, `fsck.rs`,
`migrate_topology.rs:361`, replication apply, cluster anti-entropy, unit tests)
completely untouched, and the default bodies guarantee the conditional and
unconditional paths cannot diverge.

`ArcaError::S3(S3Error)` already exists (`arca-core/src/error.rs:13`), and the
storage layer knows bucket and key, so it can build the `/{bucket}/{key}`
resource itself. No new error variant needed.

### 3.4 Keep the early check as an optimisation

**Do not** follow the report's step 1 literally ("the handler passes the raw
precondition set down instead of deciding"). Removing the early check would
make every losing writer upload its whole body before learning it lost, and
would lose the `Connection: close` early-reject behaviour that was added
deliberately (changelog: intermittent 400 on a keep-alive connection reused
after a rejected conditional PUT).

Keep both:

- **Early check** (`object.rs:485`): unchanged, a cheap reject that avoids the
  upload in the non-racing case. Documented in a comment as an *optimisation
  only, never the decision point*.
- **Authoritative check**: inside the metadata transaction, via
  `put_object_if`. Its verdict is the response.

### 3.5 Handler changes

**`put_object` (`object.rs`).** Build the `WritePrecondition` from the same
headers, call `put_object_if`, and on `PreconditionFailed` / `NoSuchKey`:

1. delete the blob just written (`state.blob.delete(&blob_id)` — removes file
   and sidecar, `fs/blob.rs:453`),
2. return the 412 / 404 **without** `close_after` (the body was fully read, so
   the connection is clean).

Losing writers now pay for a full upload plus a discarded blob. That is
inherent to CAS-at-commit and is what AWS does; §3.4's early check keeps the
non-racing cost at zero.

**`If-None-Match` with a specific (non-`*`) ETag.** AWS documents only
`If-None-Match: *` for `PutObject`. Arca's existing early check
(`check_conditionals`) already accepts specific ETags with standard HTTP
semantics (refuse if any listed ETag matches the current object); the
authoritative check keeps the same behaviour. Restricting to `*` now would be
a gratuitous incompatibility with what Arca already accepts. Pinned by a
dedicated test in §7.1.

**`complete_multipart_upload` (`multipart.rs`).** Same, with one trap:
`BlobStore::delete` on a **composite** blob walks the sidecar's part list and
deletes the part blobs too (`fs/blob.rs:453-470`). On a refused CMU the parts
must survive so the client can retry, so the cleanup must use the
non-composite-aware `delete_blob_file_raw` (`fs/blob.rs:177`) on the assembled
blob only, and must not touch the upload record
(`delete_multipart_upload` at `multipart.rs:489` runs after the commit, so this
is naturally satisfied by returning early).

**412 vs 409 on CMU.** AWS can answer a conditional `CompleteMultipartUpload`
with `409 ConditionalRequestConflict` when the conditional write conflicts
with another operation concurrently in flight, in addition to
`412 PreconditionFailed` for a plainly failed condition. Arca evaluates the
CAS once, at commit time, and maps **every refused precondition to `412`** — a
deliberate simplification (there is no separate "conflict in flight" state to
report). Stated here explicitly so it does not resurface later as a
"compatibility bug" from the Ceph s3-tests.

**Delete paths (`object.rs`, `bucket.rs`).** Pass a `DeletePrecondition` down to
`delete_object_if` / `delete_object_version_if`. For the batch, map
`Err(PreconditionFailed)` to the per-key `DeleteErrorEntry` that
`bucket.rs:1991-1996` already builds, instead of pre-filtering the key.

### 3.6 Decorator changes

- **`CachingMetadataStore`** (`caching.rs:107`): forward `*_if`; invalidate only
  on `Ok`. Note in a comment that a refused write changes nothing, so the
  cached `get_latest_object` entry stays valid. (Cache coherence is fine today:
  every write path, including `apply_remote_object` at `caching.rs:216`,
  invalidates, and `inner_metadata` in `main.rs:496` is the caching-wrapped
  handle, so remote applies invalidate too.)
- **`ClusterMetadataStore`** (`cluster_meta.rs:340`): forward to inner; on `Err`
  return before `fan_out_object`, so a refused write is never replicated. See
  §4 for what this does and does not guarantee.
- **Replication apply and anti-entropy stay unconditional.** `apply_remote_object`
  and the anti-entropy repair path keep calling the plain `put_object` — correct
  by design: a remote LWW write must never be blocked by a local precondition,
  or the cluster stops converging. Add a short comment at those call sites
  saying exactly this, so a future refactoring does not "fix" them into the
  conditional path.

---

## 4. The cluster: what we can and cannot promise

The report's step 4 says the serialization domain "must cover the key
cluster-wide, since a per-process lock does not constrain writes that land on
different nodes. The natural domain is the key's authoritative metadata
transaction."

**That domain does not exist in Arca.** Phase 29's HA design is a symmetric,
self-configuring, fully-replicated multi-master cluster with last-writer-wins
convergence. There is no authoritative node per key: `put_object` commits
locally, then fans out (`cluster_meta.rs:345-355`), and the quorum check
(`enforce_ack_quorum`) is about *durability* — how many nodes hold the write —
not about ordering. Two conditional PUTs for the same key arriving at two nodes
both pass their local CAS, both commit, and LWW picks a winner. Moving the check
into a transaction cannot fix that.

Three options, in increasing cost:

**(A) Node-local CAS + honest documentation — recommended for this fix.**
The CAS is exact within one node, which covers every single-node deployment
(the overwhelmingly common case) and covers a cluster whenever all conditional
writes for a key reach the same node — which is what a sticky load balancer or
a single-endpoint client already produces. Document the limit precisely in
`ha.md`, the README compatibility notes, and a new tech-debt entry — including
the practical mitigation, which is what a real operator needs: *with a per-key
sticky load balancer, or a client pointed at a single endpoint, the CAS is
effectively exact cluster-wide*. Add a
cluster integration test that *pins* the limit (both nodes return 200) so it is
a known, tested boundary rather than an accident.

**(B) Owner-node forwarding for conditional writes.** Deterministically map the
key to an owner among the live members (rendezvous hash) and forward conditional
writes there, so the owner's local CAS is cluster-wide. Arca already has
membership, endpoints and a peer client. The cost is a streaming body-forwarding
proxy path plus a correctness story for membership changes mid-write (the owner
set shifts; two nodes can briefly both believe they own the key). Real work, and
a genuine follow-up phase — not part of this fix.

**(C) Consensus (Raft) per key or per bucket.** Out of scope for Arca's stated
architecture.

**Decision (Pietro, 2026-08-26): (A) now, (B) tracked as TD-025.** Silently
claiming cluster-wide CAS would be a worse outcome than the current bug.

---

## 5. Corrections to the report

1. **`If-None-Match: *` is missing from it**, and it is the more dangerous case
   (create-if-absent / distributed lock). Verified broken 3/3.
2. **The cluster fix as described is not implementable** — there is no
   per-key authoritative metadata transaction in Arca's HA design (§4).
3. **Suggested ID `TD-024` is taken** by the `rustls-webpki` / `rumqttc` entry.
   Use **TD-025**.
4. Minor: the report's step 1 (handler stops deciding) should be step
   "handler *also* decides, cheaply" — see §3.4.
5. Minor: `CopyObject` copy-source conditionals are listed in the report's root
   cause as affected ("the PUT and copy paths"). They are not (§2.1).

---

## 6. Behaviour change — APPROVED

After the fix, a concurrent conditional write that today receives `200` will
receive `412`. That is the whole point, and it makes a broken guarantee real
rather than breaking a working one — but it *is* a visible change in observed
responses under concurrency, so it needed explicit sign-off (per the
backward-incompatibility rule). No API, config, schema or on-disk format
changes.

**Approved by Pietro on 2026-08-26.** Rationale on record: this is the repair
of an already-published promise (README advertises conditional requests), the
current `200` is a false answer to a client that asked for exactly this `412`,
and no correct client can depend on the current behaviour.

---

## 7. Tests

### 7.1 Unit — `arca-core`

- `WritePrecondition::evaluate` / `DeletePrecondition::evaluate`: `*`, quoted
  and unquoted ETags, comma-separated lists, absent object, delete-marker as
  latest.
- `if_none_match` with a specific (non-`*`) ETag: refused when it matches the
  current object, allowed otherwise — pins the HTTP-semantics behaviour kept
  in §3.5.
- `etag_matches` moves with its existing tests.

### 7.2 Unit — `arca-storage`, both backends

For SQLite and PostgreSQL, on unversioned / versioned / suspended buckets:

- CAS hit commits; CAS miss returns `PreconditionFailed` and **writes nothing**
  (assert row count, ETag and — for a versioned bucket — that **no new version
  was created**, the report's step 5).
- `if_match` on an absent object → `NoSuchKey`.
- `if_none_match: "*"` on an existing object → `PreconditionFailed`; on an
  absent one → commits.
- `delete_object_if`: ETag / size / mtime mismatch each refuse, and the row
  survives.
- PG-specific: refused write releases the `object_seq` lock and leaves a gap,
  not a stuck transaction.

### 7.3 Integration — new `tests/integration/test_conditional_writes.py`

The regression test the report asks for, plus the sequential coverage that is
missing entirely today:

- **Sequential** (currently untested): PUT with matching / stale `If-Match`,
  `If-None-Match: *` on present and absent keys, CMU with both headers.
- **Concurrent `If-Match`**: N=8 writers from the same base ETag, released on a
  barrier, 1 MiB bodies, repeated 10x → exactly one 200, seven 412, final
  content equals the winner's payload.
- **Concurrent `If-None-Match: *`**: N=8 on an absent key, repeated 10x →
  exactly one 200.
- **Concurrent CMU `If-Match`**: two uploads completing against the same base
  ETag → one 200, one 412, and the loser's parts still resolvable for a retry.
- **Conditional DELETE**: stale `If-Match` racing an overwrite → the delete must
  not remove the newer object.
- Run the whole file against both SQLite and PostgreSQL
  (`bin/test integration` and the `postgres` phase).

Reproducers used for this analysis are at
`/private/tmp/claude-501/-Users-pietro-work-arca/09a3f6c8-a221-470d-b39d-f9df27d8ac83/scratchpad/`
(`race_cas.py`, `race_more.py`, `race_del.py`) and fold into this file. That
directory is an ephemeral scratchpad: if it no longer exists at execution
time, do NOT stop — the scenarios above fully specify the tests; rewrite them
from this spec.

### 7.4 Cluster

One test under the existing `cluster_full` marker that pins the §4(A) boundary:
concurrent conditional PUTs to two different nodes both return 200, with a
comment pointing at TD-025. It documents the limit and fails loudly if the
guarantee ever silently changes.

### 7.5 Bookkeeping

`README.md` "Test Coverage" table counts (project rule).

---

## 8. Milestones

| M | Content | Release-critical |
|---|---|---|
| M1 | `arca-core` types + `etag_matches` move + trait shape + both backends + §7.1/§7.2 unit tests | yes |
| M2 | `PutObject` and `CompleteMultipartUpload` handlers + decorators + blob cleanup | yes |
| M3 | `DeleteObject`, versioned delete, `DeleteObjects` batch | yes |
| M4 | Integration suite §7.3 + cluster boundary test §7.4 + README counts | yes |
| M5 | `CHANGELOG.md` Unreleased/Fixed, `TECH_DEBT.md` TD-025, roadmap tech-debt section, `documentation/docs/` + `bin/docs-build` | yes |

M1-M3 are the fix; M4 is what stops it regressing; M5 is what stops the cluster
limit becoming folklore. I would not ship M1-M3 without M4.

TD-025 to record: *cluster-wide conditional-write atomicity not provided —
compare-and-swap is exact per node; concurrent conditional writes for the same
key on different nodes both succeed and converge by LWW. Fix path: owner-node
forwarding for conditional writes (§4 option B).*

## 9. Risks

- **Wasted upload for losing writers.** Inherent to CAS-at-commit; bounded by
  keeping the early check (§3.4). Worth one line in the docs.
- **Orphaned blob if the process dies between a refused CAS and the cleanup.**
  Reclaimed by `arca gc` / anti-entropy GC, same as any other orphan. No new
  class of leak.
- **Composite-blob cleanup on a refused CMU** (§3.5) is the one place where a
  careless `delete` would destroy the client's parts. Needs a dedicated unit
  test, not just review.
- **PG seq gaps** on refused writes: harmless, precedent exists.
- **Trait surface grows** by three methods. Mitigated by making the existing
  methods default delegates, so there is one implementation per backend per
  operation.

---

## 10. Execution run-book (read before starting)

Every decision in this plan is made — do **not** re-ask §4 (cluster option A)
or §6 (behaviour change), both approved by Pietro on 2026-08-26. Execute
M1→M5 strictly in order, each milestone fully green before the next.

**Environment and build (all containerized, never on the host):**

- After ANY code change, rebuild the test images too, or they run stale code:
  `docker compose -f docker/docker-compose.yml build unit-test test`.
- Unit tests: `bin/test unit`. Per-crate filter: `bin/test unit test -p CRATE`
  (the `bin/test unit -p CRATE` form is broken). Never pipe a test run through
  `| tail`.
- Integration tests need a running server: `bin/arca start -d --build --dev`,
  then `bin/test integration`. Run the new file against PostgreSQL too via the
  `postgres` phase of `bin/test`.
- Cluster boundary test (§7.4): runs under the existing `cluster_full` marker
  with the cluster harness (`bin/cluster`).

**Bookkeeping (M5, plus habits during M1–M4):**

- Update `CHANGELOG.md` → `[Unreleased]` → `Fixed` while developing, not at
  the end.
- `TECH_DEBT.md`: add **TD-025** (text in §8), mirror it in the tech-debt
  section of `documentation/docs/roadmap.md`.
- Docs: `ha.md` cluster limit + mitigation (§4A), README compatibility note,
  README "Test Coverage" counts; update `documentation/docs/` sources AND
  rebuild with `bin/docs-build` so `docs/` is in sync.
- `TECHDEBT(TD-025)` comment in `cluster_meta.rs` at the conditional-write
  forward site.

**Commits:** one per milestone, English messages, conventional style used by
the repo (`fix(...)`, `test(...)`, `docs(...)`). Per the standing workflow
rule, propose each commit to Pietro before committing — this is the only
expected interaction during execution. The untracked files at the repo root
(`CONDITIONAL-WRITE-ATOMICITY.md`, the vulnerability report) are Pietro's:
leave them alone and let him decide their fate at commit time.

**After M5:** propose a release to Pietro (standing preference after a
completed phase of work), suggesting a MINOR bump (behaviour fix visible on
the wire, new trait methods, no breaking API).
