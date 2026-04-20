// ==================== HELP CONTENT CATALOG ====================
//
// Each topic is keyed by a stable id used by helpTrigger(id) in the UI. The
// shape is:
//
//   {
//     section: 'Bucket setting',   // breadcrumb-style label at the modal top
//     title:   'Compression',      // modal headline
//     hint:    'One-line gist…',   // the tooltip shown on hover
//     body:    [ block, block ],   // array of blocks rendered by helpModal
//   }
//
// Block kinds:
//   { kind: 'intro',   text }
//   { kind: 'note',    tone, text }      // tone = info | warn | tip
//   { kind: 'bullets', items: [str] }
//   { kind: 'rows',    rows: [{ key, text, best }] }
//   { kind: 'code',    text }
//
// Keep text concise — the whole point of inline help is "skim, learn, close".
// Link to /docs for long-form reference.

export const HELP_TOPICS = {
  // =========================== COMPRESSION ===========================
  'compression.algorithm': {
    section: 'Bucket · Compression',
    title: 'Compression algorithms',
    hint: 'Seven codecs, each tuned to a different ratio/speed trade-off.',
    body: [
      { kind: 'intro', text:
        'Arca ships seven codecs. Pick by matching your content type and whether you care more about disk footprint or encode/decode CPU. MIME-type filter skips already-compressed payloads (images, archives, PDFs) automatically; objects under 1 KiB are skipped too.' },
      { kind: 'rows', rows: [
        { key: 'auto',   text: 'Picks per object from a deterministic rule table on Content-Type and size. No sampling or benchmarking — just a short match.',            best: 'Mixed workloads where you don\'t want to tune per bucket.' },
        { key: 'zstd',   text: 'Modern general-purpose codec. Excellent ratio, very fast decode, level-tunable 1–22 (default 3).',                                          best: 'JSON, XML, YAML, logs, source archives, databases.' },
        { key: 'lz4',    text: 'Extreme encode/decode speed at the cost of ratio. Pure-Rust block variant — no tunable level.',                                             best: 'CPU-constrained hot paths; request-latency-sensitive workloads.' },
        { key: 'snappy', text: 'Google-designed streaming codec. Speed/ratio profile similar to lz4.',                                                                       best: 'Interop with Kafka, HBase, Parquet and other Snappy-native pipelines.' },
        { key: 'gzip',   text: 'Classic DEFLATE. Medium ratio, medium speed. Level 0–9 (default 6). Ubiquitous tooling support.',                                            best: 'Interop with external CLIs and downloads that expect .gz.' },
        { key: 'brotli', text: 'Exceptional ratio on textual web payloads. Slow encode, medium decode. Level 0–11 (default 4).',                                             best: 'HTML / CSS / JS / SVG delivery where shrinking dominates.' },
        { key: 'xz',     text: 'LZMA2-based. Highest ratio of the set, slowest encode. Level 0–9 (default 6).',                                                              best: 'Cold-storage archives where disk footprint beats CPU.' },
      ] },
      { kind: 'note', tone: 'info', text:
        'Changing the algorithm applies to future uploads only. Run arca compress-existing --algorithm <name> to migrate already-stored objects.' },
    ],
  },
  'compression.level': {
    section: 'Bucket · Compression',
    title: 'Compression level',
    hint: 'Higher = better ratio but more CPU. auto/lz4/snappy have no tunable level.',
    body: [
      { kind: 'intro', text:
        'Each codec has its own valid level range; defaults in Arca favor speed over maximum ratio.' },
      { kind: 'rows', rows: [
        { key: 'zstd',   text: '1–22, default 3. Raising to 9 roughly doubles the encode cost for a ~10 % size reduction.',   best: 'Default 3 for live traffic; 9 or higher when batch-compressing cold data.' },
        { key: 'gzip',   text: '0–9, default 6. Level 0 is store-only (no compression).',                                       best: 'Leave at 6 unless interop is the whole point.' },
        { key: 'brotli', text: '0–11, default 4. Level 11 is archival — extremely slow encode, best ratio.',                    best: '4 for live delivery, 11 for precomputed static assets.' },
        { key: 'xz',     text: '0–9, default 6. Level 9 is the LZMA archival preset.',                                          best: 'Archive-class batch jobs where CPU time is cheap.' },
      ] },
      { kind: 'note', tone: 'tip', text:
        'lz4 and snappy don\'t expose a level (single-mode codecs). auto resolves to a concrete algorithm per object and uses that algorithm\'s own default level.' },
    ],
  },

  // =========================== ENCRYPTION ===========================
  'encryption': {
    section: 'Bucket · Encryption',
    title: 'Server-side encryption (SSE-S3)',
    hint: 'Transparent AES-256-GCM at rest. ETag stays the plaintext MD5.',
    body: [
      { kind: 'intro', text:
        'SSE-S3 encrypts blob contents with AES-256-GCM at rest. Each object gets a random data-encryption key (DEK) that is wrapped with a single master key (KEK) loaded from config or KMS at startup.' },
      { kind: 'bullets', items: [
        'Transparent to clients — Content-Length reports plaintext size; ETag is the MD5 of plaintext.',
        'Mixed-mode: encrypted and plaintext blobs coexist in the same bucket (sidecar records per-object encryption state).',
        'Range reads and multipart work on encrypted objects.',
        'Master key rotation: keep the old one in previous_master_key while new writes use the new one.',
      ] },
      { kind: 'note', tone: 'info', text:
        'When the server default is Disabled, you can still toggle per-bucket encryption here — as long as a master key was configured at startup.' },
    ],
  },

  // =========================== VERSIONING ===========================
  'versioning': {
    section: 'Bucket · Versioning',
    title: 'Object versioning',
    hint: 'Keep every write as a new version; deletes create tombstones instead of erasing.',
    body: [
      { kind: 'intro', text:
        'With versioning on, every PUT to an existing key creates a new version; DELETE creates a delete marker that hides older versions without removing them. You can read, restore or permanently purge any historical version.' },
      { kind: 'rows', rows: [
        { key: 'Enabled',   text: 'New versionIds are assigned on every write. Delete markers on top of old versions.', best: 'Production buckets where recovery is important.' },
        { key: 'Suspended', text: 'New writes use the null versionId and overwrite the previous null version. Existing versions are preserved.',         best: 'Switching off versioning without losing history.' },
        { key: 'Unversioned', text: 'Never enabled. DELETE permanently removes the object. No version tracking.',       best: 'Ephemeral scratch buckets.' },
      ] },
      { kind: 'note', tone: 'warn', text:
        'Enabling Object Lock auto-enables versioning and prevents it from being suspended. Once enabled, you can only suspend, never go back to Unversioned.' },
    ],
  },

  // =========================== OBJECT LOCK ===========================
  'object-lock': {
    section: 'Bucket · Object Lock',
    title: 'Object Lock (WORM compliance)',
    hint: 'Prevents objects from being deleted or overwritten for a retention period.',
    body: [
      { kind: 'intro', text:
        'Object Lock enforces Write-Once-Read-Many semantics per object version. It has two independent knobs: a retention mode with a retain-until date, and a legal hold flag (toggleable independently of retention).' },
      { kind: 'rows', rows: [
        { key: 'GOVERNANCE', text: 'Protects versions from deletion, but admins with the s3:BypassGovernanceRetention permission can override on a per-request basis.',   best: 'Internal data-retention policy with an operator escape hatch.' },
        { key: 'COMPLIANCE', text: 'Nobody — not even the root account — can delete or shorten retention. Period ends only when the retain-until date passes.',           best: 'Regulatory compliance (SEC 17a-4, FINRA, etc.).' },
        { key: 'Legal hold', text: 'Independent boolean flag. While ON, the version cannot be deleted regardless of retention state.',                                      best: 'Ad-hoc litigation freezes on specific objects.' },
      ] },
      { kind: 'note', tone: 'warn', text:
        'Enabling Object Lock on a bucket also enables versioning, and neither can be turned off afterwards. Choose deliberately.' },
    ],
  },

  // =========================== LIFECYCLE ===========================
  'lifecycle': {
    section: 'Bucket · Lifecycle',
    title: 'Lifecycle rules',
    hint: 'Time-based policies that expire objects or abort stale multipart uploads.',
    body: [
      { kind: 'intro', text:
        'A lifecycle rule is a filter (prefix / tag / logical AND) plus one or more time-based actions. A background worker scans buckets periodically and applies matching rules.' },
      { kind: 'bullets', items: [
        'Expiration — delete current versions after N days. Uses the object\'s own last-modified timestamp.',
        'Noncurrent version expiration — permanently purge old versions N days after they become noncurrent. Useful to cap cost on versioned buckets.',
        'Abort incomplete multipart uploads — clean up part blobs from uploads that were never completed (default action: 7 days).',
        'Filters: apply to the whole bucket, a key prefix, a set of tags, or an AND of prefix+tags.',
      ] },
      { kind: 'note', tone: 'tip', text:
        'Rules are evaluated in insertion order; the first matching rule wins. Put narrow filters before broad ones.' },
    ],
  },

  // =========================== NOTIFICATIONS ===========================
  'notifications': {
    section: 'Bucket · Notifications',
    title: 'Event notifications',
    hint: 'Send an event to a destination (webhook, Kafka, etc.) whenever an object changes.',
    body: [
      { kind: 'intro', text:
        'Arca emits S3-compatible event payloads whenever objects are created, copied, or deleted. Each notification entry routes events matching a filter to one destination via the connector framework.' },
      { kind: 'bullets', items: [
        'Events: s3:ObjectCreated:* (Put, Copy, CompleteMultipartUpload), s3:ObjectRemoved:* (Delete, DeleteMarkerCreated).',
        'Filters: suffix/prefix on object keys.',
        'Destinations (connectors): Webhook, Kafka, AMQP, Redis, NATS, MQTT, PostgreSQL, MySQL, MongoDB, Elasticsearch, Syslog, SMTP, gRPC — 13 in total.',
        'Delivery is best-effort with retries; failures are logged and exposed via /admin/notifications and Prometheus counters.',
      ] },
      { kind: 'note', tone: 'info', text:
        'The full payload schema matches AWS S3 (minus the AWS-specific fields). See guide/connectors for per-connector configuration.' },
    ],
  },

  // =========================== SETTINGS PAGE ===========================
  'settings.log-level': {
    section: 'Settings',
    title: 'Log level',
    hint: 'Filter verbosity for the server\'s tracing output. Applies instantly, no restart.',
    body: [
      { kind: 'rows', rows: [
        { key: 'error', text: 'Critical errors only.',                              best: 'Production when you trust your metrics for everything else.' },
        { key: 'warn',  text: 'Errors + recoverable warnings.',                     best: 'Production (recommended default).' },
        { key: 'info',  text: 'Operational events (startup, config loads, auth).', best: 'Staging, or debugging a production issue.' },
        { key: 'debug', text: 'Per-request detail, protocol traces.',               best: 'Local development.' },
        { key: 'trace', text: 'Everything, including library internals.',           best: 'Deep debugging — use briefly, it\'s very noisy.' },
      ] },
      { kind: 'note', tone: 'tip', text:
        'You can scope levels by module, e.g. arca=debug,tower=warn. Settings here override server.log_level from the config file.' },
    ],
  },
  'settings.region': {
    section: 'Settings',
    title: 'S3 region',
    hint: 'Advertised in the LocationConstraint field. Rarely needs changing.',
    body: [
      { kind: 'intro', text:
        'The region is returned by GetBucketLocation and is expected by some S3 SDKs for signing (SigV4 uses it in the credential scope). Arca accepts any region; clients don\'t have to match it precisely.' },
      { kind: 'note', tone: 'tip', text:
        'If you hard-code server.region in the config file, this field is read-only. Otherwise, changing it here updates the server_config table and takes effect on the next request.' },
    ],
  },
  'settings.retention.audit': {
    section: 'Settings',
    title: 'Audit log retention',
    hint: 'How long to keep entries in the audit_log table.',
    body: [
      { kind: 'intro', text:
        'Every authenticated operation (S3 + admin) is recorded to the audit log: who, when, what, target, result. A background worker prunes rows older than this threshold once per hour.' },
      { kind: 'bullets', items: [
        '0 = keep forever (not recommended for high-traffic clusters).',
        'Typical values: 30 days (dev), 90 days (prod), 365 days+ (regulated environments).',
        'Storage cost scales linearly with request volume.',
      ] },
    ],
  },
  'settings.retention.metrics': {
    section: 'Settings',
    title: 'Metrics snapshot retention',
    hint: 'How long to keep historical gauge snapshots for the dashboard.',
    body: [
      { kind: 'intro', text:
        'Arca periodically snapshots gauges (bucket count, object count, bytes stored) so the dashboard can draw trend charts. Point-in-time counters (requests, latency histograms) are exposed by /admin/metrics without history.' },
      { kind: 'note', tone: 'tip', text:
        '0 = keep forever. 90 days is a reasonable default; if you need longer retention, scrape with Prometheus or another external TSDB.' },
    ],
  },
  'settings.retention.notifications': {
    section: 'Settings',
    title: 'Notification event retention',
    hint: 'How long to keep delivered & failed events visible in the console event log.',
    body: [
      { kind: 'intro', text:
        'Every event published through a connector is persisted to the notification_events table so operators can inspect deliveries and retries. The retention window is independent of whether delivery succeeded.' },
    ],
  },

  // =========================== USERS / TEAMS / GRANTS ===========================
  'rbac.grant': {
    section: 'Access control',
    title: 'Grants',
    hint: 'A grant = who + what action + which resource + optional condition.',
    body: [
      { kind: 'intro', text:
        'Arca\'s access-control model is a flat list of grants; the request principal is allowed if any attached grant matches. Grants are attached to a user, a team, or Everyone.' },
      { kind: 'bullets', items: [
        'Principal: user, team (all members), or Everyone.',
        'Action: one of the S3 action verbs (s3:GetObject, s3:PutBucketPolicy, …) or a wildcard.',
        'Resource: bucket or bucket+key-prefix. Wildcards allowed.',
        'Condition: optional SigV4 credential, source IP, or other guard.',
      ] },
      { kind: 'note', tone: 'info', text:
        'Admin credentials bypass the grant system — they can do anything. Root is always an admin.' },
    ],
  },
  'rbac.team': {
    section: 'Access control',
    title: 'Teams',
    hint: 'Named groups of users. Attach a grant to a team once — every member gets it.',
    body: [
      { kind: 'intro', text:
        'Teams are the simplest way to keep grants tidy. Instead of granting s3:GetObject to seven individual developers, create a "developers" team, add the grant once, and manage membership separately.' },
      { kind: 'note', tone: 'tip', text:
        'A user can belong to any number of teams. Grants stack — a user gets the union of their own grants plus every team\'s grants.' },
    ],
  },
};

// Returns a topic by id, falling back to a generic "missing" topic so the UI
// never crashes when a topic id has a typo.
export function topicFor(id) {
  return HELP_TOPICS[id] || {
    section: 'Help',
    title: 'Topic not found',
    hint: id,
    body: [{ kind: 'intro', text: `No help content registered under "${id}" yet.` }],
  };
}
