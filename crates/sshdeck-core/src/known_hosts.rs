//! Known-hosts management with explicit, caller-driven trust.
//!
//! The transport ([`crate::session`]) fails closed on unknown and changed host
//! keys. This module is the *only* way to change that, and every change is a
//! deliberate call the UI makes after showing the user a fingerprint:
//!
//! * [`learn`] adds a host key the caller has confirmed. If the host is already
//!   recorded with the same key it is a no-op (no duplicate lines). If the host
//!   is recorded with a **different** key it returns
//!   [`KnownHostsError::KeyChanged`] carrying both fingerprints and writes
//!   nothing.
//! * [`trust_changed`] replaces a changed key, but only when the caller passes
//!   the old fingerprint it showed the user — a mismatch is refused.
//!
//! Line format and hashed-entry matching come from `russh::keys::known_hosts`,
//! so `[host]:port` entries and `|1|...` hashed entries behave as OpenSSH does.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use russh::keys::known_hosts::{known_host_keys_path, learn_known_hosts_path};
use russh::keys::{check_known_hosts_path, Error as KeysError, HashAlg, PublicKey};

/// A failure while reading or updating `known_hosts`.
#[derive(Debug, thiserror::Error)]
pub enum KnownHostsError {
    #[error("known_hosts: {0}")]
    Keys(#[from] KeysError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the host key for {host}:{port} changed (recorded {old}, offered {new}); \
         call trust_changed after the user confirms"
    )]
    KeyChanged {
        host: String,
        port: u16,
        old: String,
        new: String,
    },
    #[error(
        "the recorded key for {host}:{port} ({recorded}) does not match the confirmed \
         fingerprint {expected}"
    )]
    ConfirmationMismatch {
        host: String,
        port: u16,
        recorded: String,
        expected: String,
    },
}

/// Outcome of [`learn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Learned {
    /// The key was appended.
    Added,
    /// The same key was already recorded; nothing was written.
    AlreadyKnown,
}

/// The default `~/.ssh/known_hosts`, when `HOME` is set.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ssh").join("known_hosts"))
}

/// `SHA256:...` fingerprint, as shown to the user before trusting a key.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Whether `key` is the recorded key for `host:port`.
pub fn known(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: impl AsRef<Path>,
) -> Result<bool, KnownHostsError> {
    Ok(check_known_hosts_path(host, port, key, path.as_ref())?)
}

/// Adds a user-confirmed host key. Never overwrites a different recorded key.
pub fn learn(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: impl AsRef<Path>,
) -> Result<Learned, KnownHostsError> {
    let path = path.as_ref();
    match check_known_hosts_path(host, port, key, path) {
        Ok(true) => Ok(Learned::AlreadyKnown),
        Ok(false) => {
            learn_known_hosts_path(host, port, key, path)?;
            restrict(path)?;
            Ok(Learned::Added)
        }
        Err(KeysError::KeyChanged { .. }) => {
            let recorded: Vec<String> = known_host_keys_path(host, port, path)?
                .iter()
                .map(|(_, recorded)| fingerprint(recorded))
                .collect();
            Err(KnownHostsError::KeyChanged {
                host: host.to_string(),
                port,
                old: recorded.join(", "),
                new: fingerprint(key),
            })
        }
        Err(err) => Err(err.into()),
    }
}

/// Replaces a changed host key, confirming against the fingerprint the user saw.
///
/// `expected_old_fingerprint` must match a key currently recorded for the host;
/// otherwise nothing is written and the call fails, so a stale confirmation
/// cannot silently trust an unrelated key.
pub fn trust_changed(
    host: &str,
    port: u16,
    expected_old_fingerprint: &str,
    key: &PublicKey,
    path: impl AsRef<Path>,
) -> Result<(), KnownHostsError> {
    let path = path.as_ref();
    let existing = known_host_keys_path(host, port, path)?;
    let recorded: Vec<String> = existing
        .iter()
        .map(|(_, recorded)| fingerprint(recorded))
        .collect();
    if !recorded.iter().any(|line| line == expected_old_fingerprint) {
        return Err(KnownHostsError::ConfirmationMismatch {
            host: host.to_string(),
            port,
            recorded: recorded.join(", "),
            expected: expected_old_fingerprint.to_string(),
        });
    }

    // Drop every line currently recorded for this host, then append the new one.
    // `known_host_keys_path` numbers non-comment lines only, so map its logical
    // line numbers back onto raw file lines before removing them.
    let drop_lines: HashSet<usize> = existing.iter().map(|(line, _)| *line).collect();
    let raw = read_lines(path)?;
    let mut logical = 0usize;
    let mut drop_raw: HashSet<usize> = HashSet::new();
    for (index, line) in raw.iter().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        logical += 1;
        if drop_lines.contains(&logical) {
            drop_raw.insert(index);
        }
    }
    let kept: Vec<String> = raw
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !drop_raw.contains(index))
        .map(|(_, line)| line)
        .collect();
    write_lines(path, &kept)?;
    learn_known_hosts_path(host, port, key, path)?;
    restrict(path)?;
    Ok(())
}

fn read_lines(path: &Path) -> Result<Vec<String>, KnownHostsError> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.lines().map(str::to_string).collect())
}

fn write_lines(path: &Path, lines: &[String]) -> Result<(), KnownHostsError> {
    let mut payload = String::new();
    for line in lines {
        payload.push_str(line);
        payload.push('\n');
    }
    std::fs::write(path, payload)?;
    Ok(())
}

/// OpenSSH only warns about a group/world-writable `known_hosts`; 0600 is the
/// safe, boring choice (and no private material is ever stored here anyway).
#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), KnownHostsError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), KnownHostsError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ed25519 public keys lifted from russh's own known_hosts tests.
    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAILagOJFgwaMNhBWQINinKOXmqS4Gh5NgxgriXwdOoINJ";

    fn key(base64: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(base64).expect("valid key")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sshdeck-known-hosts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn learn_appends_and_suppresses_duplicates() {
        let dir = temp_dir("learn");
        let path = dir.join("known_hosts");
        let first = key(KEY_A);

        assert_eq!(
            learn("example.com", 22, &first, &path).expect("learn"),
            Learned::Added
        );
        assert_eq!(
            learn("example.com", 22, &first, &path).expect("learn"),
            Learned::AlreadyKnown
        );

        let lines = read_lines(&path).expect("read");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("example.com "))
                .count(),
            1
        );
        assert!(known("example.com", 22, &first, &path).expect("known"));

        // A non-default port uses the `[host]:port` form.
        assert_eq!(
            learn("example.com", 2222, &first, &path).expect("learn"),
            Learned::Added
        );
        assert!(read_lines(&path)
            .expect("read")
            .iter()
            .any(|line| line.starts_with("[example.com]:2222 ssh-ed25519 ")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn learn_reports_a_changed_key_and_trust_changed_replaces_it() {
        let dir = temp_dir("changed");
        let path = dir.join("known_hosts");
        let first = key(KEY_A);
        let second = key(KEY_B);
        let other = key(KEY_B);
        std::fs::write(
            &path,
            format!(
                "# a comment\nother.example {}\n",
                other.to_openssh().expect("openssh")
            ),
        )
        .expect("seed file");

        learn("host", 22, &first, &path).expect("learn");

        let err = learn("host", 22, &second, &path).expect_err("changed key is refused");
        match err {
            KnownHostsError::KeyChanged { old, new, .. } => {
                assert_eq!(old, fingerprint(&first));
                assert_eq!(new, fingerprint(&second));
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // Nothing was written by the failed learn.
        assert!(known("host", 22, &first, &path).expect("still the old key"));

        // A stale confirmation is refused.
        assert!(matches!(
            trust_changed("host", 22, "SHA256:not-it", &second, &path),
            Err(KnownHostsError::ConfirmationMismatch { .. })
        ));

        trust_changed("host", 22, &fingerprint(&first), &second, &path).expect("trust");
        assert!(known("host", 22, &second, &path).expect("new key is known"));
        assert!(learn("host", 22, &first, &path).is_err());
        // Exactly one line remains for the host, and unrelated lines survive.
        let lines = read_lines(&path).expect("read");
        assert!(lines.iter().any(|line| line.starts_with("# a comment")));
        assert!(lines.iter().any(|line| line.starts_with("other.example ")));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("host "))
                .count(),
            1
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
