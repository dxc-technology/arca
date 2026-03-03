//! Credential generation and bootstrap utilities.

use arca_core::store::CredentialStore;
use arca_core::types::Credential;
use rand::Rng;

/// Characters used in access key IDs (uppercase ASCII + digits).
const KEY_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Characters used in secret access keys (alphanumeric + special).
const SECRET_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Length of generated access key IDs.
const ACCESS_KEY_LEN: usize = 20;

/// Length of generated secret access keys.
const SECRET_KEY_LEN: usize = 40;

/// Generates a random string of the given length from the given character set.
fn random_string(charset: &[u8], len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..charset.len());
            charset[idx] as char
        })
        .collect()
}

/// Generates a new credential with random access key and secret key.
pub fn generate_credential(description: &str) -> Credential {
    Credential {
        access_key_id: random_string(KEY_CHARSET, ACCESS_KEY_LEN),
        secret_access_key: random_string(SECRET_CHARSET, SECRET_KEY_LEN),
        description: description.to_string(),
        created_at: chrono::Utc::now(),
        active: true,
    }
}

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
            }
        }
        _ => {
            tracing::info!("No credentials found, generating root credential");
            let cred = generate_credential("auto-generated root credential");

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_length() {
        let cred = generate_credential("test");
        assert_eq!(cred.access_key_id.len(), ACCESS_KEY_LEN);
    }

    #[test]
    fn secret_key_length() {
        let cred = generate_credential("test");
        assert_eq!(cred.secret_access_key.len(), SECRET_KEY_LEN);
    }

    #[test]
    fn access_key_valid_charset() {
        let cred = generate_credential("test");
        assert!(cred
            .access_key_id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn credential_fields_set() {
        let cred = generate_credential("my description");
        assert_eq!(cred.description, "my description");
        assert!(cred.active);
        // created_at should be recent (within last second)
        let elapsed = chrono::Utc::now() - cred.created_at;
        assert!(elapsed.num_seconds() < 2);
    }

    #[test]
    fn generated_credentials_are_unique() {
        let cred1 = generate_credential("test");
        let cred2 = generate_credential("test");
        assert_ne!(cred1.access_key_id, cred2.access_key_id);
        assert_ne!(cred1.secret_access_key, cred2.secret_access_key);
    }
}
