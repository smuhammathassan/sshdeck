//! Jump hosts: native chains and OpenSSH `ProxyJump` semantics.
//!
//! The inventory models a jump host as a *reference* on the target
//! ([`Host::proxy_jump`]), not as a separate connection mode. That reference is
//! either
//!
//! * a **native chain** — the label (or address) of another `Host` in the
//!   inventory, whose own `proxy_jump` is followed in turn, or
//! * an **OpenSSH `ProxyJump` spec** (`[user@]host[:port]`, comma-separated for
//!   several jumps), which is what the `ssh_config` importer stores verbatim.
//!
//! [`HostChain::resolve`] turns either form into one ordered, inspectable list of
//! hops — jumps first, target last — so a UI can render "you → jump → target"
//! and name the hop that failed. Dialling each hop through the previous one, with
//! a host-key check per hop, lives in [`crate::session`].
//!
//! Resolution is pure: no network, no clock, no environment. Cycles and
//! over-deep chains are rejected with a typed error instead of recursing forever.
//!
//! `ponytail:` a `ProxyCommand` value is detected and **skipped**, never run: we
//! will not build a shell command line from inventory data (token-splitting and
//! `%h`/`%p` expansion are exactly where shell-injection surprises live). The
//! error names the command and the reason. Upgrade path: a `ProxyCommand` field
//! on `Host` plus a fixed argv exec with no shell, if a real target needs it.

use std::collections::HashSet;

use crate::session::SessionConfig;
use crate::{AuthMethod, Host, HostId, Inventory};

/// Most jump hops allowed in front of the target. OpenSSH itself does not cap
/// `ProxyJump`, but an unbounded chain can only ever be a misconfiguration or a
/// hostile import on a desktop client, so it is bounded here. A chain exactly at
/// the limit is valid; one hop more is [`ChainError::TooDeep`].
pub const MAX_JUMP_DEPTH: usize = 4;

/// One parsed `ProxyJump` element: `[user@]host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpSpec {
    username: Option<String>,
    host: String,
    port: Option<u16>,
}

impl JumpSpec {
    /// Parses one `ProxyJump` element. IPv6 literals must be bracketed
    /// (`[::1]:2222`), matching OpenSSH.
    pub fn parse(spec: &str) -> Result<Self, ChainError> {
        let spec = spec.trim();
        let invalid = |reason: &str| ChainError::InvalidJump {
            spec: spec.to_string(),
            reason: reason.to_string(),
        };

        if spec.is_empty() {
            return Err(invalid("empty jump specification"));
        }
        if spec.chars().any(char::is_whitespace) {
            return Err(invalid(
                "a jump specification cannot contain whitespace (that is a ProxyCommand)",
            ));
        }

        let (username, rest) = match spec.split_once('@') {
            Some((user, rest)) => {
                if user.is_empty() {
                    return Err(invalid("user before '@' is empty"));
                }
                if rest.contains('@') {
                    return Err(invalid("more than one '@'"));
                }
                (Some(user.to_string()), rest)
            }
            None => (None, spec),
        };

        let (host, port) = if let Some(after) = rest.strip_prefix('[') {
            let Some((inside, tail)) = after.split_once(']') else {
                return Err(invalid("unbalanced '[' in an IPv6 host"));
            };
            let port = if tail.is_empty() {
                None
            } else {
                let Some(port) = tail.strip_prefix(':') else {
                    return Err(invalid("expected ':port' after the closing ']'"));
                };
                Some(parse_port(port, spec)?)
            };
            (inside.to_string(), port)
        } else if let Some((host, port)) = rest.split_once(':') {
            if host.contains(':') {
                return Err(invalid("an IPv6 host must be bracketed"));
            }
            (host.to_string(), Some(parse_port(port, spec)?))
        } else {
            (rest.to_string(), None)
        };

        if host.is_empty() {
            return Err(invalid("host is empty"));
        }
        Ok(Self {
            username,
            host,
            port,
        })
    }

    /// Parses a `ProxyJump` value: one element, or several separated by commas,
    /// in dial order (OpenSSH's `ProxyJump a,b`).
    pub fn parse_list(spec: &str) -> Result<Vec<Self>, ChainError> {
        spec.split(',').map(Self::parse).collect()
    }

    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The port to dial, defaulting to 22 when the spec omits it.
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(22)
    }
}

fn parse_port(port: &str, spec: &str) -> Result<u16, ChainError> {
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(ChainError::InvalidJump {
            spec: spec.to_string(),
            reason: format!("{port:?} is not a port"),
        }),
        Ok(port) => Ok(port),
    }
}

/// One resolved hop: the dial config plus the inventory label it came from, when
/// it came from the inventory at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainHop {
    config: SessionConfig,
    label: Option<String>,
}

impl ChainHop {
    fn from_host(host: &Host) -> Self {
        Self {
            config: SessionConfig::from_host(host),
            label: Some(host.label.clone()),
        }
    }

    fn from_spec(spec: &JumpSpec, inventory: &Inventory, default_user: &str) -> Self {
        let known = find_host(spec.host(), inventory);
        let username = spec
            .username()
            .map(str::to_string)
            .or_else(|| known.map(|host| host.username.clone()))
            .unwrap_or_else(|| default_user.to_string());
        let auth = known.map_or(AuthMethod::Agent, |host| host.auth.clone());
        // An explicit `:port` in the spec wins; otherwise the matching
        // inventory host's port, otherwise 22.
        let port = spec
            .port()
            .or_else(|| known.map(|host| host.port))
            .unwrap_or(22);
        Self {
            config: SessionConfig::direct(spec.host(), port, username, auth),
            label: known.map(|host| host.label.clone()),
        }
    }

    pub fn address(&self) -> &str {
        self.config.address()
    }

    pub fn port(&self) -> u16 {
        self.config.port()
    }

    pub fn username(&self) -> &str {
        self.config.username()
    }

    /// The inventory label this hop came from, absent for a bare `ProxyJump`
    /// spec that names no known host.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// The hop's dial configuration, including its auth method. Secrets are
    /// attached by the caller (they live in the keychain, not in core).
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// `user@host:port`, collapsing the default port and the empty user.
    pub fn endpoint(&self) -> String {
        let user = if self.username().is_empty() {
            String::new()
        } else {
            format!("{}@", self.username())
        };
        if self.port() == 22 {
            format!("{user}{}", self.address())
        } else {
            format!("{user}{}:{}", self.address(), self.port())
        }
    }
}

/// A resolved connection path: jump hops in dial order, then the target.
///
/// The chain is both the connection configuration (each hop's [`SessionConfig`])
/// and the inspectable form a UI shows with [`Self::describe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostChain {
    jumps: Vec<ChainHop>,
    target: ChainHop,
}

impl HostChain {
    /// Resolves `host`'s `proxy_jump` reference through `inventory` into dial
    /// order. A reference that names an inventory host (by label, then address)
    /// is followed recursively; anything else is parsed as a `ProxyJump` spec.
    ///
    /// The target is always the last hop, so the result is never empty.
    pub fn resolve(host: &Host, inventory: &Inventory) -> Result<Self, ChainError> {
        let mut jumps = Vec::new();
        let mut seen: HashSet<HostId> = HashSet::new();
        seen.insert(host.id.clone());
        let mut depth = 0usize;
        if let Some(reference) = host.proxy_jump.as_deref() {
            collect_jumps(
                reference,
                inventory,
                &host.username,
                &mut seen,
                &mut depth,
                &mut jumps,
            )?;
        }
        Ok(Self {
            jumps,
            target: ChainHop::from_host(host),
        })
    }

    /// The target, i.e. the last hop.
    pub fn target(&self) -> &ChainHop {
        &self.target
    }

    /// The jump hops, in dial order.
    pub fn jumps(&self) -> &[ChainHop] {
        &self.jumps
    }

    /// Every hop in dial order: jumps first, target last.
    pub fn hops(&self) -> Vec<&ChainHop> {
        self.jumps
            .iter()
            .chain(std::iter::once(&self.target))
            .collect()
    }

    /// Total number of hops, target included.
    pub fn len(&self) -> usize {
        self.jumps.len() + 1
    }

    /// A resolved chain always has a target, so this is always `false`.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The hop's dial config by dial-order index. Used to attach per-hop
    /// secrets (a jump host may use a different key or password than the target).
    pub fn hop_mut(&mut self, index: usize) -> Option<&mut SessionConfig> {
        let jumps = self.jumps.len();
        if index < jumps {
            return self.jumps.get_mut(index).map(|hop| &mut hop.config);
        }
        if index == jumps {
            return Some(&mut self.target.config);
        }
        None
    }

    /// `you -> jump -> target`, using each hop's `user@host:port`.
    pub fn describe(&self) -> String {
        let mut path = String::from("you");
        for hop in self.hops() {
            path.push_str(" -> ");
            path.push_str(&hop.endpoint());
        }
        path
    }
}

/// A chain resolution failure. Every variant names the offending host or spec.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("invalid jump specification {spec:?}: {reason}")]
    InvalidJump { spec: String, reason: String },
    #[error("jump chain is {depth} deep, over the limit of {limit}")]
    TooDeep { limit: usize, depth: usize },
    #[error("jump chain revisits host {host:?} (cycle)")]
    Cycle { host: String },
    #[error(
        "ProxyCommand {command:?} was skipped: sshdeck does not run external \
         commands, so nothing is shell-interpolated"
    )]
    ProxyCommandSkipped { command: String },
}

/// Follows one `proxy_jump` reference, appending hops in dial order (outermost
/// ancestor first).
fn collect_jumps(
    reference: &str,
    inventory: &Inventory,
    default_user: &str,
    seen: &mut HashSet<HostId>,
    depth: &mut usize,
    out: &mut Vec<ChainHop>,
) -> Result<(), ChainError> {
    let reference = reference.trim();
    if reference.eq_ignore_ascii_case("none") {
        return Ok(());
    }
    if is_proxy_command(reference) {
        return Err(ChainError::ProxyCommandSkipped {
            command: reference.to_string(),
        });
    }

    if let Some(jump) = find_host(reference, inventory) {
        if !seen.insert(jump.id.clone()) {
            return Err(ChainError::Cycle {
                host: jump.label.clone(),
            });
        }
        *depth += 1;
        if *depth > MAX_JUMP_DEPTH {
            return Err(ChainError::TooDeep {
                limit: MAX_JUMP_DEPTH,
                depth: *depth,
            });
        }
        // Ancestors dial first, so recurse before appending this hop.
        if let Some(next) = jump.proxy_jump.as_deref() {
            collect_jumps(next, inventory, default_user, seen, depth, out)?;
        }
        out.push(ChainHop::from_host(jump));
        return Ok(());
    }

    for spec in JumpSpec::parse_list(reference)? {
        *depth += 1;
        if *depth > MAX_JUMP_DEPTH {
            return Err(ChainError::TooDeep {
                limit: MAX_JUMP_DEPTH,
                depth: *depth,
            });
        }
        out.push(ChainHop::from_spec(&spec, inventory, default_user));
    }
    Ok(())
}

/// An inventory host named by `reference`: label first, then address. The label
/// comparison is case-insensitive because labels are user-facing.
fn find_host<'a>(reference: &str, inventory: &'a Inventory) -> Option<&'a Host> {
    let reference = reference.trim();
    inventory
        .hosts()
        .iter()
        .find(|host| host.label.eq_ignore_ascii_case(reference))
        .or_else(|| {
            inventory
                .hosts()
                .iter()
                .find(|host| host.address.eq_ignore_ascii_case(reference))
        })
}

/// Whether a `proxy_jump` value is really a `ProxyCommand` command line. A
/// `ProxyJump` spec never contains whitespace and never contains `%`; a
/// `ProxyCommand` almost always contains both (`ssh -W %h:%p bastion`).
fn is_proxy_command(reference: &str) -> bool {
    reference.chars().any(char::is_whitespace) || reference.contains('%')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(label: &str, address: &str) -> Host {
        Host::new(label, address)
    }

    fn inventory(hosts: Vec<Host>) -> Inventory {
        let mut inventory = Inventory::default();
        for host in hosts {
            inventory.insert(host);
        }
        inventory
    }

    #[test]
    fn native_chain_resolves_jumps_before_the_target() {
        let target = host("app", "app.internal");
        let mut middle = host("bastion", "bastion.internal");
        middle.proxy_jump = Some("edge".into());
        let edge = host("edge", "edge.internal");
        let inventory = inventory(vec![target.clone(), middle, edge]);

        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.len(), 3);
        assert_eq!(chain.jumps().len(), 2);
        assert_eq!(chain.target().address(), "app.internal");
        assert_eq!(chain.target().label(), Some("app"));
        let endpoints: Vec<&str> = chain.hops().iter().map(|hop| hop.address()).collect();
        assert_eq!(
            endpoints,
            ["edge.internal", "bastion.internal", "app.internal"]
        );
        assert_eq!(
            chain.describe(),
            "you -> edge.internal -> bastion.internal -> app.internal"
        );
    }

    #[test]
    fn a_single_host_resolves_to_a_one_hop_chain() {
        let target = host("solo", "solo.internal");
        let inventory = inventory(vec![target.clone()]);
        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.len(), 1);
        assert!(chain.jumps().is_empty());
        assert_eq!(chain.target().address(), "solo.internal");
        assert_eq!(chain.describe(), "you -> solo.internal");
    }

    #[test]
    fn proxy_jump_none_means_no_jump() {
        let mut target = host("solo", "solo.internal");
        target.proxy_jump = Some("none".into());
        let inventory = inventory(vec![target.clone()]);
        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn a_cycle_is_rejected_naming_the_revisited_host() {
        let mut first = host("first", "first.internal");
        let mut second = host("second", "second.internal");
        first.proxy_jump = Some("second".into());
        second.proxy_jump = Some("first".into());
        let inventory = inventory(vec![first.clone(), second]);

        let err = HostChain::resolve(&first, &inventory).expect_err("cycle");
        match &err {
            ChainError::Cycle { host } => assert_eq!(host, "first"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_host_that_jumps_to_itself_is_a_cycle() {
        let mut solo = host("solo", "solo.internal");
        solo.proxy_jump = Some("solo".into());
        let inventory = inventory(vec![solo.clone()]);
        assert!(matches!(
            HostChain::resolve(&solo, &inventory),
            Err(ChainError::Cycle { .. })
        ));
    }

    #[test]
    fn a_chain_at_the_depth_limit_resolves_but_one_more_is_rejected() {
        let chain_of = |jumps: usize| {
            let mut hosts = Vec::new();
            for index in 0..jumps {
                let mut jump = host(&format!("jump{index}"), &format!("jump{index}.internal"));
                jump.proxy_jump = if index + 1 < jumps {
                    Some(format!("jump{}", index + 1))
                } else {
                    None
                };
                hosts.push(jump);
            }
            let mut target = host("target", "target.internal");
            target.proxy_jump = if jumps == 0 {
                None
            } else {
                Some("jump0".into())
            };
            hosts.push(target.clone());
            (inventory(hosts), target)
        };

        let (inventory, target) = chain_of(MAX_JUMP_DEPTH);
        let chain = HostChain::resolve(&target, &inventory).expect("at the limit resolves");
        assert_eq!(chain.len(), MAX_JUMP_DEPTH + 1);

        let (inventory, target) = chain_of(MAX_JUMP_DEPTH + 1);
        match HostChain::resolve(&target, &inventory) {
            Err(ChainError::TooDeep { limit, depth }) => {
                assert_eq!(limit, MAX_JUMP_DEPTH);
                assert_eq!(depth, MAX_JUMP_DEPTH + 1);
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn parses_proxy_jump_specs_in_every_supported_form() {
        let spec = JumpSpec::parse("ops@bastion.example.com:2222").expect("parses");
        assert_eq!(spec.username(), Some("ops"));
        assert_eq!(spec.host(), "bastion.example.com");
        assert_eq!(spec.port(), Some(2222));

        let spec = JumpSpec::parse("ops@bastion.example.com").expect("parses");
        assert_eq!(spec.username(), Some("ops"));
        assert_eq!(spec.port(), None);
        assert_eq!(spec.port_or_default(), 22);

        let spec = JumpSpec::parse("bastion.example.com").expect("parses");
        assert_eq!(spec.username(), None);
        assert_eq!(spec.host(), "bastion.example.com");
        assert_eq!(spec.port(), None);

        let spec = JumpSpec::parse("[::1]:2200").expect("parses");
        assert_eq!(spec.host(), "::1");
        assert_eq!(spec.port(), Some(2200));

        let list = JumpSpec::parse_list("a.example.com,ops@b.example.com:2222").expect("parses");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].host(), "a.example.com");
        assert_eq!(list[1].host(), "b.example.com");
    }

    #[test]
    fn rejects_malformed_proxy_jump_specs() {
        for bad in [
            "",
            "   ",
            "@host",
            "user@",
            "host:0",
            "host:notaport",
            "[::1",
            "user@host@other",
            "a::b",
            "ssh -W %h:%p bastion",
        ] {
            assert!(
                JumpSpec::parse(bad).is_err(),
                "{bad:?} must be rejected by the parser"
            );
        }
    }

    #[test]
    fn a_proxy_jump_spec_becomes_a_hop_and_inherits_the_target_user() {
        let mut target = host("db", "db.internal");
        target.username = "deploy".into();
        target.proxy_jump = Some("ops@bastion.example.com:2222".into());
        let inventory = inventory(vec![target.clone()]);

        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.target().address(), "db.internal");
        let jump = &chain.jumps()[0];
        assert_eq!(jump.address(), "bastion.example.com");
        assert_eq!(jump.port(), 2222);
        assert_eq!(jump.username(), "ops");
        assert_eq!(jump.label(), None);
    }

    #[test]
    fn a_proxy_jump_without_a_user_defaults_to_the_target_user() {
        let mut target = host("db", "db.internal");
        target.username = "deploy".into();
        target.proxy_jump = Some("bastion.example.com".into());
        let inventory = inventory(vec![target.clone()]);

        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        assert_eq!(chain.jumps()[0].username(), "deploy");
        assert_eq!(chain.jumps()[0].port(), 22);
    }

    #[test]
    fn a_proxy_jump_naming_an_inventory_host_reuses_its_config() {
        let mut bastion = host("bastion", "bastion.example.com");
        bastion.username = "ops".into();
        bastion.auth = AuthMethod::Key {
            key_path: "id_ed25519".into(),
            passphrase_ref: None,
        };
        let mut target = host("db", "db.internal");
        target.proxy_jump = Some("bastion.example.com".into());
        let inventory = inventory(vec![bastion, target.clone()]);

        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        let jump = &chain.jumps()[0];
        assert_eq!(jump.label(), Some("bastion"));
        assert_eq!(jump.username(), "ops");
        assert_eq!(
            jump.config().auth(),
            &AuthMethod::Key {
                key_path: "id_ed25519".into(),
                passphrase_ref: None,
            }
        );
    }

    #[test]
    fn a_comma_separated_proxy_jump_dials_in_order() {
        let mut target = host("db", "db.internal");
        target.proxy_jump = Some("a.example.com,b.example.com".into());
        let inventory = inventory(vec![target.clone()]);

        let chain = HostChain::resolve(&target, &inventory).expect("resolves");
        let endpoints: Vec<&str> = chain.hops().iter().map(|hop| hop.address()).collect();
        assert_eq!(endpoints, ["a.example.com", "b.example.com", "db.internal"]);
    }

    #[test]
    fn a_proxy_command_style_jump_is_skipped_with_a_reason() {
        let mut target = host("db", "db.internal");
        target.proxy_jump = Some("ssh -W %h:%p bastion".into());
        let inventory = inventory(vec![target.clone()]);

        let err = HostChain::resolve(&target, &inventory).expect_err("is skipped");
        assert!(matches!(&err, ChainError::ProxyCommandSkipped { .. }));
        let message = err.to_string();
        assert!(message.contains("skipped"), "{message}");
        assert!(message.contains("ProxyCommand"), "{message}");
    }

    #[test]
    fn hop_mut_reaches_every_hop_in_dial_order() {
        let mut target = host("db", "db.internal");
        target.proxy_jump = Some("jump.example.com".into());
        let inventory = inventory(vec![target.clone()]);
        let mut chain = HostChain::resolve(&target, &inventory).expect("resolves");

        assert_eq!(
            chain.hop_mut(0).map(|config| config.address()),
            Some("jump.example.com")
        );
        assert_eq!(
            chain.hop_mut(1).map(|config| config.address()),
            Some("db.internal")
        );
        assert!(chain.hop_mut(2).is_none());
    }
}
