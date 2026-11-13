//! Password hashing, random tokens, at-rest encryption, and TOTP.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, bail};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};
use totp_rs::{Algorithm, Secret, TOTP};

// ---- portal password hashing (argon2id) ----

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("hash: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ---- random tokens for cookies / pending logins ----

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// A long random password for a Windows account the user never types directly.
pub fn random_windows_password() -> String {
    let mut b = [0u8; 18];
    OsRng.fill_bytes(&mut b);
    // URL-safe-ish, no padding; mixes case and digits.
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

// ---- at-rest encryption of the Windows password (AES-256-GCM) ----

fn gcm_key(secret: &str) -> Key<Aes256Gcm> {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    let digest = h.finalize();
    Key::<Aes256Gcm>::clone_from_slice(&digest)
}

pub fn encrypt_at_rest(secret: &str, plaintext: &str) -> anyhow::Result<String> {
    let cipher = Aes256Gcm::new(&gcm_key(secret));
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|_| anyhow!("encrypt failed"))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ct);
    Ok(STANDARD.encode(out))
}

pub fn decrypt_at_rest(secret: &str, blob: &str) -> anyhow::Result<String> {
    let data = STANDARD.decode(blob)?;
    if data.len() < 12 {
        bail!("ciphertext too short");
    }
    let (nonce_bytes, ct) = data.split_at(12);
    let cipher = Aes256Gcm::new(&gcm_key(secret));
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ct)
        .map_err(|_| anyhow!("decrypt failed"))?;
    Ok(String::from_utf8(pt)?)
}

// ---- TOTP (2-step login) ----

pub fn new_totp_secret() -> String {
    Secret::generate_secret().to_encoded().to_string()
}

/// Build a TOTP from a base32 secret. `account` is only metadata (issuer/account
/// shown in the authenticator app); it does not affect the generated codes.
fn totp(secret_b32: &str, account: &str) -> anyhow::Result<TOTP> {
    let bytes = Secret::Encoded(secret_b32.to_string())
        .to_bytes()
        .map_err(|_| anyhow!("bad totp secret"))?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some("Access Portal".to_string()),
        account.to_string(),
    )
    .map_err(|e| anyhow!("totp: {e}"))
}

pub fn verify_totp(secret_b32: &str, code: &str) -> bool {
    match totp(secret_b32, "user") {
        Ok(t) => t.check_current(code.trim()).unwrap_or(false),
        Err(_) => false,
    }
}

/// otpauth:// URL so the user can add the secret to an authenticator app.
pub fn totp_provisioning_uri(secret_b32: &str, account: &str) -> anyhow::Result<String> {
    Ok(totp(secret_b32, account)?.get_url())
}
