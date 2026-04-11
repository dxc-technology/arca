//! Master key (KEK) and data encryption key (DEK) management.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};

/// A loaded master key (KEK) with its derived key_id.
#[derive(Clone)]
pub struct MasterKey {
    /// Raw 32-byte key material.
    key_bytes: Vec<u8>,
    /// First 8 hex chars of SHA-256(key_bytes).
    key_id: String,
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKey")
            .field("key_id", &self.key_id)
            .finish()
    }
}

impl MasterKey {
    /// Creates a MasterKey from raw 32-byte key material.
    pub fn from_bytes(key_bytes: &[u8]) -> Result<Self, &'static str> {
        if key_bytes.len() != 32 {
            return Err("master key must be exactly 32 bytes");
        }
        let key_id = derive_key_id(key_bytes);
        Ok(Self {
            key_bytes: key_bytes.to_vec(),
            key_id,
        })
    }

    /// Creates a MasterKey from a base64-encoded string.
    pub fn from_base64(encoded: &str) -> Result<Self, String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("invalid base64: {e}"))?;
        Self::from_bytes(&bytes).map_err(|e| e.to_string())
    }

    /// Returns the key ID (first 8 hex chars of SHA-256(key)).
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Wraps (encrypts) a DEK using this master key.
    /// Returns (encrypted_dek, nonce) both as raw bytes.
    pub fn wrap_dek(&self, dek: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        let rng = SystemRandom::new();
        let mut nonce_bytes = [0u8; 12];
        rng.fill(&mut nonce_bytes)
            .map_err(|_| "failed to generate nonce")?;

        let unbound = UnboundKey::new(&AES_256_GCM, &self.key_bytes)
            .map_err(|_| "failed to create wrapping key")?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut in_out = dek.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| "DEK wrapping failed")?;

        Ok((in_out, nonce_bytes.to_vec()))
    }

    /// Unwraps (decrypts) an encrypted DEK using this master key.
    pub fn unwrap_dek(&self, encrypted_dek: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
        if nonce_bytes.len() != 12 {
            return Err("nonce must be 12 bytes".to_string());
        }
        let unbound = UnboundKey::new(&AES_256_GCM, &self.key_bytes)
            .map_err(|_| "failed to create unwrapping key")?;
        let key = LessSafeKey::new(unbound);

        let mut nonce_arr = [0u8; 12];
        nonce_arr.copy_from_slice(nonce_bytes);
        let nonce = Nonce::assume_unique_for_key(nonce_arr);

        let mut in_out = encrypted_dek.to_vec();
        let plaintext = key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .map_err(|_| "DEK unwrapping failed (wrong key or corrupted data)")?;

        Ok(plaintext.to_vec())
    }
}

/// Generates a random 32-byte DEK.
pub fn generate_dek() -> Result<[u8; 32], &'static str> {
    let rng = SystemRandom::new();
    let mut dek = [0u8; 32];
    rng.fill(&mut dek).map_err(|_| "failed to generate DEK")?;
    Ok(dek)
}

/// Generates a random 4-byte nonce prefix for chunk encryption.
pub fn generate_nonce_prefix() -> Result<[u8; 4], &'static str> {
    let rng = SystemRandom::new();
    let mut prefix = [0u8; 4];
    rng.fill(&mut prefix)
        .map_err(|_| "failed to generate nonce prefix")?;
    Ok(prefix)
}

/// Derives the key_id: first 8 hex chars of SHA-256(key).
fn derive_key_id(key_bytes: &[u8]) -> String {
    use ring::digest;
    let hash = digest::digest(&digest::SHA256, key_bytes);
    hex::encode(&hash.as_ref()[..4])
}

/// Builds a 12-byte nonce from a 4-byte prefix and an 8-byte big-endian chunk counter.
pub fn build_nonce(prefix: &[u8; 4], chunk_index: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(prefix);
    nonce[4..12].copy_from_slice(&chunk_index.to_be_bytes());
    nonce
}

/// Encrypts a single chunk of plaintext using AES-256-GCM.
/// Returns ciphertext + tag (appended).
pub fn encrypt_chunk(
    key: &LessSafeKey,
    nonce_bytes: &[u8; 12],
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
    let mut in_out = plaintext.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| "chunk encryption failed")?;
    Ok(in_out)
}

/// Decrypts a single chunk (ciphertext + tag) using AES-256-GCM.
/// Returns plaintext.
pub fn decrypt_chunk(
    key: &LessSafeKey,
    nonce_bytes: &[u8; 12],
    ciphertext_and_tag: &[u8],
) -> Result<Vec<u8>, String> {
    let nonce = Nonce::assume_unique_for_key(*nonce_bytes);
    let mut in_out = ciphertext_and_tag.to_vec();
    let plaintext_len = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| "chunk decryption failed (corrupted or wrong key)")?
        .len();
    in_out.truncate(plaintext_len);
    Ok(in_out)
}

/// Creates an AES-256-GCM LessSafeKey from raw key bytes.
pub fn make_aead_key(key_bytes: &[u8]) -> Result<LessSafeKey, String> {
    let unbound = UnboundKey::new(&AES_256_GCM, key_bytes)
        .map_err(|_| "invalid AES-256-GCM key")?;
    Ok(LessSafeKey::new(unbound))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_master_key() -> MasterKey {
        let key_bytes = [0x42u8; 32];
        MasterKey::from_bytes(&key_bytes).unwrap()
    }

    #[test]
    fn key_id_derivation_consistent() {
        let mk1 = test_master_key();
        let mk2 = test_master_key();
        assert_eq!(mk1.key_id(), mk2.key_id());
        assert_eq!(mk1.key_id().len(), 8);
    }

    #[test]
    fn different_keys_different_ids() {
        let mk1 = MasterKey::from_bytes(&[0x42u8; 32]).unwrap();
        let mk2 = MasterKey::from_bytes(&[0x43u8; 32]).unwrap();
        assert_ne!(mk1.key_id(), mk2.key_id());
    }

    #[test]
    fn dek_wrap_unwrap_roundtrip() {
        let mk = test_master_key();
        let dek = generate_dek().unwrap();
        let (wrapped, nonce) = mk.wrap_dek(&dek).unwrap();
        let unwrapped = mk.unwrap_dek(&wrapped, &nonce).unwrap();
        assert_eq!(unwrapped, dek);
    }

    #[test]
    fn wrong_key_unwrap_fails() {
        let mk1 = MasterKey::from_bytes(&[0x42u8; 32]).unwrap();
        let mk2 = MasterKey::from_bytes(&[0x43u8; 32]).unwrap();
        let dek = generate_dek().unwrap();
        let (wrapped, nonce) = mk1.wrap_dek(&dek).unwrap();
        assert!(mk2.unwrap_dek(&wrapped, &nonce).is_err());
    }

    #[test]
    fn chunk_encrypt_decrypt_roundtrip() {
        let dek = generate_dek().unwrap();
        let key = make_aead_key(&dek).unwrap();
        let nonce = build_nonce(&[1, 2, 3, 4], 0);
        let plaintext = b"hello, encryption!";
        let encrypted = encrypt_chunk(&key, &nonce, plaintext).unwrap();
        assert_ne!(&encrypted[..plaintext.len()], plaintext);
        let decrypted = decrypt_chunk(&key, &nonce, &encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let dek = generate_dek().unwrap();
        let key = make_aead_key(&dek).unwrap();
        let nonce = build_nonce(&[1, 2, 3, 4], 0);
        let mut encrypted = encrypt_chunk(&key, &nonce, b"test").unwrap();
        encrypted[0] ^= 0xFF; // corrupt
        assert!(decrypt_chunk(&key, &nonce, &encrypted).is_err());
    }

    #[test]
    fn build_nonce_format() {
        let nonce = build_nonce(&[0xAA, 0xBB, 0xCC, 0xDD], 42);
        assert_eq!(&nonce[..4], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(&nonce[4..], &42u64.to_be_bytes());
    }

    #[test]
    fn from_base64_valid() {
        use base64::Engine;
        let key_bytes = [0x55u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(key_bytes);
        let mk = MasterKey::from_base64(&encoded).unwrap();
        assert_eq!(mk.key_id().len(), 8);
    }

    #[test]
    fn from_base64_wrong_length() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(MasterKey::from_base64(&encoded).is_err());
    }

    #[test]
    fn from_bytes_wrong_length() {
        assert!(MasterKey::from_bytes(&[0u8; 16]).is_err());
    }

    #[test]
    fn generate_dek_produces_32_bytes() {
        let dek = generate_dek().unwrap();
        assert_eq!(dek.len(), 32);
    }

    #[test]
    fn generate_nonce_prefix_produces_4_bytes() {
        let prefix = generate_nonce_prefix().unwrap();
        assert_eq!(prefix.len(), 4);
    }
}
