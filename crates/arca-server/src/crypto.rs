//! Process-level rustls crypto-provider selection.
//!
//! The dependency graph compiles MORE than one rustls `CryptoProvider`
//! (lapin's TLS stack pulls in `aws-lc-rs` alongside the project's `ring`
//! backend), so rustls cannot pick one implicitly: building a TLS config or
//! an HTTPS client panics unless a process default has been installed.
//! `main()` installs it at startup; the constructors that build TLS machinery
//! (cluster client, membership prober, Vault client, replicator, webhook and
//! Elasticsearch connectors, the rustls server config loader) call this too,
//! so they stay safe from ANY entry point — unit tests included.

use std::sync::Once;

static INSTALL: Once = Once::new();

/// Installs the `ring` crypto provider as the process-wide default
/// (idempotent; an already-installed default — ours or a harness's — is fine).
pub fn ensure_default_crypto_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
