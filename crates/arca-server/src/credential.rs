//! Credential bootstrap utilities.

pub use arca_core::credential::generate_credential;
use arca_core::store::CredentialStore;
use arca_core::types::Credential;

/// Ensures at least one active credential exists.
///
/// If `ARCA_ROOT_ACCESS_KEY` and `ARCA_ROOT_SECRET_KEY` environment variables
/// are both set and no active credentials exist, uses those values.
/// Otherwise generates random credentials.
pub async fn ensure_root_credential(
    store: &dyn CredentialStore,
) -> anyhow::Result<()> {
    let count = store.count_active_credentials().await?;
    if count > 0 {
        tracing::info!(count, "Active credentials found");
        return Ok(());
    }

    let env_access = std::env::var("ARCA_ROOT_ACCESS_KEY").ok();
    let env_secret = std::env::var("ARCA_ROOT_SECRET_KEY").ok();

    let cred = match (env_access, env_secret) {
        (Some(access_key), Some(secret_key)) => {
            tracing::info!("Using root credential from environment variables");
            Credential {
                access_key_id: access_key,
                secret_access_key: secret_key,
                description: "root credential (from env)".to_string(),
                created_at: chrono::Utc::now(),
                active: true,
                admin: true,
            }
        }
        _ => {
            tracing::info!("No credentials found, generating root credential");
            let cred = generate_credential("auto-generated root credential", true);

            println!();
            println!("========================================");
            println!("  Root credential created automatically");
            println!("========================================");
            println!("  Access Key: {}", cred.access_key_id);
            println!("  Secret Key: {}", cred.secret_access_key);
            println!("========================================");
            println!("  WARNING: This will only be shown once.");
            println!("  Store these credentials securely.");
            println!("========================================");
            println!();

            cred
        }
    };

    store.put_credential(&cred).await?;
    Ok(())
}
