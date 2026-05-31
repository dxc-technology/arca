//! `arca cluster status` — prints this node's identity and configured topology.
//!
//! Phase 29 M1 prints the self identity and the static configuration. Live peer
//! state (reachability, replication lag) is added once the membership manager
//! and the admin API land (M2+).

use crate::config::{ClusterConfig, ClusterMode, DiscoveryMode};

/// Prints a human-readable summary of the cluster configuration for this node.
pub fn print_status(cluster: &ClusterConfig, node_id: &str) {
    println!("Arca cluster status");
    println!("===================");
    println!("  Node ID:      {node_id}");
    println!("  Cluster ID:   {}", cluster.cluster_id);
    match cluster.mode {
        ClusterMode::Quorum => {
            let quorum = cluster.write_quorum().unwrap_or(0);
            let size = cluster.cluster_size.unwrap_or(0);
            println!("  Mode:         quorum (CP) — write majority {quorum} of {size}");
        }
        ClusterMode::Available => {
            println!("  Mode:         available (AP) — always writable");
        }
    }
    match cluster.discovery {
        DiscoveryMode::Mdns => println!("  Discovery:    mDNS (_arca._tcp.local)"),
        DiscoveryMode::Static => {
            println!("  Discovery:    static seeds");
            for seed in &cluster.seeds {
                println!("                  - {seed}");
            }
        }
        DiscoveryMode::Dns => println!(
            "  Discovery:    dns ({})",
            cluster.dns_name.as_deref().unwrap_or("?")
        ),
    }
    println!();
    println!("  Live peer state is shown by the running server's /admin/cluster");
    println!("  endpoint and the console dashboard once the cluster is up.");
}
