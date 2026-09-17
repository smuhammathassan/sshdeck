//! Directory listings and their file-browser ordering.

use std::time::SystemTime;

/// The kind of a remote filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl FileKind {
    /// Whether this entry is a directory (candidates sort first).
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    name: String,
    size: u64,
    kind: FileKind,
    /// Raw POSIX mode bits when the server reported them.
    permissions: Option<u32>,
    modified: Option<SystemTime>,
}

impl DirEntry {
    pub fn new(
        name: impl Into<String>,
        size: u64,
        kind: FileKind,
        permissions: Option<u32>,
        modified: Option<SystemTime>,
    ) -> Self {
        Self {
            name: name.into(),
            size,
            kind,
            permissions,
            modified,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    pub fn permissions(&self) -> Option<u32> {
        self.permissions
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    /// `ls -l`-style mode string, e.g. `drwxr-xr-x`.
    ///
    /// When the server did not report a mode, the type character still comes
    /// from the entry kind and the permission bits are shown as unknown.
    pub fn mode_string(&self) -> String {
        match self.permissions {
            Some(mode) => format_permissions(mode | type_bits(self.kind)),
            None => unknown_mode(self.kind),
        }
    }
}

/// Metadata for a single remote path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    size: u64,
    kind: FileKind,
    permissions: Option<u32>,
    modified: Option<SystemTime>,
    uid: Option<u32>,
    gid: Option<u32>,
}

impl FileStat {
    pub fn new(
        size: u64,
        kind: FileKind,
        permissions: Option<u32>,
        modified: Option<SystemTime>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Self {
        Self {
            size,
            kind,
            permissions,
            modified,
            uid,
            gid,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn kind(&self) -> FileKind {
        self.kind
    }

    pub fn permissions(&self) -> Option<u32> {
        self.permissions
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    pub fn uid(&self) -> Option<u32> {
        self.uid
    }

    pub fn gid(&self) -> Option<u32> {
        self.gid
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    pub fn mode_string(&self) -> String {
        match self.permissions {
            Some(mode) => format_permissions(mode | type_bits(self.kind)),
            None => unknown_mode(self.kind),
        }
    }
}

/// Sorts a listing the way a file browser wants it: directories first, then a
/// case-insensitive name order, with a case-sensitive tiebreak so the order is
/// stable for names differing only by case.
pub fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        b.kind
            .is_dir()
            .cmp(&a.kind.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Formats a full POSIX `st_mode` (type bits plus permission bits) as ten
/// characters, honouring setuid, setgid and the sticky bit.
pub fn format_permissions(mode: u32) -> String {
    let mut out = String::with_capacity(10);
    out.push(type_char_from_mode(mode));

    out.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    out.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    out.push(setid_char(mode & 0o100 != 0, mode & 0o4000 != 0));

    out.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    out.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    out.push(setid_char(mode & 0o010 != 0, mode & 0o2000 != 0));

    out.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    out.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    out.push(match (mode & 0o001 != 0, mode & 0o1000 != 0) {
        (true, true) => 't',
        (false, true) => 'T',
        (true, false) => 'x',
        (false, false) => '-',
    });

    out
}

fn setid_char(exec: bool, setid: bool) -> char {
    match (exec, setid) {
        (true, true) => 's',
        (false, true) => 'S',
        (true, false) => 'x',
        (false, false) => '-',
    }
}

fn type_char_from_mode(mode: u32) -> char {
    match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o100000 => '-',
        0o060000 => 'b',
        0o020000 => 'c',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '?',
    }
}

/// Mode string with unknown permission bits, e.g. `d?????????`.
fn unknown_mode(kind: FileKind) -> String {
    let mut mode = String::with_capacity(10);
    mode.push(type_char(kind));
    mode.push_str("?????????");
    mode
}

fn type_char(kind: FileKind) -> char {
    match kind {
        FileKind::Dir => 'd',
        FileKind::Symlink => 'l',
        FileKind::File => '-',
        FileKind::Other => '?',
    }
}

fn type_bits(kind: FileKind) -> u32 {
    match kind {
        FileKind::Dir => 0o040000,
        FileKind::Symlink => 0o120000,
        FileKind::File => 0o100000,
        FileKind::Other => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: FileKind) -> DirEntry {
        DirEntry::new(name, 0, kind, Some(0o100644), None)
    }

    #[test]
    fn sorts_directories_first_then_case_insensitive_names() {
        let mut entries = vec![
            entry("beta.txt", FileKind::File),
            entry("Zeta", FileKind::Dir),
            entry("Alpha", FileKind::Dir),
            entry("apple.txt", FileKind::File),
            entry("README", FileKind::File),
        ];
        sort_entries(&mut entries);

        let names: Vec<&str> = entries.iter().map(DirEntry::name).collect();
        assert_eq!(
            names,
            vec!["Alpha", "Zeta", "apple.txt", "beta.txt", "README"]
        );
    }

    #[test]
    fn formats_ls_style_permissions() {
        assert_eq!(format_permissions(0o040755), "drwxr-xr-x");
        assert_eq!(format_permissions(0o100644), "-rw-r--r--");
        assert_eq!(format_permissions(0o120777), "lrwxrwxrwx");
    }

    #[test]
    fn formats_setuid_setgid_and_sticky_bits() {
        assert_eq!(format_permissions(0o104755), "-rwsr-xr-x");
        assert_eq!(format_permissions(0o102755), "-rwxr-sr-x");
        assert_eq!(format_permissions(0o041777), "drwxrwxrwt");
        // The capital forms mean the execute bit is *not* set.
        assert_eq!(format_permissions(0o104654), "-rwSr-xr--");
        assert_eq!(format_permissions(0o041666), "drw-rw-rwT");
    }

    #[test]
    fn entry_mode_uses_kind_when_type_bits_are_missing() {
        let dir = DirEntry::new("docs", 0, FileKind::Dir, Some(0o755), None);
        assert_eq!(dir.mode_string(), "drwxr-xr-x");
        let unknown = DirEntry::new("x", 0, FileKind::Symlink, None, None);
        assert_eq!(unknown.mode_string(), "l?????????");
    }
}
