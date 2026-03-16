//! Vault/OpenBAO KV v2 client for master encryption key management.
//!
//! At startup Arca tries to read the master key from Vault. If the secret does
//! not exist yet, Arca generates a random 256-bit key and writes it. This
//! mirrors how MinIO and MongoDB handle KMS integration: the application owns
//! its key lifecycle rather than requiring an external provisioning step.

use anyhow::{bail, Context, Result};

use crate::config::{KmsAuthMethod, KmsConfig};
use arca_storage::encryption::keys::MasterKey;

/// Ensures the master encryption key exists in Vault/OpenBAO KV v2.
///
/// 1. Authenticates (token or AppRole).
/// 2. Tries to read the secret.
/// 3. If found, returns the existing key.
/// 4. If not found (HTTP 404), generates a random 256-bit key, writes it to
///    Vault, and returns it.
pub async fn ensure_master_key(kms: &KmsConfig) -> Result<MasterKey> {
    let client = build_http_client(kms)?;

    let token = match &kms.auth_method {
        KmsAuthMethod::Token => kms
            .token
            .clone()
            .context("[encryption.kms] token is required for token auth")?,
        KmsAuthMethod::Approle => approle_login(&client, kms).await?,
    };

    let api_path = normalize_kv2_path(&kms.secret_path);
    let url = format!("{}/v1/{}", kms.endpoint.trim_end_matches('/'), api_path);

    let resp = client
        .get(&url)
        .header("X-Vault-Token", &token)
        .send()
        .await
        .with_context(|| format!("KMS request failed — is Vault/OpenBAO reachable at {}?", kms.endpoint))?;

    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "KMS authentication failed (HTTP {status}) — check your token or AppRole credentials"
        );
    }

    if status == reqwest::StatusCode::NOT_FOUND {
        tracing::info!(
            path = %kms.secret_path,
            "KMS secret not found, generating new master key"
        );
        return generate_and_write_key(&client, &token, &url, kms).await;
    }

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("KMS returned HTTP {status}: {body}");
    }

    parse_key_response(resp, kms).await
}

/// Generates a random 256-bit master key and writes it to Vault KV v2.
async fn generate_and_write_key(
    client: &reqwest::Client,
    token: &str,
    url: &str,
    kms: &KmsConfig,
) -> Result<MasterKey> {
    use base64::Engine;

    let key_bytes = arca_storage::encryption::keys::generate_dek()
        .map_err(|e| anyhow::anyhow!("failed to generate master key: {e}"))?;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);

    // Vault KV v2 write: POST /v1/{mount}/data/{path}
    let body = serde_json::json!({
        "data": {
            &kms.secret_field: key_b64
        }
    });

    let resp = client
        .post(url)
        .header("X-Vault-Token", token)
        .json(&body)
        .send()
        .await
        .context("failed to write master key to KMS")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("KMS write failed (HTTP {status}): {body}");
    }

    tracing::info!(path = %kms.secret_path, "New master key written to KMS");

    MasterKey::from_bytes(&key_bytes)
        .map_err(|e| anyhow::anyhow!("generated key is invalid: {e}"))
}

/// Parses a Vault KV v2 response to extract the master key.
async fn parse_key_response(resp: reqwest::Response, kms: &KmsConfig) -> Result<MasterKey> {
    let body: serde_json::Value = resp
        .json()
        .await
        .context("KMS response is not valid JSON")?;

    // KV v2 response structure: { "data": { "data": { "<field>": "<value>" } } }
    let key_b64 = body
        .get("data")
        .and_then(|d| d.get("data"))
        .and_then(|d| d.get(&kms.secret_field))
        .and_then(|v| v.as_str())
        .with_context(|| {
            format!(
                "KMS secret at '{}' does not contain field '{}' — verify the secret structure",
                kms.secret_path, kms.secret_field
            )
        })?;

    MasterKey::from_base64(key_b64)
        .map_err(|e| anyhow::anyhow!("KMS secret field '{}' contains an invalid key: {e}", kms.secret_field))
}

/// Authenticates with Vault/OpenBAO using AppRole and returns a client token.
async fn approle_login(client: &reqwest::Client, kms: &KmsConfig) -> Result<String> {
    let url = format!(
        "{}/v1/auth/approle/login",
        kms.endpoint.trim_end_matches('/')
    );

    let body = serde_json::json!({
        "role_id": kms.role_id.as_deref().unwrap_or_default(),
        "secret_id": kms.secret_id.as_deref().unwrap_or_default(),
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("AppRole login failed — is Vault/OpenBAO reachable at {}?", kms.endpoint))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("AppRole login failed (HTTP {status}): {body}");
    }

    let resp_body: serde_json::Value = resp
        .json()
        .await
        .context("AppRole login response is not valid JSON")?;

    resp_body
        .get("auth")
        .and_then(|a| a.get("client_token"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .context("AppRole login response missing auth.client_token")
}

/// Builds an HTTP client with optional TLS configuration.
fn build_http_client(kms: &KmsConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();

    if kms.tls_skip_verify {
        builder = builder.danger_accept_invalid_certs(true);
    }

    if let Some(ref ca_path) = kms.ca_file {
        let ca_pem = std::fs::read(ca_path)
            .with_context(|| format!("reading KMS CA certificate: {ca_path}"))?;
        let cert = reqwest::Certificate::from_pem(&ca_pem)
            .with_context(|| format!("parsing KMS CA certificate: {ca_path}"))?;
        builder = builder.add_root_certificate(cert);
    }

    builder.build().context("building KMS HTTP client")
}

/// Normalizes a KV v2 secret path by inserting `/data/` after the mount point.
///
/// Vault KV v2 requires the API path to include `/data/` between the mount
/// and the secret path, but users typically write `secret/arca/master-key`.
/// This function auto-inserts `/data/` if not already present.
///
/// Examples:
/// - `secret/arca/master-key` → `secret/data/arca/master-key`
/// - `secret/data/arca/master-key` → `secret/data/arca/master-key` (no change)
/// - `kv/myapp/key` → `kv/data/myapp/key`
/// - `kv/data/myapp/key` → `kv/data/myapp/key` (no change)
fn normalize_kv2_path(secret_path: &str) -> String {
    let path = secret_path.trim_start_matches('/');
    let parts: Vec<&str> = path.splitn(2, '/').collect();

    match parts.as_slice() {
        [mount, rest] => {
            if rest.starts_with("data/") || *rest == "data" {
                // Already has /data/ segment — no change.
                path.to_string()
            } else {
                format!("{mount}/data/{rest}")
            }
        }
        // Single-segment path (just a mount name, no sub-path) — unlikely but handle it.
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_inserts_data_segment() {
        assert_eq!(
            normalize_kv2_path("secret/arca/master-key"),
            "secret/data/arca/master-key"
        );
    }

    #[test]
    fn normalize_preserves_existing_data_segment() {
        assert_eq!(
            normalize_kv2_path("secret/data/arca/master-key"),
            "secret/data/arca/master-key"
        );
    }

    #[test]
    fn normalize_custom_mount() {
        assert_eq!(
            normalize_kv2_path("kv/myapp/key"),
            "kv/data/myapp/key"
        );
    }

    #[test]
    fn normalize_strips_leading_slash() {
        assert_eq!(
            normalize_kv2_path("/secret/arca/key"),
            "secret/data/arca/key"
        );
    }

    #[test]
    fn normalize_single_segment() {
        assert_eq!(normalize_kv2_path("secret"), "secret");
    }

    #[test]
    fn normalize_data_only_rest() {
        assert_eq!(
            normalize_kv2_path("secret/data"),
            "secret/data"
        );
    }

    #[test]
    fn parse_kv2_response() {
        // Simulate parsing a Vault KV v2 JSON response.
        let json: serde_json::Value = serde_json::json!({
            "data": {
                "data": {
                    "key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                },
                "metadata": {
                    "version": 1
                }
            }
        });

        let key_b64 = json
            .get("data")
            .and_then(|d| d.get("data"))
            .and_then(|d| d.get("key"))
            .and_then(|v| v.as_str())
            .unwrap();

        assert_eq!(key_b64, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert!(MasterKey::from_base64(key_b64).is_ok());
    }

    #[test]
    fn parse_approle_login_response() {
        let json: serde_json::Value = serde_json::json!({
            "auth": {
                "client_token": "s.abc123def456",
                "accessor": "xyz",
                "policies": ["default", "arca-kms"]
            }
        });

        let token = json
            .get("auth")
            .and_then(|a| a.get("client_token"))
            .and_then(|t| t.as_str())
            .unwrap();

        assert_eq!(token, "s.abc123def456");
    }
}
