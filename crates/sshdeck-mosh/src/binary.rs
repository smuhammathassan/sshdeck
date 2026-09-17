//! Finding the system `mosh` binary and reading its version.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::MoshError;

/// The program name looked for on `PATH`.
#[cfg(windows)]
const PROGRAM: &str = "mosh.exe";
#[cfg(not(windows))]
const PROGRAM: &str = "mosh";

/// A located `mosh` binary.
#[derive(Debug, Clone)]
pub struct MoshBinary {
    path: PathBuf,
    version: Option<MoshVersion>,
}

impl MoshBinary {
    /// Locates `mosh`: `explicit` when given, otherwise the first executable
    /// `mosh` on `PATH`. `mosh --version` is probed once and cached.
    ///
    /// A missing binary is [`MoshError::NotFound`], never a panic, and the
    /// message is meant to be shown to the user as-is.
    pub fn detect(explicit: Option<&Path>) -> Result<Self, MoshError> {
        Self::detect_in(explicit, std::env::var_os("PATH").as_deref())
    }

    /// [`Self::detect`] with the `PATH` value passed in, so the lookup is
    /// testable without mutating the process environment.
    fn detect_in(explicit: Option<&Path>, path: Option<&OsStr>) -> Result<Self, MoshError> {
        let found = match explicit {
            Some(candidate) if is_executable(candidate) => candidate.to_path_buf(),
            Some(candidate) => {
                return Err(MoshError::NotFound(format!(
                    "{} is not an executable file. Install mosh (macOS: `brew install mosh`, Debian/Ubuntu: `apt install mosh`) or correct the path.",
                    candidate.display()
                )))
            }
            None => search_path(path).ok_or_else(|| {
                MoshError::NotFound(
                    "no executable `mosh` on PATH. Install it (macOS: `brew install mosh`, Debian/Ubuntu: `apt install mosh`) or pass the path explicitly."
                        .to_string(),
                )
            })?,
        };
        let version = probe_version(&found);
        Ok(Self {
            path: found,
            version,
        })
    }

    /// The resolved path of the binary.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The version reported by `mosh --version`, when it could be parsed.
    /// A banner we do not recognise is `None`, not an error.
    pub fn version(&self) -> Option<&MoshVersion> {
        self.version.as_ref()
    }
}

/// The first executable `mosh` on `path`.
fn search_path(path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .map(|dir| dir.join(PROGRAM))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is a regular file we may execute. Follows symlinks, so a
/// `mosh` symlinked out of a versioned Cellar still counts.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
}

/// Without a portable executable-bit check, existence is the best we can do.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs `mosh --version` and parses the first version-like token from stdout,
/// falling back to stderr. A failure to run, or an unrecognised banner, is
/// `None`: version detection must never take the app down.
fn probe_version(path: &Path) -> Option<MoshVersion> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    MoshVersion::parse(&stdout).or_else(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        MoshVersion::parse(&stderr)
    })
}

/// A parsed version number.
///
/// Upstream prints `mosh 1.4.0 [build 1.4.0]`, but the banner is a
/// `PACKAGE_STRING` substitution that packagers have reworded before, so the
/// parser accepts any whitespace-separated `major.minor[.patch]` token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoshVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl MoshVersion {
    /// Parses the first `major.minor[.patch]` token in `text`. `None` when
    /// there is none; a missing patch component reads as `.0`.
    pub fn parse(text: &str) -> Option<Self> {
        text.split_whitespace().find_map(parse_token)
    }

    pub fn major(&self) -> u32 {
        self.major
    }

    pub fn minor(&self) -> u32 {
        self.minor
    }

    pub fn patch(&self) -> u32 {
        self.patch
    }
}

/// Trims the punctuation a banner may wrap a version in (`[build 1.4.0]`,
/// `v1.4.0`) and parses what is left.
fn parse_token(token: &str) -> Option<MoshVersion> {
    let core = token.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.');
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(MoshVersion {
        major,
        minor,
        patch,
    })
}

impl std::fmt::Display for MoshVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that exists and contains no `mosh`. The name is part of the
    /// path so tests running in parallel do not fight over one directory.
    fn empty_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sshdeck-mosh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the temp dir");
        dir
    }

    #[test]
    fn a_path_without_mosh_is_a_typed_error() {
        let empty = empty_dir("missing");
        let err = MoshBinary::detect_in(None, Some(empty.as_os_str()))
            .expect_err("an empty directory on PATH must not find mosh");
        assert!(matches!(err, MoshError::NotFound(_)), "got {err:?}");
        let message = err.to_string();
        assert!(message.contains("mosh"), "must name the program: {message}");
        assert!(
            message.contains("Install"),
            "must tell the user what to do: {message}"
        );
    }

    #[test]
    fn an_unset_path_is_a_typed_error() {
        let err = MoshBinary::detect_in(None, None).expect_err("no PATH must not find mosh");
        assert!(matches!(err, MoshError::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn a_named_path_that_is_not_a_file_is_a_typed_error() {
        let missing = empty_dir("named").join("nope");
        let err = MoshBinary::detect_in(Some(&missing), None)
            .expect_err("a nonexistent override must not be accepted");
        assert!(matches!(err, MoshError::NotFound(_)), "got {err:?}");
        assert!(err.to_string().contains("nope"), "got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn finds_an_executable_on_path_and_ignores_a_non_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("sshdeck-mosh-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the temp dir");
        let binary = dir.join("mosh");
        // A shell script with no version banner: proves the "missing string"
        // case end to end without needing real mosh installed.
        std::fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("writes the fake mosh");

        let err = MoshBinary::detect_in(None, Some(dir.as_os_str()))
            .expect_err("a non-executable file is not a mosh");
        assert!(matches!(err, MoshError::NotFound(_)), "got {err:?}");

        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x");
        let found =
            MoshBinary::detect_in(None, Some(dir.as_os_str())).expect("finds the fake mosh");
        assert_eq!(found.path(), binary.as_path());
        assert!(found.version().is_none(), "no banner means no version");
    }

    #[test]
    fn parses_a_normal_version_banner() {
        let version =
            MoshVersion::parse("mosh 1.4.0 [build 1.4.0]\nCopyright 2012 Keith Winstein\n")
                .expect("parses the upstream banner");
        assert_eq!(
            (version.major(), version.minor(), version.patch()),
            (1, 4, 0)
        );
        assert_eq!(version.to_string(), "1.4.0");
    }

    #[test]
    fn parses_a_two_component_version_and_an_unwrapped_one() {
        assert_eq!(
            MoshVersion::parse("mosh 1.4\n")
                .expect("parses")
                .to_string(),
            "1.4.0"
        );
        assert_eq!(
            MoshVersion::parse("v1.3.2\n").expect("parses").to_string(),
            "1.3.2"
        );
    }

    #[test]
    fn version_parsing_tolerates_missing_and_junk_output() {
        assert_eq!(MoshVersion::parse(""), None);
        assert_eq!(MoshVersion::parse("mosh version unknown\n"), None);
        assert_eq!(MoshVersion::parse("1"), None);
        assert_eq!(MoshVersion::parse("1.4.0.5"), None);
        assert_eq!(MoshVersion::parse("...."), None);
    }

    /// Live check. Skipped by default; run with
    /// `SSHDECK_TEST_MOSH=$(command -v mosh) cargo test -p sshdeck-mosh -- --ignored`.
    #[test]
    #[ignore = "needs the real mosh binary; set SSHDECK_TEST_MOSH"]
    fn detects_the_real_mosh() {
        let Ok(path) = std::env::var("SSHDECK_TEST_MOSH") else {
            return;
        };
        let binary =
            MoshBinary::detect(Some(Path::new(&path))).expect("the given path is executable");
        assert!(
            binary.version().is_some(),
            "no version parsed from `mosh --version` at {}",
            binary.path().display()
        );
    }
}
