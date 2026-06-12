//! Credential generation utilities.

use crate::types::Credential;
use rand::RngExt;

/// Characters used in access key IDs (uppercase ASCII + digits).
const KEY_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Characters used in secret access keys (alphanumeric + special).
const SECRET_CHARSET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Length of generated access key IDs.
const ACCESS_KEY_LEN: usize = 20;

/// Length of generated secret access keys.
const SECRET_KEY_LEN: usize = 40;

/// Generates a random string of the given length from the given character set.
fn random_string(charset: &[u8], len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..charset.len());
            charset[idx] as char
        })
        .collect()
}

/// Generates a new credential with random access key and secret key.
pub fn generate_credential(description: &str, admin: bool, user_id: &str) -> Credential {
    Credential {
        access_key_id: random_string(KEY_CHARSET, ACCESS_KEY_LEN),
        secret_access_key: random_string(SECRET_CHARSET, SECRET_KEY_LEN),
        description: description.to_string(),
        created_at: chrono::Utc::now(),
        active: true,
        admin,
        user_id: user_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_length() {
        let cred = generate_credential("test", false, "root");
        assert_eq!(cred.access_key_id.len(), ACCESS_KEY_LEN);
    }

    #[test]
    fn secret_key_length() {
        let cred = generate_credential("test", false, "root");
        assert_eq!(cred.secret_access_key.len(), SECRET_KEY_LEN);
    }

    #[test]
    fn access_key_valid_charset() {
        let cred = generate_credential("test", false, "root");
        assert!(cred
            .access_key_id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn credential_fields_set() {
        let cred = generate_credential("my description", false, "root");
        assert_eq!(cred.description, "my description");
        assert!(cred.active);
        assert!(!cred.admin);
        let elapsed = chrono::Utc::now() - cred.created_at;
        assert!(elapsed.num_seconds() < 2);
    }

    #[test]
    fn credential_admin_flag() {
        let admin = generate_credential("admin", true, "root");
        assert!(admin.admin);
        let user = generate_credential("user", false, "root");
        assert!(!user.admin);
    }

    #[test]
    fn generated_credentials_are_unique() {
        let cred1 = generate_credential("test", false, "root");
        let cred2 = generate_credential("test", false, "root");
        assert_ne!(cred1.access_key_id, cred2.access_key_id);
        assert_ne!(cred1.secret_access_key, cred2.secret_access_key);
    }
}
