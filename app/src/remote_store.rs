//! Password-encrypted remote CCS connection, stored beside app preferences.
use anyhow::{Context, Result, ensure};
use ring::{
    aead, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, path::Path};

const MAGIC: &[u8; 4] = b"CCS1";
const ROUNDS: u32 = 600_000;
const AAD: &[u8] = b"ccs.remote.v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    pub url: String,
    pub token: String,
}

fn key(password: &str, salt: &[u8]) -> Result<aead::LessSafeKey> {
    ensure!(password.len() >= 8, "Use a password of at least 8 characters");
    let mut bytes = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(ROUNDS).unwrap(),
        salt,
        password.as_bytes(),
        &mut bytes,
    );
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, &bytes)
        .map_err(|_| anyhow::anyhow!("Could not prepare encryption key"))?;
    bytes.fill(0);
    Ok(aead::LessSafeKey::new(key))
}

pub fn save(path: &Path, password: &str, remote: &Remote) -> Result<()> {
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rng.fill(&mut salt).map_err(|_| anyhow::anyhow!("Could not generate encryption salt"))?;
    rng.fill(&mut nonce).map_err(|_| anyhow::anyhow!("Could not generate encryption nonce"))?;
    let key = key(password, &salt)?;
    let mut body = serde_json::to_vec(remote)?;
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(AAD),
        &mut body,
    )
    .map_err(|_| anyhow::anyhow!("Could not encrypt remote connection"))?;
    let mut bytes = Vec::with_capacity(4 + salt.len() + nonce.len() + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&salt);
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&body);
    ccs::fsx::write_atomic(path, &bytes, 0o600)?;
    Ok(())
}

pub fn open(path: &Path, password: &str) -> Result<Remote> {
    let mut bytes = std::fs::read(path).context("Read saved remote connection")?;
    ensure!(
        bytes.len() >= 4 + 16 + 12 + 16 && bytes.len() <= 65536 && bytes.starts_with(MAGIC),
        "Invalid saved remote connection"
    );
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&bytes[20..32]);
    let key = key(password, &bytes[4..20])?;
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(AAD),
            &mut bytes[32..],
        )
        .map_err(|_| anyhow::anyhow!("Wrong password or damaged remote connection"))?;
    Ok(serde_json::from_slice(plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_connection_is_encrypted_and_authenticated() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("ccs-remote-{}.enc", std::process::id()));
        let remote =
            Remote { url: "https://example.com:4142".into(), token: "private-token".into() };
        save(&path, "correct-horse-battery", &remote).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!bytes.windows(remote.token.len()).any(|part| part == remote.token.as_bytes()));
        assert_eq!(open(&path, "correct-horse-battery").unwrap(), remote);
        assert!(open(&path, "wrong-password").is_err());
        let mut corrupt = bytes;
        *corrupt.last_mut().unwrap() ^= 1;
        std::fs::write(&path, corrupt).unwrap();
        assert!(open(&path, "correct-horse-battery").is_err());
        std::fs::remove_file(path).unwrap();
    }
}
