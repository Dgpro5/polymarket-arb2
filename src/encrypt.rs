// Argon2id key derivation + AES-256-GCM authenticated encryption.
//
// First run:  prompts for private key + password, encrypts to data/key.enc
// Later runs: prompts for password only, decrypts and returns the key.
//
// File format: [16-byte salt][12-byte nonce][ciphertext + 16-byte GCM tag]

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow};
use argon2::Argon2;
use std::fs;
use std::path::Path;

const ENCRYPTED_KEY_PATH: &str = "data/key.enc";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

/// Interactive entry point. Checks if an encrypted key file exists.
/// - If yes: asks for password, decrypts, returns the private key.
/// - If no:  asks for private key + password (with confirmation), encrypts and stores.
pub fn get_private_key() -> Result<String> {
    if Path::new(ENCRYPTED_KEY_PATH).exists() {
        let password = rpassword::prompt_password("Enter password to decrypt private key: ")
            .context("failed to read password")?;
        decrypt(&password)
    } else {
        eprintln!("No encrypted key found — first-time setup.");
        let key = rpassword::prompt_password("Enter your private key: ")
            .context("failed to read private key")?;
        let key = key.trim().trim_start_matches("0x").to_string();
        if key.is_empty() {
            return Err(anyhow!("Private key cannot be empty"));
        }

        let password = rpassword::prompt_password("Create a password for encryption: ")
            .context("failed to read password")?;
        let confirm = rpassword::prompt_password("Confirm password: ")
            .context("failed to read confirmation")?;
        if password != confirm {
            return Err(anyhow!("Passwords do not match"));
        }
        if password.len() < 8 {
            return Err(anyhow!("Password must be at least 8 characters"));
        }

        encrypt_and_store(&key, &password)?;
        eprintln!("Private key encrypted and stored at {ENCRYPTED_KEY_PATH}");
        Ok(key)
    }
}

fn encrypt_and_store(private_key: &str, password: &str) -> Result<()> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|e| anyhow!("RNG error: {e}"))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| anyhow!("RNG error: {e}"))?;

    let key = derive_key(password, &salt)?;

    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| anyhow!("cipher init failed: {e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, private_key.as_bytes())
        .map_err(|e| anyhow!("encryption failed: {e}"))?;

    // salt ‖ nonce ‖ ciphertext+tag
    let mut data = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    data.extend_from_slice(&salt);
    data.extend_from_slice(&nonce_bytes);
    data.extend_from_slice(&ciphertext);

    if let Some(parent) = Path::new(ENCRYPTED_KEY_PATH).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(ENCRYPTED_KEY_PATH, &data).context("write encrypted key file")?;

    Ok(())
}

fn decrypt(password: &str) -> Result<String> {
    let data = fs::read(ENCRYPTED_KEY_PATH).context("read encrypted key file")?;

    if data.len() < SALT_LEN + NONCE_LEN + 1 {
        return Err(anyhow!("Encrypted key file is corrupt (too short)"));
    }

    let salt = &data[..SALT_LEN];
    let nonce_bytes = &data[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &data[SALT_LEN + NONCE_LEN..];

    let key = derive_key(password, salt)?;

    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| anyhow!("cipher init failed: {e}"))?;
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow!("Decryption failed — wrong password or corrupt file"))?;

    String::from_utf8(plaintext).context("decrypted key is not valid UTF-8")
}

/// Argon2id: memory-hard KDF resistant to GPU/ASIC brute-force.
/// Derives a 256-bit key from password + salt.
fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("Argon2 key derivation failed: {e}"))?;
    Ok(key)
}
