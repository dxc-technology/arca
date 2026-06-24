//! Offline `arca migrate-topology` (Phase 30, milestone M4).
//!
//! Guided, in-place transition between a standalone single node and an HA
//! cluster. Arca's cluster is FULLY REPLICATED (every node holds every object),
//! NOT sharded, so there is no data resharding or redistribution: this tool only
//! generates the `[cluster]` config stanza, runs a couple of small DB ops, and
//! prints the operator runbook. It complements the Phase 29.1 HA runbooks.
//!
//! Two directions, both run with the server STOPPED:
//!
//! * `--to-cluster` (single -> HA): turns a standalone instance into the FIRST
//!   node of a cluster. Emits a `[cluster]` stanza with a generated `cluster_id`
//!   and a strong random `secret`, reconciles the per-node `object_seq` counter
//!   so the first clustered write cannot skip a pre-cluster object, and prints
//!   the steps to bring up additional (empty) nodes that anti-entropy fills.
//!
//! * `--to-single` (HA -> standalone): collapses a cluster back to one node, run
//!   ON the surviving authoritative node after every peer is confirmed synced
//!   and shut down. Purges the cluster-only state (object tombstones and
//!   control-plane tombstones), VACUUMs SQLite, and prints the steps to strip
//!   `[cluster]` and restart standalone.
//!
//! This is intentionally CLI-only (no console maintenance job): both directions
//! are inherently operator+restart actions — `--to-cluster` emits config the
//! operator must paste in and `--to-single` is cold cleanup after peers are
//! stopped — neither of which an online job can perform.

use anyhow::{Context, Result};

use arca_core::store::{ControlTombstoneStore, MetadataStore, ServerConfigStore};

use crate::config::{ClusterMode, Config, DiscoveryMode};

/// Generates the `[cluster]` config stanza for the first node of a new cluster.
///
/// Returns the rendered TOML so callers (and tests) can either print it or write
/// it to a file. The `secret` is 32 random bytes hex (reuses the encryption
/// module's `ring`-backed RNG via `generate_dek`); the `cluster_id` is a short
/// random label. Defaults mirror [`ClusterConfig`](crate::config::ClusterConfig):
/// `mode = "quorum"` with `cluster_size = 3`, `discovery = "mdns"`.
pub fn render_cluster_stanza() -> Result<String> {
    let secret = {
        let key = arca_storage::encryption::keys::generate_dek()
            .map_err(|e| anyhow::anyhow!("generating cluster secret: {e}"))?;
        hex::encode(key)
    };
    // Short, human-readable, collision-resistant cluster label.
    let cluster_id = format!(
        "arca-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );

    // Defaults chosen to match the ClusterConfig defaults: quorum mode (CP) with
    // the smallest meaningful cluster (3 nodes -> majority of 2), mDNS discovery.
    let mode = ClusterMode::Quorum;
    let discovery = DiscoveryMode::Mdns;

    let mode_str = match mode {
        ClusterMode::Quorum => "quorum",
        ClusterMode::Available => "available",
    };
    let discovery_str = match discovery {
        DiscoveryMode::Mdns => "mdns",
        DiscoveryMode::Static => "static",
        DiscoveryMode::Dns => "dns",
    };

    let mut s = String::new();
    s.push_str("[cluster]\n");
    s.push_str("enabled = true\n");
    s.push_str(&format!("cluster_id = \"{cluster_id}\"\n"));
    s.push_str("# Shared secret authenticating every inter-node request — IDENTICAL on every node.\n");
    s.push_str("# Keep it secret; rotate via secret_previous (see the HA guide).\n");
    s.push_str(&format!("secret = \"{secret}\"\n"));
    s.push_str(&format!("mode = \"{mode_str}\"  # \"quorum\" (CP, default) or \"available\" (AP)\n"));
    s.push_str("cluster_size = 3  # required in quorum mode; the expected number of nodes\n");
    s.push_str(&format!("discovery = \"{discovery_str}\"  # \"mdns\" | \"static\" | \"dns\"\n"));
    s.push_str("\n");
    s.push_str("# For discovery = \"static\", list every node endpoint here (identical on every\n");
    s.push_str("# node; a node ignores its own entry). Then set discovery = \"static\" above:\n");
    s.push_str("# seeds = [\"node-a:9000\", \"node-b:9000\", \"node-c:9000\"]\n");
    s.push_str("\n");
    s.push_str("# For discovery = \"dns\" (e.g. a Kubernetes headless Service), set:\n");
    s.push_str("# dns_name = \"arca-headless.default.svc.cluster.local\"\n");
    s.push_str("\n");
    s.push_str("# Inter-node mutual TLS is REQUIRED when [server.tls] is enabled. Mint the\n");
    s.push_str("# CA + per-node certs with `arca tls generate-cluster --node <name> ...`,\n");
    s.push_str("# then add a [cluster.tls] section pointing at this node's cert/key + the CA:\n");
    s.push_str("# [cluster.tls]\n");
    s.push_str("# ca_file   = \"/etc/arca/certs/cluster/arca-cluster-ca.crt\"\n");
    s.push_str("# cert_file = \"/etc/arca/certs/cluster/<this-node>.crt\"\n");
    s.push_str("# key_file  = \"/etc/arca/certs/cluster/<this-node>.key\"\n");

    Ok(s)
}

/// Entry point for `arca migrate-topology --to-cluster [--output <file>]`.
///
/// Preflight, then: emit the `[cluster]` stanza (to stdout and optionally to
/// `output`), reconcile the `object_seq` counter, and print the runbook.
pub async fn run_to_cluster(
    config: &Config,
    output: Option<&std::path::Path>,
) -> Result<()> {
    println!("arca migrate-topology --to-cluster");
    println!("Turns this standalone instance into the FIRST node of a new HA cluster.\n");

    // Preflight: refuse if already clustered (the stanza is already present).
    if config.cluster.as_ref().is_some_and(|c| c.enabled) {
        anyhow::bail!(
            "this instance is already clustered ([cluster].enabled = true). \
             To add MORE nodes, bring up empty nodes with the same [cluster] stanza — \
             anti-entropy will replicate this node's data to them. See the HA guide."
        );
    }

    // Open the metadata store to reconcile the write counter. Use the same
    // backend the running server would (auto-detected in load_config).
    println!("Opening metadata backend: {}", config.storage.metadata_backend);
    let stores = open_topology_stores(config).await?;

    let before = stores.metadata.current_object_seq().await?;
    let after = stores.metadata.seed_object_seq_to_max().await?;
    println!(
        "Reconciled object_seq write counter: {before} -> {after} (so the first \
         clustered write cannot skip a pre-cluster object).\n"
    );

    // Generate and emit the [cluster] stanza.
    let stanza = render_cluster_stanza()?;
    if let Some(path) = output {
        std::fs::write(path, &stanza)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("Wrote the [cluster] stanza to {}.\n", path.display());
    }
    println!("Add this section to your config.toml (config-path: {}):\n", config_path_hint(config));
    println!("{stanza}");

    println!("\nNext steps:");
    println!("  1. Paste the [cluster] section above into THIS node's config.toml.");
    println!("     (If you used --output, copy the file contents in.)");
    println!("  2. If [server.tls] is enabled, generate inter-node mTLS material:");
    println!("       arca tls generate-cluster --node <this-node> --node <node-2> --node <node-3>");
    println!("     and add the printed [cluster.tls] section (see the commented template).");
    println!("  3. Restart THIS node. Restarting with [cluster].enabled = true switches the");
    println!("     metadata store into cluster (tombstone) mode automatically — no flag needed.");
    println!("  4. Bring up the OTHER nodes EMPTY with the SAME [cluster] stanza (same");
    println!("     cluster_id and secret; per-node TLS cert/key). They start blank and");
    println!("     anti-entropy pulls the full dataset from this seed node — no data copy.");
    println!("  5. Verify convergence with `arca cluster status` and `GET /admin/cluster`.");

    Ok(())
}

/// Entry point for `arca migrate-topology --to-single [--force]`.
///
/// Run ON the surviving authoritative node, with every peer confirmed synced and
/// stopped. Purges cluster-only state (object + control-plane tombstones),
/// VACUUMs SQLite, and prints the steps to go standalone.
pub async fn run_to_single(config: &Config, force: bool) -> Result<()> {
    println!("arca migrate-topology --to-single");
    println!("Collapses the cluster back to THIS single surviving node.\n");

    if !config.cluster.as_ref().is_some_and(|c| c.enabled) {
        anyhow::bail!(
            "this instance is not clustered ([cluster].enabled is unset/false): \
             nothing to collapse."
        );
    }

    if !force {
        anyhow::bail!(
            "PRECONDITION: run this ONLY on the surviving authoritative node, after \
             confirming every peer reported in-sync (first_pass_done) via \
             `GET /admin/cluster` or `arca cluster status` AND every peer is stopped. \
             Collapsing while a peer is behind LOSES that peer's un-replicated writes. \
             Re-run with --force once you have confirmed this."
        );
    }

    println!("Opening metadata backend: {}", config.storage.metadata_backend);
    let stores = open_topology_stores(config).await?;

    // Purge cluster-only state. Both purges delete rows older than the cutoff;
    // a cutoff slightly in the FUTURE deletes ALL of them (immediate purge).
    let cutoff = chrono::Utc::now() + chrono::Duration::seconds(1);
    let object_tombstones = stores.metadata.purge_tombstones(cutoff).await?;
    let control_tombstones = stores
        .control_tombstone
        .purge_control_tombstones(cutoff)
        .await?;
    println!(
        "Purged cluster-only state: {object_tombstones} object tombstone(s), \
         {control_tombstones} control-plane tombstone(s)."
    );

    // Reclaim the freed pages (SQLite only; PostgreSQL autovacuum handles this).
    match stores.sqlite_for_vacuum {
        Some(s) => {
            print!("VACUUM (reclaiming freed SQLite pages)... ");
            s.vacuum().await?;
            println!("done.");
        }
        None => {
            println!(
                "PostgreSQL backend: no VACUUM needed (autovacuum reclaims the freed rows)."
            );
        }
    }

    println!("\nNext steps:");
    println!("  1. Stop this node if it is still running.");
    println!("  2. Remove the entire [cluster] section (and [cluster.tls], if any) from");
    println!("     this node's config.toml.");
    println!("  3. Restart. With no [cluster] section the node runs standalone again");
    println!("     (hard deletes remove rows outright instead of tombstoning).");

    Ok(())
}

/// Returns the configured `config-path` for display, if we can infer it. We do
/// not have the original path here (only the parsed config), so this is a hint.
fn config_path_hint(_config: &Config) -> &'static str {
    "your config.toml"
}

/// Store handles the topology tool needs. Opened directly from config (NOT
/// wrapped in caching/cluster decorators) so the DB ops hit the real backend.
struct TopologyStores {
    metadata: std::sync::Arc<dyn MetadataStore>,
    control_tombstone: std::sync::Arc<dyn ControlTombstoneStore>,
    /// Concrete SQLite store for VACUUM; `None` on PostgreSQL.
    sqlite_for_vacuum: Option<std::sync::Arc<arca_storage::SqliteStore>>,
    /// Server-config store (used for preflight inspection; kept for clarity).
    #[allow(dead_code)]
    server_config: std::sync::Arc<dyn ServerConfigStore>,
}

/// Opens the metadata backend from config, returning the store handles the
/// topology tool needs. Mirrors `open_stores` but keeps the concrete SQLite
/// store around for VACUUM.
async fn open_topology_stores(config: &Config) -> Result<TopologyStores> {
    match config.storage.metadata_backend.as_str() {
        "sqlite" => {
            let store = std::sync::Arc::new(
                arca_storage::SqliteStore::open(&config.storage.db_path())
                    .await
                    .context("opening SQLite store")?,
            );
            Ok(TopologyStores {
                metadata: store.clone() as std::sync::Arc<dyn MetadataStore>,
                control_tombstone: store.clone()
                    as std::sync::Arc<dyn ControlTombstoneStore>,
                server_config: store.clone() as std::sync::Arc<dyn ServerConfigStore>,
                sqlite_for_vacuum: Some(store),
            })
        }
        "postgres" => {
            let pg = config.storage.postgres.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "[storage.postgres] section required when metadata_backend = \"postgres\""
                )
            })?;
            let store = std::sync::Arc::new(
                arca_storage::PgStore::open(&pg.connection_string, pg.max_connections)
                    .await
                    .context("opening PostgreSQL store")?,
            );
            Ok(TopologyStores {
                metadata: store.clone() as std::sync::Arc<dyn MetadataStore>,
                control_tombstone: store.clone()
                    as std::sync::Arc<dyn ControlTombstoneStore>,
                server_config: store.clone() as std::sync::Arc<dyn ServerConfigStore>,
                sqlite_for_vacuum: None,
            })
        }
        other => anyhow::bail!("unknown metadata backend \"{other}\" (expected sqlite|postgres)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClusterConfig;

    #[test]
    fn cluster_stanza_parses_back_with_strong_secret_and_id() {
        let stanza = render_cluster_stanza().unwrap();

        // The emitted TOML must parse as a ClusterConfig and validate.
        #[derive(serde::Deserialize)]
        struct Wrapper {
            cluster: ClusterConfig,
        }
        let parsed: Wrapper = toml::from_str(&stanza).expect("stanza parses as TOML");
        let c = parsed.cluster;

        assert!(c.enabled);
        assert!(!c.cluster_id.trim().is_empty());
        assert!(c.cluster_id.starts_with("arca-"));
        // 32 bytes hex = 64 chars; well above the 16-char hard floor.
        assert_eq!(c.secret.len(), 64);
        assert!(c.secret.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(c.mode, ClusterMode::Quorum);
        assert_eq!(c.discovery, DiscoveryMode::Mdns);
        assert_eq!(c.cluster_size, Some(3));

        // It must pass the real config validation (secret length, quorum size).
        c.validate().expect("emitted stanza validates");
    }

    #[test]
    fn two_stanzas_have_distinct_secrets_and_ids() {
        let a = render_cluster_stanza().unwrap();
        let b = render_cluster_stanza().unwrap();
        assert_ne!(a, b, "each invocation must generate fresh material");
    }

    #[tokio::test]
    async fn seed_object_seq_to_max_reconciles_counter() {
        use arca_core::store::MetadataStore;
        use arca_core::types::{BlobId, ObjectRecord};
        use std::collections::HashMap;

        let store = arca_storage::SqliteStore::open_in_memory().await.unwrap();

        // Fresh DB: counter and max are both 0.
        assert_eq!(store.current_object_seq().await.unwrap(), 0);
        assert_eq!(store.seed_object_seq_to_max().await.unwrap(), 0);

        let make = |key: &str| ObjectRecord {
            bucket: "b".to_string(),
            key: key.to_string(),
            blob_id: BlobId("blob".to_string()),
            size: 1,
            etag: "e".to_string(),
            content_type: None,
            last_modified: chrono::Utc::now(),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            replication_status: None,
            lock_updated_at: None,
        };

        // Each put advances the counter (and stamps seq on the row).
        store.create_bucket("b").await.unwrap();
        for key in ["k1", "k2", "k3"] {
            store.put_object(&make(key)).await.unwrap();
        }
        let after_puts = store.current_object_seq().await.unwrap();
        assert!(after_puts >= 3, "counter advanced past the 3 writes");

        // Re-seeding never rewinds and never goes below MAX(seq).
        let seeded = store.seed_object_seq_to_max().await.unwrap();
        assert!(seeded >= after_puts, "seed only ever bumps the counter up");
        assert_eq!(
            store.current_object_seq().await.unwrap(),
            seeded,
            "counter now equals the seeded value"
        );

        // Idempotent: a second seed is a no-op at this point.
        assert_eq!(store.seed_object_seq_to_max().await.unwrap(), seeded);
    }
}
