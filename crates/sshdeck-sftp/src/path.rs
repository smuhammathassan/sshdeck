//! Remote path handling.
//!
//! SFTP paths are always POSIX-style `/`-separated strings on the wire, so this
//! module never uses [`std::path`] (whose separator is the OS's). Paths are
//! joined textually, and a configured base directory cannot be escaped: any
//! `..` that would climb above the base is rejected rather than silently
//! changing what the base means.

/// A remote path that is outside its configured base directory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("remote path {path:?} escapes the base directory {base:?}")]
    Escapes { path: String, base: String },
}

/// Collapses `.` and `..`, drops empty components and duplicate slashes, and
/// removes a trailing slash. A leading `/` is preserved; `..` at the root is a
/// no-op. Relative paths keep leading `..` components.
pub fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                let can_pop = parts.last().is_some_and(|last| *last != "..");
                if can_pop {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
                // `..` at the root stays at the root.
            }
            other => parts.push(other),
        }
    }

    let mut result = String::new();
    if absolute {
        result.push('/');
    }
    result.push_str(&parts.join("/"));

    if result.is_empty() {
        // An all-relative, all-`. `/`..` path is the current directory.
        result.push('.');
    }
    result
}

/// Joins `child` onto `base`, normalising the result. `child` may be relative
/// or absolute; the joined path must stay within `base` or [`PathError`].
pub fn join(base: &str, child: &str) -> Result<String, PathError> {
    let base = normalize(base);
    let combined = if child.starts_with('/') {
        child.to_string()
    } else {
        format!("{base}/{child}")
    };
    let joined = normalize(&combined);

    if within(&base, &joined) {
        Ok(joined)
    } else {
        Err(PathError::Escapes { path: joined, base })
    }
}

/// The directory chain under `base` that must exist for `path` to exist, from
/// the first component below `base` down to `path` itself. `base` itself is not
/// included: `mkdir -p` semantics never create above the configured base.
///
/// Returns [`PathError`] when `path` escapes `base`, because the chain is built
/// with [`join`] and therefore inherits its rejection.
pub fn ancestors(base: &str, path: &str) -> Result<Vec<String>, PathError> {
    let base = normalize(base);
    let target = join(&base, path)?;
    if target == base {
        return Ok(Vec::new());
    }

    let mut dirs = Vec::new();
    let mut built = String::new();
    if target.starts_with('/') {
        built.push('/');
    }
    for component in target.split('/') {
        if component.is_empty() {
            continue;
        }
        if !built.is_empty() && !built.ends_with('/') {
            built.push('/');
        }
        built.push_str(component);
        if built == base || !within(&base, &built) {
            continue;
        }
        dirs.push(built.clone());
    }
    Ok(dirs)
}

/// Whether `path` is `base` itself or sits underneath it.
fn within(base: &str, path: &str) -> bool {
    if path == base {
        return true;
    }
    // `/` and `.` are roots: anything that did not normalise to a leading `..`
    // is inside them.
    if base == "/" || base == "." {
        return path != ".." && !path.starts_with("../");
    }
    path.strip_prefix(base)
        .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_separators_and_dots() {
        assert_eq!(normalize("/a//b/./c/"), "/a/b/c");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("a/"), "a");
        assert_eq!(normalize("/a/b/../.."), "/");
        assert_eq!(normalize("a/b/../../../c"), "../c");
    }

    #[test]
    fn joins_without_assuming_the_os_separator() {
        assert_eq!(
            join("/home/user", "docs/file.txt").expect("joins"),
            "/home/user/docs/file.txt"
        );
        // Trailing slashes on either side are absorbed.
        assert_eq!(
            join("/home/user/", "docs/").expect("joins"),
            "/home/user/docs"
        );
        assert_eq!(join("/", "etc/hosts").expect("joins"), "/etc/hosts");
        assert_eq!(join("/home/user", "").expect("joins"), "/home/user");
    }

    #[test]
    fn join_cancels_dotdot_within_the_base() {
        assert_eq!(
            join("/home/user", "docs/../file").expect("joins"),
            "/home/user/file"
        );
        assert_eq!(
            join("/home/user", "./docs/./a/../b").expect("joins"),
            "/home/user/docs/b"
        );
    }

    #[test]
    fn join_rejects_escapes_above_the_base() {
        for child in ["../other", "a/../../other", "/etc/passwd", "../../../root"] {
            let err = join("/home/user", child).expect_err("must reject the escape");
            assert!(matches!(err, PathError::Escapes { .. }), "got {err:?}");
        }
        // A sibling whose name merely shares the prefix is not inside.
        assert!(join("/home/user", "/home/user2/secret").is_err());
    }

    #[test]
    fn join_allows_dotdot_that_stays_inside() {
        assert_eq!(
            join("/home/user/docs", "sub/../notes.txt").expect("joins"),
            "/home/user/docs/notes.txt"
        );
    }

    #[test]
    fn ancestors_lists_every_directory_below_the_base() {
        assert_eq!(
            ancestors("/home/user", "docs/a/b").expect("stays inside"),
            vec![
                "/home/user/docs".to_string(),
                "/home/user/docs/a".to_string(),
                "/home/user/docs/a/b".to_string(),
            ]
        );
        // The base itself is never in the chain.
        assert_eq!(
            ancestors("/home/user", "docs").expect("inside"),
            vec!["/home/user/docs".to_string()]
        );
        assert!(ancestors("/home/user", ".").expect("inside").is_empty());
        assert!(ancestors("/home/user", "docs/..")
            .expect("inside")
            .is_empty());
        // An absolute base of `/` still only lists below the root.
        assert_eq!(
            ancestors("/", "/etc/hosts").expect("inside"),
            vec!["/etc".to_string(), "/etc/hosts".to_string()]
        );
    }

    #[test]
    fn ancestors_rejects_escapes_above_the_base() {
        for child in ["../other", "docs/../../etc", "/etc/passwd"] {
            assert!(
                ancestors("/home/user", child).is_err(),
                "{child} must not escape"
            );
        }
    }
}
