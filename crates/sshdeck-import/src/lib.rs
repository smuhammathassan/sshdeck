//! Migration into sshdeck from other SSH clients' export files.
//!
//! Plain data mapping, no UI, no network, no filesystem: every importer is a
//! pure function over a `&str` and returns [`Host`] values from `sshdeck-core`.
//!
//! ## What is here
//!
//! - [`import_ssh_config`] — OpenSSH `~/.ssh/config`. Fully specified, so this
//!   is the deterministic, high-confidence importer.
//! - [`export_inventory_json`] / [`import_inventory_json`] — sshdeck's own
//!   on-disk format. Serde round trip, lossless by construction.
//! - [`import_termius_json`] — Termius JSON export. **Best effort**: we do not
//!   have the export schema (see the module docs on [`import_termius_json`] for
//!   the full list of unconfirmed assumptions). We do not claim compatibility.
//!
//! ## Reporting
//!
//! Importers never panic on malformed input. They return an [`ImportReport`]
//! that carries the hosts they could map plus diagnostics: [`ImportReport::skipped`]
//! for entries that were recognised but not importable, and
//! [`ImportReport::notes`] for everything that was ignored or unrepresentable.
//! Only truly unparseable JSON returns [`ImportError`].

mod openssh;
mod termius;

pub use openssh::import_ssh_config;
pub use termius::import_termius_json;

use sshdeck_core::{Host, Inventory};

/// Failure to read a machine-readable export.
///
/// The OpenSSH config reader is deliberately infallible (it degrades to a
/// partial report); this error is only for JSON that could not be parsed at all.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// An entry that was recognised but could not be turned into a [`Host`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    label: String,
    reason: String,
}

impl Skipped {
    /// What we saw, as best we could name it.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Why it was not imported.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn new(label: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            reason: reason.into(),
        }
    }
}

/// The result of an import: the hosts plus everything we could not map.
///
/// A partial result is normal, not an error: a malformed stanza is reported
/// here instead of aborting the whole import.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    hosts: Vec<Host>,
    skipped: Vec<Skipped>,
    notes: Vec<String>,
}

impl ImportReport {
    /// Hosts that mapped cleanly, in file order.
    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    /// Entries recognised in the source but not importable.
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// Human-readable diagnostics for ignored or unrepresentable input.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// True when nothing at all could be imported.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// Consumes the report, yielding the imported hosts.
    pub fn into_hosts(self) -> Vec<Host> {
        self.hosts
    }

    /// Builds an [`Inventory`], assigning ids from labels as `Inventory::insert`
    /// does. Source ids (if any) are not meaningful here, so they are not kept.
    pub fn into_inventory(self) -> Inventory {
        let mut inventory = Inventory::default();
        for host in self.hosts {
            inventory.insert(host);
        }
        inventory
    }

    pub(crate) fn push_host(&mut self, host: Host) {
        self.hosts.push(host);
    }

    pub(crate) fn skip(&mut self, label: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push(Skipped::new(label, reason));
    }

    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

/// Serialises an inventory to the JSON the app persists.
pub fn export_inventory_json(inventory: &Inventory) -> Result<String, ImportError> {
    Ok(serde_json::to_string(inventory)?)
}

/// Reads the JSON the app persists back into an [`Inventory`], losslessly.
pub fn import_inventory_json(json: &str) -> Result<Inventory, ImportError> {
    Ok(serde_json::from_str(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sshdeck_core::AuthMethod;
    use std::path::PathBuf;

    #[test]
    fn inventory_json_round_trips_exactly() {
        let mut inventory = Inventory::default();
        let mut host = Host::new("Prod Web", "10.0.0.1");
        host.username = "deploy".into();
        host.port = 2222;
        host.group = Some("prod".into());
        host.tags = vec!["edge".into(), "web".into()];
        host.auth = AuthMethod::Key {
            key_path: PathBuf::from("/home/deploy/.ssh/id_ed25519"),
            passphrase_ref: Some("prod-key".into()),
        };
        host.proxy_jump = Some("bastion".into());
        inventory.insert(host);
        inventory.insert(Host::new("Bastion", "bastion.example.com"));

        let json = export_inventory_json(&inventory).expect("export succeeds");
        let restored = import_inventory_json(&json).expect("import succeeds");

        assert_eq!(inventory.hosts(), restored.hosts());
        let re_exported = export_inventory_json(&restored).expect("re-export succeeds");
        assert_eq!(json, re_exported);
    }

    #[test]
    fn invalid_inventory_json_is_an_error_not_a_panic() {
        assert!(import_inventory_json("{ not json").is_err());
    }
}
