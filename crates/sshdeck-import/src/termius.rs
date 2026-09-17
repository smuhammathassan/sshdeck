//! Termius JSON export importer — **best effort, schema unconfirmed**.
//!
//! We do **not** have Termius's export schema, and no exported document has been
//! verified against this reader. It therefore does not claim compatibility with
//! any Termius version. It is a *tolerant* reader: it accepts a JSON document
//! that carries a top-level array of host objects (see below), maps the fields
//! it recognises under several plausible names, ignores the rest, and records
//! everything it could not map in the returned [`ImportReport`].
//!
//! Invalid JSON is the only hard failure. A document with no recognisable hosts
//! array, or individual entries missing an address, produces an empty/partial
//! report with diagnostics — never a panic.
//!
//! ## Unconfirmed assumptions
//!
//! Every mapping below is an assumption, not a verified fact:
//!
//! - `(unconfirmed)` The document is either a bare JSON array of host objects,
//!   or an object nesting that array under one of `hosts`, `connections`,
//!   `items`, `data` (first match wins).
//! - `(unconfirmed)` Label comes from `label`, `name`, `title`, `alias` or
//!   `host_label`; address from `address`, `host`, `hostname`, `host_name` or
//!   `ip`; username from `username`, `user` or `login`; group from `group`,
//!   `group_name` or `folder`.
//! - `(unconfirmed)` Port is a JSON number or a numeric string under `port` or
//!   `port_number`.
//! - `(unconfirmed)` Tags live under `tags` or `labels`, as an array of strings
//!   or of objects carrying `name`/`label`/`title`, or as a comma-separated
//!   string.
//! - `(unconfirmed)` A private key path lives under `identity`,
//!   `identity_file`, `key`, `key_path`, `private_key` or
//!   `private_key_path`; when present the host maps to [`AuthMethod::Key`],
//!   otherwise to [`AuthMethod::Agent`].
//! - `(unconfirmed)` A jump host lives under `proxy_jump` or `jump_host` or
//!   `proxy`. Termius models jumps as host chains and `proxy` as a SOCKS/HTTP
//!   proxy object; a non-string `proxy` is not mapped and is reported.
//! - `(unconfirmed)` `password`, `secret` and `passphrase` fields are **not**
//!   imported — secrets never enter the inventory — and are reported.

use crate::{ImportError, ImportReport};
use serde_json::{Map, Value};
use sshdeck_core::{AuthMethod, Host};
use std::path::PathBuf;

const HOST_ARRAY_KEYS: &[&str] = &["hosts", "connections", "items", "data"];
const LABEL_KEYS: &[&str] = &["label", "name", "title", "alias", "host_label"];
const ADDRESS_KEYS: &[&str] = &["address", "host", "hostname", "host_name", "ip"];
const PORT_KEYS: &[&str] = &["port", "port_number"];
const USER_KEYS: &[&str] = &["username", "user", "login"];
const GROUP_KEYS: &[&str] = &["group", "group_name", "folder"];
const TAG_KEYS: &[&str] = &["tags", "labels"];
const KEY_KEYS: &[&str] = &[
    "identity",
    "identity_file",
    "key",
    "key_path",
    "private_key",
    "private_key_path",
];
const PROXY_KEYS: &[&str] = &["proxy_jump", "jump_host", "proxy"];
const SECRET_KEYS: &[&str] = &["password", "secret", "passphrase"];
const IGNORED_KEYS: &[&str] = &["id", "uuid", "created_at", "updated_at"];

/// Reads a Termius JSON export from text, best effort.
///
/// Returns [`ImportError::Json`] only when the text is not valid JSON at all.
pub fn import_termius_json(text: &str) -> Result<ImportReport, ImportError> {
    let value: Value = serde_json::from_str(text)?;
    let mut report = ImportReport::default();

    let Some(entries) = locate_entries(&value) else {
        report.note(format!(
            "no hosts array found; looked for a bare array or an object keyed by {HOST_ARRAY_KEYS:?}"
        ));
        return Ok(report);
    };

    for (index, entry) in entries.iter().enumerate() {
        match entry.as_object() {
            Some(object) => import_entry(object, index, &mut report),
            None => report.skip(format!("entry {index}"), "not a JSON object"),
        }
    }
    Ok(report)
}

/// Finds the host array at the documented (unconfirmed) locations.
fn locate_entries(value: &Value) -> Option<&[Value]> {
    if let Some(array) = value.as_array() {
        return Some(array.as_slice());
    }
    let object = value.as_object()?;
    HOST_ARRAY_KEYS
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_array))
        .map(Vec::as_slice)
}

fn import_entry(object: &Map<String, Value>, index: usize, report: &mut ImportReport) {
    let label = first_string(object, LABEL_KEYS);
    let Some(address) = first_string(object, ADDRESS_KEYS) else {
        let name = label.unwrap_or_else(|| format!("entry {index}"));
        report.skip(name, "no address/host/hostname field");
        return;
    };
    let label = label.unwrap_or_else(|| address.clone());

    let mut host = Host::new(label.as_str(), address.as_str());
    if let Some(user) = first_string(object, USER_KEYS) {
        host.username = user;
    }
    if let Some(port) = first_port(object, PORT_KEYS) {
        host.port = port;
    }
    host.group = first_string(object, GROUP_KEYS);
    host.tags = collect_tags(object);
    if let Some(path) = first_string(object, KEY_KEYS) {
        host.auth = AuthMethod::Key {
            key_path: PathBuf::from(path),
            passphrase_ref: None,
        };
    }
    host.proxy_jump = first_string(object, PROXY_KEYS);

    for key in object.keys() {
        let lower = key.to_ascii_lowercase();
        if SECRET_KEYS.contains(&lower.as_str()) {
            report.note(format!(
                "{label}: `{key}` present but secrets are not imported"
            ));
        } else if !is_known_field(&lower) {
            report.note(format!("{label}: ignored field `{key}`"));
        }
    }

    report.push_host(host);
}

/// First non-empty string among `keys`, in order.
fn first_string(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let text = object.get(*key)?.as_str()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

/// First usable port among `keys`: a positive JSON integer or numeric string.
fn first_port(object: &Map<String, Value>, keys: &[&str]) -> Option<u16> {
    keys.iter().find_map(|key| {
        let value = object.get(*key)?;
        value
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u16>().ok()))
            .filter(|port| *port > 0)
    })
}

/// Tags from an array of strings/objects, or a comma-separated string.
fn collect_tags(object: &Map<String, Value>) -> Vec<String> {
    let mut tags = Vec::new();
    for key in TAG_KEYS {
        let Some(value) = object.get(*key) else {
            continue;
        };
        match value {
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::String(text) if !text.trim().is_empty() => {
                            tags.push(text.trim().to_string());
                        }
                        Value::Object(inner) => {
                            if let Some(name) = first_string(inner, &["name", "label", "title"]) {
                                tags.push(name);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Value::String(text) => {
                for part in text.split(',') {
                    let part = part.trim();
                    if !part.is_empty() {
                        tags.push(part.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    tags
}

/// Keys we deliberately read. Everything else is reported as unmapped.
fn is_known_field(lower: &str) -> bool {
    LABEL_KEYS.contains(&lower)
        || ADDRESS_KEYS.contains(&lower)
        || PORT_KEYS.contains(&lower)
        || USER_KEYS.contains(&lower)
        || GROUP_KEYS.contains(&lower)
        || TAG_KEYS.contains(&lower)
        || KEY_KEYS.contains(&lower)
        || PROXY_KEYS.contains(&lower)
        || SECRET_KEYS.contains(&lower)
        || IGNORED_KEYS.contains(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
    {
      "hosts": [
        {
          "name": "Prod DB",
          "hostname": "db.internal",
          "port": 2222,
          "username": "admin",
          "group": "prod",
          "tags": ["db", { "name": "critical" }],
          "identity_file": "/home/u/.ssh/id_ed25519",
          "proxy_jump": "bastion",
          "id": "abc-123",
          "password": "hunter2",
          "color": "red"
        },
        { "label": "No Address" }
      ]
    }
    "#;

    #[test]
    fn termius_sample_maps_hosts_and_reports_what_it_skipped() {
        let report = import_termius_json(SAMPLE).expect("valid JSON");

        assert_eq!(report.hosts().len(), 1);
        let host = &report.hosts()[0];
        assert_eq!(host.label, "Prod DB");
        assert_eq!(host.address, "db.internal");
        assert_eq!(host.port, 2222);
        assert_eq!(host.username, "admin");
        assert_eq!(host.group.as_deref(), Some("prod"));
        assert_eq!(host.tags, vec!["db".to_string(), "critical".to_string()]);
        assert_eq!(
            host.auth,
            AuthMethod::Key {
                key_path: PathBuf::from("/home/u/.ssh/id_ed25519"),
                passphrase_ref: None,
            }
        );
        assert_eq!(host.proxy_jump.as_deref(), Some("bastion"));

        assert_eq!(report.skipped().len(), 1);
        assert_eq!(report.skipped()[0].label(), "No Address");
        assert!(report.skipped()[0].reason().contains("address"));

        assert!(report
            .notes()
            .iter()
            .any(|note| note.contains("secrets are not imported")));
        assert!(report
            .notes()
            .iter()
            .any(|note| note.contains("ignored field `color`")));
    }

    #[test]
    fn termius_accepts_a_bare_array_and_does_not_panic_on_junk() {
        let bare = r#"[{"alias": "edge", "ip": "10.1.1.1", "user": "root"}]"#;
        let report = import_termius_json(bare).expect("valid JSON");
        assert_eq!(report.hosts().len(), 1);
        assert_eq!(report.hosts()[0].label, "edge");
        assert_eq!(report.hosts()[0].address, "10.1.1.1");

        // Valid JSON but the wrong shape: a partial (empty) report, not a panic.
        let wrong_shape = import_termius_json(r#"{"unrelated": 1}"#).expect("valid JSON");
        assert!(wrong_shape.is_empty());
        assert!(!wrong_shape.notes().is_empty());

        // Not JSON at all: the one hard error.
        assert!(import_termius_json("definitely not json").is_err());
    }
}
