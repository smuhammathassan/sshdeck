//! Secret storage for sshdeck.
//!
//! Secrets are encrypted at rest and referenced by opaque id, so the host
//! inventory never contains a password. No UI dependency, no network, no gpui.
//!
//! # Key management
//!
//! A vault is opened in one of two modes, recorded in the file as `key_source`:
//!
//! - **keychain** (preferred): a random 256-bit master secret is generated once
//!   and kept in the OS keychain (`Security.framework` on macOS) through the
//!   `keyring` crate, so launch never prompts. [`Vault::open`] and
//!   [`Vault::open_default`] take this path.
//! - **passphrase** (fallback): the master secret is a user passphrase, used
//!   when the keychain is unavailable. Call [`Vault::open_with_passphrase`].
//!   The passphrase is never stored. A keychain vault cannot be recovered with
//!   a passphrase (its master secret exists only in the keychain).
//!
//! Either master secret is stretched with Argon2id — a per-vault random salt
//! and the KDF parameters live in the file — into a 256-bit AEAD key. Every
//! record is sealed with ChaCha20-Poly1305 under a fresh random nonce, and the
//! record id is bound as associated data so a ciphertext cannot be swapped
//! between ids. The format is versioned and self-describing; a wrong key or a
//! tampered record fails authentication instead of returning garbage.

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// On-disk format version. Bump only with a migration path.
const VAULT_VERSION: u32 = 1;
/// Only Argon2id is supported; stored so the format stays self-describing.
const KDF_ALGORITHM: &str = "argon2id";
/// OWASP-recommended Argon2id parameters (19 MiB, 2 passes, 1 lane).
const ARGON2_M_COST: u32 = 19_456;
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
/// Keychain service name; the account is derived from the vault path.
const KEYCHAIN_SERVICE: &str = "sshdeck";
/// Binds a ciphertext to its record id and to this format version.
const AAD_PREFIX: &str = "sshdeck-vault-v1:";

/// Whether a vault's master secret comes from the OS keychain or a passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeySource {
    Keychain,
    Passphrase,
}

/// Argon2id parameters, persisted so a future default change can still read
/// old vaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KdfParams {
    algorithm: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            algorithm: KDF_ALGORITHM.to_string(),
            m_cost: ARGON2_M_COST,
            t_cost: ARGON2_T_COST,
            p_cost: ARGON2_P_COST,
        }
    }
}

/// One sealed secret: a fresh nonce plus AEAD ciphertext (tag included).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    ciphertext: String,
}

/// Serialized vault. `salt` and the nonce/ciphertext fields are base64.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    key_source: KeySource,
    kdf: KdfParams,
    salt: String,
    #[serde(default)]
    secrets: BTreeMap<String, Sealed>,
}

/// Errors from opening, reading, or writing a vault.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("could not read vault at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write vault at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("vault at {path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("vault format version {0} is not supported")]
    UnsupportedVersion(u32),
    #[error("vault uses unsupported KDF `{0}`")]
    UnsupportedKdf(String),
    #[error("vault is malformed: {0}")]
    Malformed(String),
    #[error("key derivation failed: {0}")]
    Kdf(#[from] argon2::Error),
    #[error("system randomness is unavailable: {0}")]
    Random(#[from] getrandom::Error),
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed: wrong key or tampered record")]
    Decrypt,
    #[error("OS keychain error: {0}")]
    Keychain(#[source] keyring::v1::Error),
    #[error("OS keychain is unavailable; open the vault with a passphrase instead")]
    KeychainUnavailable,
    #[error("this vault is keyed by the OS keychain; passphrase fallback cannot open it")]
    KeySourceMismatch,
    #[error("this vault needs its passphrase: use Vault::open_with_passphrase")]
    PassphraseRequired,
    #[error("no keychain entry exists for this vault")]
    MissingKeychainEntry,
}

/// Encrypted secret store backed by a single JSON file.
///
/// Field access is intentionally private: the master key lives in `key` and is
/// zeroized on drop, and the caller only sees decrypted values on request.
pub struct Vault {
    path: PathBuf,
    /// 256-bit AEAD key derived from the master secret. Zeroized on drop.
    key: Zeroizing<[u8; KEY_LEN]>,
    file: VaultFile,
}

impl Vault {
    /// Opens a vault at `path` using the OS keychain for the master secret.
    ///
    /// A missing file is not an error: it yields an empty vault that is only
    /// written when [`save`](Self::save) is called, matching `HostStore::load`.
    /// A vault previously created with a passphrase is not opened here — call
    /// [`open_with_passphrase`](Self::open_with_passphrase) instead, which the
    /// [`PassphraseRequired`](VaultError::PassphraseRequired) error signals.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let path = path.into();
        match read_optional(&path)? {
            Some(bytes) => {
                let file = parse(&path, &bytes)?;
                match file.key_source {
                    KeySource::Passphrase => Err(VaultError::PassphraseRequired),
                    KeySource::Keychain => {
                        let account = keychain_account(&path);
                        let secret =
                            keychain_get(&account)?.ok_or(VaultError::MissingKeychainEntry)?;
                        Self::unlock(path, file, secret.as_slice())
                    }
                }
            }
            None => {
                if keyring::v1::Entry::store_status().is_err() {
                    return Err(VaultError::KeychainUnavailable);
                }
                let secret = keychain_get_or_create(&keychain_account(&path))?;
                Self::create(path, KeySource::Keychain, secret.as_slice())
            }
        }
    }

    /// Opens the default vault (`~/Library/Application Support/sshdeck/vault.json`
    /// on macOS, `$XDG_CONFIG_HOME/sshdeck/vault.json` elsewhere).
    pub fn open_default() -> Result<Self, VaultError> {
        Self::open(Self::default_path())
    }

    /// Opens a vault with a passphrase-derived master secret.
    ///
    /// This is the fallback when the keychain is unavailable. On a missing file
    /// it creates an empty passphrase vault. It refuses a keychain vault, whose
    /// master secret is not derivable from a passphrase.
    pub fn open_with_passphrase(
        path: impl Into<PathBuf>,
        passphrase: impl AsRef<[u8]>,
    ) -> Result<Self, VaultError> {
        let path = path.into();
        let passphrase = passphrase.as_ref();
        match read_optional(&path)? {
            Some(bytes) => {
                let file = parse(&path, &bytes)?;
                match file.key_source {
                    KeySource::Keychain => Err(VaultError::KeySourceMismatch),
                    KeySource::Passphrase => Self::unlock(path, file, passphrase),
                }
            }
            None => Self::create(path, KeySource::Passphrase, passphrase),
        }
    }

    /// Default vault location, mirroring `HostStore::default_path()`.
    pub fn default_path() -> PathBuf {
        let base = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Library/Application Support"))
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        };
        base.unwrap_or_else(|| PathBuf::from("."))
            .join("sshdeck")
            .join("vault.json")
    }

    /// Inserts or replaces the secret stored under `id`.
    pub fn set(&mut self, id: impl Into<String>, secret: &str) -> Result<(), VaultError> {
        let id = id.into();
        let sealed = seal(&self.key, &id, secret.as_bytes())?;
        self.file.secrets.insert(id, sealed);
        Ok(())
    }

    /// Returns the secret for `id`, or `None` if there is no such record.
    ///
    /// The value is wrapped in [`Zeroizing`] so the plaintext is wiped when the
    /// caller drops it; decryption also authenticates the record, so a wrong
    /// key or a tampered ciphertext is an error rather than garbage.
    pub fn get(&self, id: &str) -> Result<Option<Zeroizing<String>>, VaultError> {
        match self.file.secrets.get(id) {
            Some(sealed) => Ok(Some(unseal(&self.key, id, sealed)?)),
            None => Ok(None),
        }
    }

    /// Removes `id`, returning whether it was present.
    pub fn remove(&mut self, id: &str) -> Result<bool, VaultError> {
        Ok(self.file.secrets.remove(id).is_some())
    }

    /// Writes the vault atomically (temp file + rename), so an interrupted write
    /// cannot truncate it. On Unix the file mode is restricted to `0600`.
    pub fn save(&self) -> Result<(), VaultError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| VaultError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut payload =
            serde_json::to_vec_pretty(&self.file).map_err(|source| VaultError::Write {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        payload.push(b'\n');

        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, &payload).map_err(|source| VaultError::Write {
            path: temp.clone(),
            source,
        })?;
        restrict_permissions(&temp)?;
        std::fs::rename(&temp, &self.path).map_err(|source| VaultError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Ids of every stored record, in stable order.
    pub fn ids(&self) -> impl Iterator<Item = &str> + '_ {
        self.file.secrets.keys().map(String::as_str)
    }

    /// Derives the AEAD key from `secret`, then builds the in-memory vault.
    fn unlock(path: PathBuf, file: VaultFile, secret: &[u8]) -> Result<Self, VaultError> {
        let salt = decode_salt(&file)?;
        let key = derive_key(secret, &salt, &file.kdf)?;
        Ok(Self { path, key, file })
    }

    /// Builds a fresh, empty vault with a new random salt.
    fn create(path: PathBuf, key_source: KeySource, secret: &[u8]) -> Result<Self, VaultError> {
        let mut salt = [0u8; SALT_LEN];
        getrandom::fill(&mut salt)?;
        let kdf = KdfParams::default();
        let key = derive_key(secret, &salt, &kdf)?;
        Ok(Self {
            path,
            key,
            file: VaultFile {
                version: VAULT_VERSION,
                key_source,
                kdf,
                salt: b64_encode(&salt),
                secrets: BTreeMap::new(),
            },
        })
    }
}

/// Argon2id-stretches a master secret into the AEAD key.
fn derive_key(
    secret: &[u8],
    salt: &[u8],
    kdf: &KdfParams,
) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    let params = Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(KEY_LEN))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon2.hash_password_into(secret, salt, key.as_mut_slice())?;
    Ok(key)
}

/// Encrypts `plaintext` for `id` under a fresh random nonce.
fn seal(key: &[u8; KEY_LEN], id: &str, plaintext: &[u8]) -> Result<Sealed, VaultError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes)?;
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let aad = aad_for(id);
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| VaultError::Encrypt)?;
    Ok(Sealed {
        nonce: b64_encode(&nonce_bytes),
        ciphertext: b64_encode(&ciphertext),
    })
}

/// Decrypts and authenticates a record, binding it to `id`.
fn unseal(key: &[u8; KEY_LEN], id: &str, sealed: &Sealed) -> Result<Zeroizing<String>, VaultError> {
    let nonce_bytes = b64_decode(&sealed.nonce)?;
    let nonce: [u8; NONCE_LEN] = nonce_bytes
        .as_slice()
        .try_into()
        .map_err(|_| VaultError::Malformed("nonce is not 12 bytes".into()))?;
    let ciphertext = b64_decode(&sealed.ciphertext)?;
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let aad = aad_for(id);
    let plaintext = cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| VaultError::Decrypt)?;
    let secret = String::from_utf8(plaintext)
        .map_err(|_| VaultError::Malformed("secret is not UTF-8".into()))?;
    Ok(Zeroizing::new(secret))
}

fn aad_for(id: &str) -> String {
    format!("{AAD_PREFIX}{id}")
}

/// Reads the file, treating "not found" as absent (first run).
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, VaultError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(VaultError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn parse(path: &Path, bytes: &[u8]) -> Result<VaultFile, VaultError> {
    let file: VaultFile = serde_json::from_slice(bytes).map_err(|source| VaultError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if file.version != VAULT_VERSION {
        return Err(VaultError::UnsupportedVersion(file.version));
    }
    if file.kdf.algorithm != KDF_ALGORITHM {
        return Err(VaultError::UnsupportedKdf(file.kdf.algorithm));
    }
    Ok(file)
}

fn decode_salt(file: &VaultFile) -> Result<Vec<u8>, VaultError> {
    let salt = b64_decode(&file.salt)?;
    if salt.len() != SALT_LEN {
        return Err(VaultError::Malformed("salt is not 16 bytes".into()));
    }
    Ok(salt)
}

fn b64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Result<Vec<u8>, VaultError> {
    Ok(STANDARD.decode(text)?)
}

/// Keychain account for a vault, scoped to its path.
fn keychain_account(path: &Path) -> String {
    format!("vault:{}", path.display())
}

/// Fetches the keychain master secret, or `None` when there is no entry.
fn keychain_get(account: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
    let entry = keyring::v1::Entry::new(KEYCHAIN_SERVICE, account).map_err(VaultError::Keychain)?;
    match entry.get_secret() {
        Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
        Err(keyring::v1::Error::NoEntry) => Ok(None),
        Err(err) => Err(VaultError::Keychain(err)),
    }
}

/// Fetches the keychain master secret, generating and storing one on first run.
fn keychain_get_or_create(account: &str) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if let Some(secret) = keychain_get(account)? {
        return Ok(secret);
    }
    let mut secret = Zeroizing::new(vec![0u8; KEY_LEN]);
    getrandom::fill(secret.as_mut_slice())?;
    let entry = keyring::v1::Entry::new(KEYCHAIN_SERVICE, account).map_err(VaultError::Keychain)?;
    entry
        .set_secret(secret.as_slice())
        .map_err(VaultError::Keychain)?;
    Ok(secret)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        VaultError::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), VaultError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sshdeck-vault-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("vault.json")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn read_json(path: &Path) -> serde_json::Value {
        let bytes = std::fs::read(path).expect("vault file exists");
        serde_json::from_slice(&bytes).expect("vault file is JSON")
    }

    #[test]
    fn round_trips_through_disk() {
        let path = temp_vault_path("round-trip");
        let mut vault =
            Vault::open_with_passphrase(&path, "correct horse").expect("create passphrase vault");
        vault.set("password:prod", "hunter2").expect("set");
        vault.set("passphrase:key", "s3cr3t").expect("set");
        assert_eq!(
            vault
                .get("password:prod")
                .expect("read")
                .expect("present")
                .as_str(),
            "hunter2"
        );

        assert!(vault.remove("passphrase:key").expect("remove"));
        assert!(!vault.remove("passphrase:key").expect("remove missing"));
        vault.save().expect("save");

        let reopened = Vault::open_with_passphrase(&path, "correct horse").expect("reopen");
        assert_eq!(
            reopened
                .get("password:prod")
                .expect("read")
                .expect("present")
                .as_str(),
            "hunter2"
        );
        assert!(reopened.get("passphrase:key").expect("read").is_none());
        assert_eq!(reopened.ids().collect::<Vec<_>>(), vec!["password:prod"]);
        cleanup(&path);
    }

    #[test]
    fn wrong_passphrase_fails_to_decrypt() {
        let path = temp_vault_path("wrong-passphrase");
        let mut vault =
            Vault::open_with_passphrase(&path, "right").expect("create passphrase vault");
        vault.set("id", "payload").expect("set");
        vault.save().expect("save");

        let wrong = Vault::open_with_passphrase(&path, "wrong").expect("wrong key still opens");
        let result = wrong.get("id");
        assert!(
            matches!(result, Err(VaultError::Decrypt)),
            "wrong passphrase must fail authentication, got {result:?}"
        );
        cleanup(&path);
    }

    #[test]
    fn tampering_with_ciphertext_is_detected() {
        let path = temp_vault_path("tamper");
        let mut vault = Vault::open_with_passphrase(&path, "pw").expect("create passphrase vault");
        vault.set("id", "payload").expect("set");
        vault.save().expect("save");

        let mut doc = read_json(&path);
        let ciphertext = doc["secrets"]["id"]["ciphertext"]
            .as_str()
            .expect("ciphertext present")
            .to_string();
        let mut bytes = b64_decode(&ciphertext).expect("ciphertext is base64");
        bytes[0] ^= 0x01;
        doc["secrets"]["id"]["ciphertext"] = serde_json::Value::String(b64_encode(&bytes));
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).expect("serialize")).expect("write");

        let vault = Vault::open_with_passphrase(&path, "pw").expect("open");
        assert!(
            matches!(vault.get("id"), Err(VaultError::Decrypt)),
            "flipped ciphertext byte must be rejected"
        );
        cleanup(&path);
    }

    #[test]
    fn missing_file_opens_as_empty() {
        let path = temp_vault_path("missing");
        let vault = Vault::open_with_passphrase(&path, "pw").expect("open missing");
        assert_eq!(vault.ids().count(), 0);
        assert!(vault.get("anything").expect("read").is_none());
        assert!(!path.exists(), "opening must not write");
        cleanup(&path);
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let path = temp_vault_path("atomic");
        let mut vault = Vault::open_with_passphrase(&path, "pw").expect("create passphrase vault");
        vault.set("id", "value").expect("set");
        vault.save().expect("save");

        assert!(path.exists(), "vault file exists");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "atomic rename must leave no .tmp behind"
        );
        cleanup(&path);
    }

    #[test]
    fn plaintext_is_never_written_to_disk() {
        let path = temp_vault_path("plaintext");
        let secret = "correct horse battery staple";
        let mut vault = Vault::open_with_passphrase(&path, "pw").expect("create passphrase vault");
        vault.set("id", secret).expect("set");
        vault.save().expect("save");

        let contents = std::fs::read_to_string(&path).expect("read vault file");
        assert!(
            !contents.contains(secret),
            "vault file leaked the plaintext secret"
        );
        cleanup(&path);
    }
}
