//! OpenSSH `~/.ssh/config` importer.
//!
//! The format is fully specified, so this mapping is deterministic and does not
//! rely on any unverified schema. It is the high-confidence importer.
//!
//! ## Semantics implemented
//!
//! - A `Host` line opens a block. Every alias on that line is an independent
//!   connection target: `ssh prod` and `ssh prod-web` both work when the line is
//!   `Host prod prod-web`. We therefore emit **one [`Host`] per concrete alias**
//!   rather than collapsing to a primary alias, so no entry point is lost. They
//!   share the block's connection parameters. (The alternative — one host with
//!   the extra aliases recorded — would silently drop names from the inventory;
//!   preserving every name matters more than deduplicating.)
//! - `HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump` and `ProxyCommand`
//!   are read. A missing `HostName` means the alias itself is the address, per
//!   OpenSSH. Unknown keywords are ignored.
//! - **First match wins.** The first occurrence of a key inside a block is kept;
//!   later duplicates are recorded as diagnostics. This mirrors OpenSSH's "first
//!   obtained value" rule.
//! - Keys that appear **before the first `Host` block** are global defaults.
//!   They are recorded and never emitted as a host.
//! - Wildcard patterns (`*`, `?`, `!`) are never emitted as connectable hosts;
//!   they are recorded in [`ImportReport::skipped`].
//! - `Match` blocks are ignored as block boundaries (their options never leak
//!   into the preceding `Host`). Full-line and trailing comments, blank lines,
//!   quoted values and `=`-separated values are handled. `Include` is recognised
//!   but not followed.
//! - `IdentityFile` maps to [`AuthMethod::Key`]; every other host maps to
//!   [`AuthMethod::Agent`].
//!
//! ## Unconfirmed assumptions
//!
//! All four are deliberate, documented gaps rather than silent guesses:
//!
//! - `(unconfirmed)` An unquoted trailing `#` starts a comment. OpenSSH only
//!   documents whole-line comments, so this is a superset; quote awareness means
//!   a `#` inside a quoted value is preserved.
//! - `(unconfirmed)` Tilde (`~`) and percent tokens (`%d`, `%h`, `%p`) in
//!   `IdentityFile` / `ProxyJump` are stored verbatim, not expanded.
//! - `(unconfirmed)` `Include` paths are reported, not read or expanded. The
//!   importer is a pure function over text and never touches the filesystem.
//! - `(unconfirmed)` Global defaults are recorded but not merged into hosts;
//!   only an explicit `Host` block is imported.

use crate::ImportReport;
use sshdeck_core::{AuthMethod, Host};
use std::path::PathBuf;

/// Parses an OpenSSH config from text and maps it to hosts.
///
/// Never fails: malformed stanzas become diagnostics in the returned report.
pub fn import_ssh_config(text: &str) -> ImportReport {
    let mut report = ImportReport::default();
    let mut current: Option<Block> = None;
    // Whether any Host block has been seen, to tell a global default from an
    // option stranded outside a block.
    let mut saw_host = false;

    for (offset, raw) in text.lines().enumerate() {
        let line = strip_comment(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let number = offset + 1;
        let (keyword, value) = split_keyword(line);

        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                if let Some(block) = current.take() {
                    block.flush(&mut report);
                }
                let aliases = tokenize(value);
                saw_host = true;
                if aliases.is_empty() {
                    report.note(format!("line {number}: Host with no pattern"));
                    current = None;
                } else {
                    current = Some(Block::new(aliases));
                }
            }
            "match" => {
                if let Some(block) = current.take() {
                    block.flush(&mut report);
                }
                report.note(format!(
                    "line {number}: Match block ignored; its options are not host defaults"
                ));
            }
            "include" => {
                report.note(format!(
                    "line {number}: Include not expanded: {}",
                    value.trim()
                ));
            }
            _ => match current.as_mut() {
                Some(block) => block.set(keyword, value, number, &mut report),
                None if !saw_host => report.note(format!(
                    "line {number}: global default `{keyword}` ignored; it is not a host"
                )),
                None => report.note(format!(
                    "line {number}: `{keyword}` outside any Host block ignored"
                )),
            },
        }
    }

    if let Some(block) = current.take() {
        block.flush(&mut report);
    }
    report
}

/// The options of one `Host` block, first occurrence of each key wins.
#[derive(Debug, Default)]
struct Block {
    aliases: Vec<String>,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<String>,
    identity: Option<String>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
}

impl Block {
    fn new(aliases: Vec<String>) -> Self {
        Self {
            aliases,
            ..Self::default()
        }
    }

    fn set(&mut self, keyword: &str, value: &str, line: usize, report: &mut ImportReport) {
        let value = scalar(value);
        match keyword.to_ascii_lowercase().as_str() {
            "hostname" => set_first(&mut self.hostname, value, keyword, line, report),
            "user" => set_first(&mut self.user, value, keyword, line, report),
            "port" => set_first(&mut self.port, value, keyword, line, report),
            "identityfile" => set_first(&mut self.identity, value, keyword, line, report),
            "proxyjump" => set_first(&mut self.proxy_jump, value, keyword, line, report),
            "proxycommand" => set_first(&mut self.proxy_command, value, keyword, line, report),
            _ => {}
        }
    }

    /// Emits one host per concrete alias, recording patterns and unmappable
    /// directives.
    fn flush(self, report: &mut ImportReport) {
        let Block {
            aliases,
            hostname,
            user,
            port,
            identity,
            proxy_jump,
            proxy_command,
        } = self;

        let proxy_jump = proxy_jump.filter(|value| !value.eq_ignore_ascii_case("none"));

        for alias in &aliases {
            if is_pattern(alias) {
                report.skip(alias.as_str(), "wildcard pattern; not a connectable host");
                continue;
            }

            let address = hostname.as_deref().unwrap_or(alias.as_str());
            let mut host = Host::new(alias.as_str(), address);

            if let Some(user) = &user {
                host.username = user.clone();
            }
            if let Some(port) = &port {
                match port.parse::<u16>() {
                    Ok(parsed) if parsed > 0 => host.port = parsed,
                    _ => report.note(format!("{alias}: invalid Port `{port}`; using 22")),
                }
            }
            if let Some(identity) = identity
                .as_deref()
                .filter(|value| !value.eq_ignore_ascii_case("none"))
            {
                host.auth = AuthMethod::Key {
                    key_path: PathBuf::from(identity),
                    passphrase_ref: None,
                };
            }
            if let Some(jump) = &proxy_jump {
                host.proxy_jump = Some(jump.clone());
            }
            if let Some(command) = &proxy_command {
                report.note(format!(
                    "{alias}: ProxyCommand ignored (no field in Host): {command}"
                ));
            }

            report.push_host(host);
        }
    }
}

/// Keeps the first value for a key and notes later duplicates.
fn set_first(
    slot: &mut Option<String>,
    value: String,
    keyword: &str,
    line: usize,
    report: &mut ImportReport,
) {
    if slot.is_none() {
        *slot = Some(value);
    } else {
        report.note(format!(
            "line {line}: duplicate `{keyword}` ignored; first value wins"
        ));
    }
}

/// Wildcard and negation patterns are selectors, not connectable names.
fn is_pattern(alias: &str) -> bool {
    alias.contains('*') || alias.contains('?') || alias.contains('!')
}

/// Cuts a trailing comment, respecting single and double quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '\\' {
                    escaped = true;
                } else if c == '#' {
                    return &line[..index];
                }
            }
        }
    }
    line
}

/// Splits `Keyword value` or `Keyword=value` into keyword and rest.
fn split_keyword(line: &str) -> (&str, &str) {
    match line.find(|c: char| c.is_whitespace() || c == '=') {
        Some(index) => {
            let value = line[index..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
            (&line[..index], value)
        }
        None => (line, ""),
    }
}

/// Whitespace-splits while honouring quotes and backslash escapes.
fn tokenize(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = value.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else {
                    current.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '\\' {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                } else if c.is_whitespace() {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(c);
                }
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// A scalar value is the rest of the line, with quoting resolved and runs of
/// whitespace normalised, so `ProxyCommand ssh -W %h:%p jump` survives intact.
fn scalar(value: &str) -> String {
    tokenize(value).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alias_blocks_with_ports_users_and_identity() {
        let config = "\
# team config
Host prod-web
    HostName 10.0.0.1
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_prod

Host staging
    HostName staging.internal
    User ubuntu
";
        let report = import_ssh_config(config);
        assert_eq!(report.hosts().len(), 2);

        let prod = &report.hosts()[0];
        assert_eq!(prod.label, "prod-web");
        assert_eq!(prod.address, "10.0.0.1");
        assert_eq!(prod.username, "deploy");
        assert_eq!(prod.port, 2222);
        assert_eq!(
            prod.auth,
            AuthMethod::Key {
                key_path: PathBuf::from("~/.ssh/id_prod"),
                passphrase_ref: None,
            }
        );

        let staging = &report.hosts()[1];
        assert_eq!(staging.label, "staging");
        assert_eq!(staging.address, "staging.internal");
        assert_eq!(staging.username, "ubuntu");
        assert_eq!(staging.port, 22);
        assert_eq!(staging.auth, AuthMethod::Agent);
    }

    #[test]
    fn connection_option_before_any_host_is_not_a_host() {
        let config = "User globaluser\nPort 2022\nHost real\n    HostName real.example.com\n";
        let report = import_ssh_config(config);

        assert_eq!(report.hosts().len(), 1);
        assert_eq!(report.hosts()[0].label, "real");
        // The global default is recorded, never merged or emitted.
        assert!(report
            .notes()
            .iter()
            .any(|note| note.contains("global default")));
    }

    #[test]
    fn wildcard_patterns_are_not_connectable_hosts() {
        let config = "\
Host *
    ServerAliveInterval 60

Host *.example.com prod?
    User ops
";
        let report = import_ssh_config(config);

        assert!(report.hosts().is_empty());
        assert_eq!(report.skipped().len(), 3);
        assert!(report
            .skipped()
            .iter()
            .all(|entry| entry.reason().contains("pattern")));
    }

    #[test]
    fn proxy_jump_maps_to_proxy_jump() {
        let config = "Host db\n    HostName db.internal\n    ProxyJump bastion\n";
        let report = import_ssh_config(config);

        assert_eq!(report.hosts().len(), 1);
        assert_eq!(report.hosts()[0].proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn multiple_aliases_each_become_a_host() {
        let config = "Host web web1\n    HostName 10.0.0.9\n";
        let report = import_ssh_config(config);

        assert_eq!(report.hosts().len(), 2);
        assert_eq!(report.hosts()[0].label, "web");
        assert_eq!(report.hosts()[1].label, "web1");
        assert_eq!(report.hosts()[0].address, "10.0.0.9");
        assert_eq!(report.hosts()[1].address, "10.0.0.9");
    }

    #[test]
    fn comments_quotes_includes_and_duplicates_are_handled() {
        let config = "\
User ignored          # global default, not a host
Host quoted
    HostName \"quoted.example.com\"   # trailing comment
    Port 22
    Port 2200
Include ~/.ssh/config.d/*
";
        let report = import_ssh_config(config);

        assert_eq!(report.hosts().len(), 1);
        assert_eq!(report.hosts()[0].address, "quoted.example.com");
        assert_eq!(report.hosts()[0].port, 22);
        assert!(report
            .notes()
            .iter()
            .any(|note| note.contains("first value wins")));
        assert!(report
            .notes()
            .iter()
            .any(|note| note.contains("Include not expanded")));
    }

    #[test]
    fn malformed_port_degrades_to_default_with_a_note() {
        let config = "Host broken\n    HostName broken.example.com\n    Port not-a-port\n";
        let report = import_ssh_config(config);

        assert_eq!(report.hosts().len(), 1);
        assert_eq!(report.hosts()[0].port, 22);
        assert!(report
            .notes()
            .iter()
            .any(|note| note.contains("invalid Port")));
    }
}
