//! Persisted application settings for sshdeck.
//!
//! Pure logic and file I/O: no UI, no `gpui`, no network, no async runtime. The
//! UI reads and writes this model through clamped setters; the on-disk file is
//! re-clamped on the way in, so a hand-edited or corrupted file cannot produce a
//! state the app cannot render.
//!
//! The design mirrors `sshdeck_core::HostStore`: a missing file loads as
//! defaults, `save` writes atomically via a temp file and rename, and the
//! default path is
//! `~/Library/Application Support/sshdeck/settings.json` on macOS or
//! `$XDG_CONFIG_HOME/sshdeck/settings.json` elsewhere.

use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};

/// Terminal font size bounds, in pixels. Matches the terminal pane's clamp
/// (`6..=72`); the floor keeps text legible, the ceiling keeps one line from
/// swallowing the pane.
pub const MIN_FONT_SIZE: f32 = 6.0;
pub const MAX_FONT_SIZE: f32 = 72.0;

/// Default font size. Matches the terminal pane's compiled default.
pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// Scrollback bounds. `docs/BUDGET.md` caps scrollback as the memory guard, so
/// there is deliberately no "unlimited" value: unbounded scrollback is how a
/// 60 MB client becomes a 500 MB one after a long build log.
pub const MIN_SCROLLBACK: usize = 1;
pub const MAX_SCROLLBACK: usize = 10_000;

/// Default scrollback. Equal to the cap on purpose: the budget says scrollback
/// is capped at 10 000 lines per session, not that it should start lower.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Cursor blink is the one accepted timer in the terminal (`docs/BUDGET.md`);
/// on by default, suppressed while the window is unfocused.
pub const DEFAULT_CURSOR_BLINK: bool = true;

/// Whether closing a window with live sessions asks first.
pub const DEFAULT_CONFIRM_ON_CLOSE: bool = true;

/// Clamps a font size in pixels to the supported range.
///
/// A non-finite value (a cleared number field, or a hand-edited file) and a
/// negative one fall back to the default rather than producing a zero-sized
/// font. This is the trust boundary: every font size, however it arrives, goes
/// through here.
fn clamp_font_size(value: f64) -> f32 {
    if !value.is_finite() || value < 0.0 {
        return DEFAULT_FONT_SIZE;
    }
    value.clamp(f64::from(MIN_FONT_SIZE), f64::from(MAX_FONT_SIZE)) as f32
}

/// Clamps a scrollback line count to the budgeted range.
///
/// Non-finite and negative values fall back to the default; `0` clamps up to
/// the floor rather than to "no scrollback". Bounded on purpose: see
/// [`MAX_SCROLLBACK`].
fn clamp_scrollback(value: f64) -> usize {
    if !value.is_finite() || value < 0.0 {
        return DEFAULT_SCROLLBACK;
    }
    value
        .clamp(MIN_SCROLLBACK as f64, MAX_SCROLLBACK as f64)
        .round() as usize
}

/// The app's colour theme.
///
/// `Dark` is the default because the Termius-matched theme ships dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Light,
    #[default]
    Dark,
}

impl ThemeMode {
    pub fn is_dark(&self) -> bool {
        matches!(self, Self::Dark)
    }
}

fn default_font_size() -> f32 {
    DEFAULT_FONT_SIZE
}

fn default_scrollback() -> usize {
    DEFAULT_SCROLLBACK
}

fn default_cursor_blink() -> bool {
    DEFAULT_CURSOR_BLINK
}

fn default_confirm_on_close() -> bool {
    DEFAULT_CONFIRM_ON_CLOSE
}

/// Reads a float leniently.
///
/// JSON has no `NaN`/`Infinity` literal (RFC 8259), and serde_json errors out
/// on a float too large for `f64`. A hand-edited file is more likely to carry a
/// quoted value than a bare non-finite token, so both a number and a numeric
/// string are accepted, and `null` is treated as absent. Non-finite results are
/// not rejected here: the clamp functions own that decision, so there is one
/// place where a bad number becomes a default.
struct LenientF64;

impl<'de> serde::de::Visitor<'de> for LenientF64 {
    type Value = f64;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a number or a numeric string")
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<f64, E> {
        Ok(value)
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<f64, E> {
        value
            .parse()
            .map_err(|_| E::custom(format_args!("expected a number, got {value:?}")))
    }

    /// `null` means "not set"; `NaN` clamps to the default downstream.
    fn visit_unit<E: serde::de::Error>(self) -> Result<f64, E> {
        Ok(f64::NAN)
    }
}

fn de_lenient_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(LenientF64)
}

fn de_font_size<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(clamp_font_size(de_lenient_f64(deserializer)?))
}

fn de_scrollback<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(clamp_scrollback(de_lenient_f64(deserializer)?))
}

/// Every persisted application setting.
///
/// Fields are private and every mutation goes through a clamping setter, so
/// nothing can put the model out of range — not even a hand-edited file read by
/// `serde`.
///
/// `#[serde(default)]` plus the absence of `deny_unknown_fields` means a file
/// written by a newer build still loads here: known fields are read, unknown
/// ones are dropped. That is the forward-compatibility half of the envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_font_size", deserialize_with = "de_font_size")]
    font_size: f32,
    #[serde(default = "default_scrollback", deserialize_with = "de_scrollback")]
    scrollback_lines: usize,
    #[serde(default = "default_cursor_blink")]
    cursor_blink: bool,
    #[serde(default)]
    theme_mode: ThemeMode,
    #[serde(default = "default_confirm_on_close")]
    confirm_on_close: bool,
    #[serde(default)]
    compact: bool,
    #[serde(default)]
    show_experimental: bool,
    #[serde(default)]
    autocomplete_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            scrollback_lines: DEFAULT_SCROLLBACK,
            cursor_blink: DEFAULT_CURSOR_BLINK,
            theme_mode: ThemeMode::default(),
            confirm_on_close: DEFAULT_CONFIRM_ON_CLOSE,
            compact: false,
            show_experimental: false,
            autocomplete_enabled: false,
        }
    }
}

impl Settings {
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    pub fn scrollback_lines(&self) -> usize {
        self.scrollback_lines
    }

    pub fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    pub fn theme_mode(&self) -> ThemeMode {
        self.theme_mode
    }

    pub fn confirm_on_close(&self) -> bool {
        self.confirm_on_close
    }

    pub fn compact(&self) -> bool {
        self.compact
    }

    pub fn show_experimental(&self) -> bool {
        self.show_experimental
    }

    pub fn autocomplete_enabled(&self) -> bool {
        self.autocomplete_enabled
    }

    /// Sets the font size, clamped to `MIN_FONT_SIZE..=MAX_FONT_SIZE`.
    pub fn set_font_size(&mut self, px: f32) {
        self.font_size = clamp_font_size(f64::from(px));
    }

    /// Sets the scrollback line count, clamped to `1..=MAX_SCROLLBACK`.
    pub fn set_scrollback_lines(&mut self, lines: usize) {
        self.scrollback_lines = lines.clamp(MIN_SCROLLBACK, MAX_SCROLLBACK);
    }

    pub fn set_cursor_blink(&mut self, on: bool) {
        self.cursor_blink = on;
    }

    pub fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
    }

    pub fn set_confirm_on_close(&mut self, on: bool) {
        self.confirm_on_close = on;
    }

    pub fn set_compact(&mut self, compact: bool) {
        self.compact = compact;
    }

    pub fn set_show_experimental(&mut self, show: bool) {
        self.show_experimental = show;
    }

    pub fn set_autocomplete_enabled(&mut self, on: bool) {
        self.autocomplete_enabled = on;
    }

    /// Default location: `~/Library/Application Support/sshdeck/settings.json`
    /// on macOS, `$XDG_CONFIG_HOME/sshdeck/settings.json` elsewhere.
    pub fn default_path() -> PathBuf {
        SettingsStore::default_path()
    }

    /// Reads settings from disk at the default location. A missing or unreadable
    /// file falls back to defaults.
    pub fn load() -> Self {
        let mut store = SettingsStore::at_default_path();
        let _ = store.load();
        store.settings
    }

    /// Saves settings atomically to the default location.
    pub fn save(&self) -> Result<(), SettingsError> {
        let store = SettingsStore {
            path: Self::default_path(),
            settings: self.clone(),
        };
        store.save()
    }
}

/// Typed failures for the settings store, mirroring `sshdeck_core::StoreError`.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not read settings at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write settings at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("settings at {path} are not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// The on-disk shape: a format version plus the settings payload.
///
/// The version is written but not enforced on read: fields are optional and
/// unknown keys are dropped, so a future file degrades instead of failing.
/// Bump [`SettingsStore::CURRENT_VERSION`] when a change cannot be expressed
/// that way, and migrate on read against this field then.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    settings: Settings,
}

/// JSON-backed settings on disk.
///
/// Loading a missing file is not an error: a first run starts from the
/// defaults, which is what an empty file would produce anyway.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    settings: Settings,
}

impl SettingsStore {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            settings: Settings::default(),
        }
    }

    /// Default location: `~/Library/Application Support/sshdeck/settings.json`
    /// on macOS, `$XDG_CONFIG_HOME/sshdeck/settings.json` elsewhere.
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
            .join("settings.json")
    }

    pub fn at_default_path() -> Self {
        Self::new(Self::default_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    /// Reads settings from disk. A missing file yields the defaults.
    pub fn load(&mut self) -> Result<(), SettingsError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.settings = Settings::default();
                return Ok(());
            }
            Err(source) => {
                return Err(SettingsError::Read {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|source| SettingsError::Parse {
                path: self.path.clone(),
                source,
            })?;
        self.settings = envelope.settings;
        Ok(())
    }

    /// Writes settings atomically (temp file + rename) so an interrupted write
    /// cannot leave a truncated file behind, with `0600` permissions on Unix.
    pub fn save(&self) -> Result<(), SettingsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SettingsError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let envelope = Envelope {
            version: Self::CURRENT_VERSION,
            settings: self.settings.clone(),
        };
        let mut payload =
            serde_json::to_vec_pretty(&envelope).map_err(|source| SettingsError::Write {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        payload.push(b'\n');

        let temp = self.path.with_extension("json.tmp");
        write_private(&temp, &payload).map_err(|source| SettingsError::Write {
            path: temp.clone(),
            source,
        })?;
        std::fs::rename(&temp, &self.path).map_err(|source| SettingsError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

/// Writes `bytes` to `path`, creating it with `0600` permissions on Unix.
///
/// The temp file is the only place a partial write can land, and it is never
/// world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test scratch directory, named so parallel tests cannot collide.
    fn temp_dir(name: &str) -> PathBuf {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("sshdeck-config-{pid}-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn defaults_match_the_documented_budget() {
        let settings = Settings::default();
        assert_eq!(settings.font_size(), 13.0);
        assert_eq!(settings.scrollback_lines(), 10_000);
        assert!(settings.cursor_blink());
        assert!(settings.theme_mode().is_dark());
        assert!(settings.confirm_on_close());
        assert!(!settings.compact());
        assert!(!settings.show_experimental());
        assert!(!settings.autocomplete_enabled());
        // The cap is the memory guard in docs/BUDGET.md; changing it is a
        // deliberate act, so fail loudly here too.
        assert_eq!(MAX_SCROLLBACK, 10_000);
        assert_eq!(DEFAULT_SCROLLBACK, MAX_SCROLLBACK);
    }

    #[test]
    fn missing_file_loads_as_defaults() {
        let dir = temp_dir("missing");
        let path = dir.join("settings.json");
        let mut store = SettingsStore::new(&path);
        store.load().expect("a missing file is not an error");
        assert_eq!(store.settings(), &Settings::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let dir = temp_dir("round-trip");
        let path = dir.join("settings.json");

        let mut store = SettingsStore::new(&path);
        store.settings_mut().set_font_size(19.5);
        store.settings_mut().set_scrollback_lines(777);
        store.settings_mut().set_cursor_blink(false);
        store.settings_mut().set_theme_mode(ThemeMode::Light);
        store.settings_mut().set_confirm_on_close(false);
        store.settings_mut().set_compact(true);
        store.settings_mut().set_show_experimental(true);
        store.settings_mut().set_autocomplete_enabled(true);
        store.save().expect("save succeeds");

        let mut reloaded = SettingsStore::new(&path);
        reloaded.load().expect("reload succeeds");
        let settings = reloaded.settings();
        assert_eq!(settings.font_size(), 19.5);
        assert_eq!(settings.scrollback_lines(), 777);
        assert!(!settings.cursor_blink());
        assert_eq!(settings.theme_mode(), ThemeMode::Light);
        assert!(!settings.confirm_on_close());
        assert!(settings.compact());
        assert!(settings.show_experimental());
        assert!(settings.autocomplete_enabled());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn font_size_clamps_into_range() {
        assert_eq!(clamp_font_size(4.0), MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(999.0), MAX_FONT_SIZE);
        assert_eq!(clamp_font_size(13.0), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(0.0), MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(-4.0), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(f64::NAN), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(f64::INFINITY), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn scrollback_clamps_zero_and_max_into_range() {
        assert_eq!(clamp_scrollback(0.0), MIN_SCROLLBACK);
        assert_eq!(clamp_scrollback(usize::MAX as f64), MAX_SCROLLBACK);
        assert_eq!(clamp_scrollback(1_000_000.0), MAX_SCROLLBACK);
        assert_eq!(clamp_scrollback(10_000.0), DEFAULT_SCROLLBACK);
        assert_eq!(clamp_scrollback(-5.0), DEFAULT_SCROLLBACK);
        assert_eq!(clamp_scrollback(f64::NAN), DEFAULT_SCROLLBACK);
    }

    #[test]
    fn negative_and_nan_json_degrade_to_defaults() {
        // A negative number and a quoted non-finite value both reach the same
        // clamped default instead of failing or panicking.
        let settings: Settings = serde_json::from_str(
            r#"{
                "font_size": -4.5,
                "scrollback_lines": "NaN",
                "cursor_blink": false
            }"#,
        )
        .expect("a negative and a non-finite value load, they do not fail");

        assert_eq!(settings.font_size(), DEFAULT_FONT_SIZE);
        assert_eq!(settings.scrollback_lines(), DEFAULT_SCROLLBACK);
        // Well-formed fields beside the bad ones still stick.
        assert!(!settings.cursor_blink());

        // `null` on a numeric field means "not set", so it degrades too.
        let nulled: Settings =
            serde_json::from_str(r#"{ "font_size": null }"#).expect("null is not set");
        assert_eq!(nulled.font_size(), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn unknown_json_fields_are_ignored() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "font_size": 14.0,
                "future_toggle": true,
                "appearance": { "nested": [1, 2, 3] }
            }"#,
        )
        .expect("unknown fields do not fail loading");
        assert_eq!(settings.font_size(), 14.0);
        assert_eq!(settings.scrollback_lines(), DEFAULT_SCROLLBACK);
    }

    #[test]
    fn future_envelope_still_loads_and_is_rewritten_current() {
        let dir = temp_dir("future");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"version": 99, "settings": {"font_size": 15.0, "unknown": 1}}"#,
        )
        .expect("write future file");

        let mut store = SettingsStore::new(&path);
        store.load().expect("a file from a newer build still loads");
        assert_eq!(store.settings().font_size(), 15.0);

        // The next save stamps the version this build knows.
        store.save().expect("save succeeds");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("\"version\": 1"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_returns_a_typed_error() {
        let dir = temp_dir("corrupt");
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ \"version\": 1, \"settings\": {").expect("write corrupt file");

        let mut store = SettingsStore::new(&path);
        let err = store.load().expect_err("a truncated file is an error");
        assert!(matches!(err, SettingsError::Parse { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_save_leaves_no_tmp_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("settings.json");

        SettingsStore::new(&path).save().expect("save succeeds");

        assert!(path.exists(), "the real file is written");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed, never left behind"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("permissions");
        let path = dir.join("settings.json");
        SettingsStore::new(&path).save().expect("save succeeds");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "group/other bits must be clear");

        std::fs::remove_dir_all(&dir).ok();
    }
}
