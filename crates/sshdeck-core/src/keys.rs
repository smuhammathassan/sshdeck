//! SSH key generation and OpenSSH serialisation.
//!
//! Reuses `russh::keys::ssh_key` (russh's pinned `ssh-key`) for every key type
//! and format, so there is exactly one `ssh-key` in the tree. The only extra
//! dependency is `rand`, pinned to the same 0.10 line russh uses, because
//! `ssh_key::PrivateKey::random` needs a `CryptoRng` and russh does not
//! re-export one (its `ssh-key` features do not enable `ssh-key/getrandom`).

use std::path::Path;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};

/// Key algorithms we generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// `ssh-ed25519` — the default.
    Ed25519,
    /// `ssh-rsa` (RSASSA-PKCS1v15, SHA-256 signatures). Generation uses
    /// `ssh-key`'s fixed 4096-bit default and is CPU-heavy, so callers should run
    /// it on a worker thread.
    Rsa,
}

/// A key generation or serialisation failure.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("key generation failed: {0}")]
    Generate(String),
    #[error("key encoding failed: {0}")]
    Encode(String),
    #[error("key parsing failed: {0}")]
    Parse(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Generates a new, unencrypted key.
///
/// `ponytail:` encryption needs `ssh_key`'s `getrandom` feature, which russh
/// does not enable; add it (or a passphrase path) when the UI needs it.
pub fn generate(kind: KeyKind) -> Result<PrivateKey, KeyError> {
    let algorithm = match kind {
        KeyKind::Ed25519 => Algorithm::Ed25519,
        KeyKind::Rsa => Algorithm::Rsa {
            hash: Some(HashAlg::Sha256),
        },
    };
    PrivateKey::random(&mut rand::rng(), algorithm)
        .map_err(|err| KeyError::Generate(err.to_string()))
}

/// OpenSSH private key text (`-----BEGIN OPENSSH PRIVATE KEY-----`).
pub fn to_openssh_private(key: &PrivateKey) -> Result<String, KeyError> {
    key.to_openssh(LineEnding::LF)
        .map(|pem| pem.as_str().to_string())
        .map_err(|err| KeyError::Encode(err.to_string()))
}

/// Reads an OpenSSH (or PEM/PKCS#8) private key back.
pub fn from_openssh_private(pem: &str) -> Result<PrivateKey, KeyError> {
    PrivateKey::from_openssh(pem).map_err(|err| KeyError::Parse(err.to_string()))
}

/// The public half in `authorized_keys` line format
/// (`ssh-ed25519 AAAA... comment`).
pub fn authorized_key(key: &PrivateKey) -> Result<String, KeyError> {
    key.public_key()
        .to_openssh()
        .map_err(|err| KeyError::Encode(err.to_string()))
}

/// `SHA256:...` fingerprint of a public key.
pub fn fingerprint(public: &PublicKey) -> String {
    public.fingerprint(HashAlg::Sha256).to_string()
}

/// Writes a private key with owner-only permissions.
pub fn save_private_key(path: impl AsRef<Path>, pem: &str) -> Result<(), KeyError> {
    let path = path.as_ref();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pem.as_bytes())?;
    restrict(path)?;
    Ok(())
}

/// Private keys must not be group/world readable.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), KeyError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_round_trips_through_openssh_pem() {
        let key = generate(KeyKind::Ed25519).expect("generate");
        let pem = to_openssh_private(&key).expect("encode");
        assert!(pem.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));

        let parsed = from_openssh_private(&pem).expect("parse");
        assert_eq!(parsed.public_key(), key.public_key());

        let authorized = authorized_key(&parsed).expect("public");
        assert!(authorized.starts_with("ssh-ed25519 "), "got {authorized}");
        assert!(fingerprint(parsed.public_key()).starts_with("SHA256:"));
    }

    #[test]
    #[ignore = "ssh-key generates 4096-bit RSA, which is too slow for CI; run with --ignored"]
    fn rsa_round_trips_and_reports_an_ssh_rsa_authorized_key() {
        let key = generate(KeyKind::Rsa).expect("generate");
        let pem = to_openssh_private(&key).expect("encode");
        let parsed = from_openssh_private(&pem).expect("parse");
        assert_eq!(parsed.public_key(), key.public_key());

        let authorized = authorized_key(&parsed).expect("public");
        assert!(authorized.starts_with("ssh-rsa "), "got {authorized}");
    }

    #[test]
    fn saving_a_private_key_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("sshdeck-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("id_ed25519");

        let key = generate(KeyKind::Ed25519).expect("generate");
        let pem = to_openssh_private(&key).expect("encode");
        save_private_key(&path, &pem).expect("save");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), pem);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
