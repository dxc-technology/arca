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
///
/// Credentials carry no privileges of their own: what the holder may do is
/// decided by `user_id` (root has implicit full access, other users are
/// authorized through their grants).
pub fn generate_credential(description: &str, user_id: &str) -> Credential {
    Credential {
        access_key_id: random_string(KEY_CHARSET, ACCESS_KEY_LEN),
        secret_access_key: random_string(SECRET_CHARSET, SECRET_KEY_LEN),
        description: description.to_string(),
        created_at: chrono::Utc::now(),
        active: true,
        user_id: user_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_length() {
        let cred = generate_credential("test", "root");
        assert_eq!(cred.access_key_id.len(), ACCESS_KEY_LEN);
    }

    #[test]
    fn secret_key_length() {
        let cred = generate_credential("test", "root");
        assert_eq!(cred.secret_access_key.len(), SECRET_KEY_LEN);
    }

    #[test]
    fn access_key_valid_charset() {
        let cred = generate_credential("test", "root");
        assert!(cred
            .access_key_id
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn credential_fields_set() {
        let cred = generate_credential("my description", "root");
        assert_eq!(cred.description, "my description");
        assert!(cred.active);
        let elapsed = chrono::Utc::now() - cred.created_at;
        assert!(elapsed.num_seconds() < 2);
    }

    #[test]
    fn credential_is_bound_to_its_user() {
        // Privileges follow the user, so the owning user_id is the only
        // authorization-relevant field the generator sets.
        let cred = generate_credential("for alice", "alice");
        assert_eq!(cred.user_id, "alice");
    }

    #[test]
    fn generated_credentials_are_unique() {
        let cred1 = generate_credential("test", "root");
        let cred2 = generate_credential("test", "root");
        assert_ne!(cred1.access_key_id, cred2.access_key_id);
        assert_ne!(cred1.secret_access_key, cred2.secret_access_key);
    }
}
