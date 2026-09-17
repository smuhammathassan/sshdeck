//! Domain model and persistence for sshdeck.
//!
//! This crate is intentionally free of UI and of any SSH implementation detail:
//! it owns the host inventory, the authentication model, and the on-disk store,
//! so the UI layer and the transport layer can evolve independently.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod session;

/// Stable identifier for a host, independent of its label or address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Derives an id from a label, suffixed for uniqueness by the caller.
    pub fn from_label(label: &str) -> Self {
        let slug: String = label
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let slug = slug.trim_matches('-').to_string();
        Self(if slug.is_empty() {
            "host".to_string()
        } else {
            slug
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How to authenticate to a host.
///
/// Secrets are never stored in the inventory. Password and key-passphrase
/// variants hold an opaque keychain reference only; the secret itself lives in
/// the OS keychain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Use keys held by the running ssh-agent.
    Agent,
    /// Public-key auth with a private key file on disk.
    Key {
        key_path: PathBuf,
        /// Keychain entry holding the passphrase, when the key is encrypted.
        passphrase_ref: Option<String>,
    },
    /// Password auth; the password lives in the keychain under this reference.
    Password { secret_ref: String },
    /// Server-driven keyboard-interactive prompts.
    KeyboardInteractive,
    /// No authentication (rare; test fixtures and hardened jump hosts).
    None,
}

impl AuthMethod {
    /// Short label for lists and the status bar.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Key { .. } => "key",
            Self::Password { .. } => "password",
            Self::KeyboardInteractive => "keyboard-interactive",
            Self::None => "none",
        }
    }

    /// Whether this method resolves without prompting the user at connect time.
    pub fn is_non_interactive(&self) -> bool {
        !matches!(self, Self::KeyboardInteractive)
    }
}

/// A saved connection target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub id: HostId,
    pub label: String,
    /// Hostname or IP address, without the port.
    pub address: String,
    pub port: u16,
    pub username: String,
    /// Optional group/folder name used to organise the sidebar.
    pub group: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub auth: AuthMethod,
    /// Label of another host to tunnel through, resolved at connect time.
    pub proxy_jump: Option<String>,
}

impl Host {
    /// A host with the conventional SSH defaults, so callers only override what
    /// they care about.
    pub fn new(label: impl Into<String>, address: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            id: HostId::from_label(&label),
            label,
            address: address.into(),
            port: 22,
            username: String::new(),
            group: None,
            tags: Vec::new(),
            auth: AuthMethod::Agent,
            proxy_jump: None,
        }
    }

    /// `user@host:port`, collapsing the port when it is the SSH default.
    pub fn endpoint(&self) -> String {
        let user = if self.username.is_empty() {
            String::new()
        } else {
            format!("{}@", self.username)
        };
        if self.port == 22 {
            format!("{user}{}", self.address)
        } else {
            format!("{user}{}:{}", self.address, self.port)
        }
    }

    /// Case-insensitive match against label, address, username, group and tags.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let haystacks = std::iter::once(&self.label)
            .chain(std::iter::once(&self.address))
            .chain(std::iter::once(&self.username))
            .chain(self.group.iter())
            .chain(self.tags.iter());
        haystacks
            .into_iter()
            .any(|h| h.to_lowercase().contains(&query))
    }
}

/// Lifecycle of a connection attempt, surfaced in the tab strip and status bar.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    #[default]
    Disconnected,
    Connecting,
    Authenticating,
    Connected,
    Failed {
        message: String,
    },
    Closed {
        code: Option<i32>,
    },
}

impl SessionState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connected | Self::Connecting | Self::Authenticating
        )
    }

    /// Whether the session is in a terminal state (open or close is allowed).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Disconnected | Self::Failed { .. } | Self::Closed { .. }
        )
    }

    pub fn label(&self) -> String {
        match self {
            Self::Disconnected => "disconnected".into(),
            Self::Connecting => "connecting".into(),
            Self::Authenticating => "authenticating".into(),
            Self::Connected => "connected".into(),
            Self::Failed { message } => format!("failed: {message}"),
            Self::Closed { code: Some(code) } => format!("closed ({code})"),
            Self::Closed { code: None } => "closed".into(),
        }
    }
}

/// The whole persisted inventory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    hosts: Vec<Host>,
    #[serde(default)]
    version: u32,
}

impl Inventory {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    pub fn get(&self, id: &HostId) -> Option<&Host> {
        self.hosts.iter().find(|h| &h.id == id)
    }

    /// Hosts matching `query`, in stored order.
    pub fn filtered(&self, query: &str) -> Vec<&Host> {
        self.hosts.iter().filter(|h| h.matches(query)).collect()
    }

    /// Inserts a host, assigning a unique id derived from its label.
    pub fn insert(&mut self, mut host: Host) -> HostId {
        let base = HostId::from_label(&host.label);
        let mut candidate = base.clone();
        let mut suffix = 2;
        while self.hosts.iter().any(|h| h.id == candidate) {
            candidate = HostId::new(format!("{}-{}", base.as_str(), suffix));
            suffix += 1;
        }
        host.id = candidate.clone();
        self.hosts.push(host);
        candidate
    }

    pub fn remove(&mut self, id: &HostId) -> Option<Host> {
        let index = self.hosts.iter().position(|h| &h.id == id)?;
        Some(self.hosts.remove(index))
    }

    /// Replaces a host in place, matching on its id.
    pub fn upsert(&mut self, host: Host) -> bool {
        match self.hosts.iter_mut().find(|h| h.id == host.id) {
            Some(slot) => {
                *slot = host;
                true
            }
            None => {
                self.hosts.push(host);
                false
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not read inventory at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write inventory at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("inventory at {path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// JSON-backed inventory on disk.
///
/// Loading a missing file is not an error: a first run starts from an empty
/// inventory, which is what an empty default would produce anyway.
#[derive(Debug, Clone)]
pub struct HostStore {
    path: PathBuf,
    inventory: Inventory,
}

impl HostStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            inventory: Inventory::default(),
        }
    }

    /// Default location: `~/Library/Application Support/sshdeck/hosts.json` on
    /// macOS, `$XDG_CONFIG_HOME/sshdeck/hosts.json` elsewhere.
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
            .join("hosts.json")
    }

    pub fn at_default_path() -> Self {
        Self::new(Self::default_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn inventory_mut(&mut self) -> &mut Inventory {
        &mut self.inventory
    }

    /// Reads the inventory from disk. A missing file yields an empty inventory.
    pub fn load(&mut self) -> Result<(), StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.inventory = Inventory::default();
                return Ok(());
            }
            Err(source) => {
                return Err(StoreError::Read {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        self.inventory = serde_json::from_slice(&bytes).map_err(|source| StoreError::Parse {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }

    /// Writes the inventory atomically (temp file + rename) so an interrupted
    /// write cannot leave a truncated file behind.
    pub fn save(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut payload =
            serde_json::to_vec_pretty(&self.inventory).map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        payload.push(b'\n');

        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, &payload).map_err(|source| StoreError::Write {
            path: temp.clone(),
            source,
        })?;
        std::fs::rename(&temp, &self.path).map_err(|source| StoreError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_assigns_unique_ids() {
        let mut inv = Inventory::default();
        let first = inv.insert(Host::new("Prod Web", "10.0.0.1"));
        let second = inv.insert(Host::new("Prod Web", "10.0.0.2"));
        assert_eq!(first.as_str(), "prod-web");
        assert_eq!(second.as_str(), "prod-web-2");
        assert_eq!(inv.len(), 2);
    }

    #[test]
    fn filtered_matches_label_address_and_tag() {
        let mut inv = Inventory::default();
        let mut host = Host::new("Prod Web", "10.0.0.1");
        host.username = "deploy".into();
        host.tags.push("edge".into());
        inv.insert(host);

        assert_eq!(inv.filtered("prod").len(), 1);
        assert_eq!(inv.filtered("10.0.0").len(), 1);
        assert_eq!(inv.filtered("DEPLOY").len(), 1);
        assert_eq!(inv.filtered("edge").len(), 1);
        assert_eq!(inv.filtered("staging").len(), 0);
        assert_eq!(inv.filtered("  ").len(), 1);
    }

    #[test]
    fn endpoint_collapses_default_port() {
        let mut host = Host::new("web", "example.com");
        host.username = "root".into();
        assert_eq!(host.endpoint(), "root@example.com");
        host.port = 2222;
        assert_eq!(host.endpoint(), "root@example.com:2222");
    }

    #[test]
    fn store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("sshdeck-test-{}", std::process::id()));
        let path = dir.join("hosts.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = HostStore::new(&path);
        store.load().expect("missing file loads as empty");
        assert!(store.inventory().is_empty());

        store
            .inventory_mut()
            .insert(Host::new("Staging", "staging.internal"));
        store.save().expect("save succeeds");

        let mut reloaded = HostStore::new(&path);
        reloaded.load().expect("reload succeeds");
        assert_eq!(reloaded.inventory().len(), 1);
        assert_eq!(reloaded.inventory().hosts()[0].label, "Staging");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_state_terminal_classification() {
        assert!(!SessionState::Connected.is_terminal());
        assert!(SessionState::Disconnected.is_terminal());
        assert!(SessionState::Failed {
            message: "no".into()
        }
        .is_terminal());
        assert!(SessionState::Connecting.is_active());
    }
}
