//! Snippets for sshdeck: saved commands with variable substitution.
//!
//! Pure logic, no UI, no network, no gpui: a [`Snippet`] is a label plus a
//! command template, and [`Snippet::expand`] turns the template plus a map of
//! variable values into the final command text to send to a session.
//!
//! # Template syntax
//!
//! A template is literal text with `{{name}}` placeholders. The name is
//! trimmed, so `{{ host }}` and `{{host}}` are the same placeholder.
//!
//! The set of variables a snippet accepts is declared explicitly as
//! [`Variable`] values, each with an optional default. A placeholder that is not
//! declared is an error ([`SnippetError::UnknownVariable`]); a declared
//! placeholder with no supplied value and no default is an error
//! ([`SnippetError::MissingVariable`]). Values are inserted literally and the
//! substituted text is never re-scanned, so a value that itself contains
//! `{{...}}` cannot trigger a second round of expansion (a command-injection
//! foot-gun).
//!
//! ```
//! use sshdeck_snippets::{Snippet, Variable};
//! use std::collections::HashMap;
//!
//! let snippet = Snippet::new("Deploy", "ssh {{user}}@{{host}}")
//!     .expect("valid")
//!     .with_variables(vec![Variable::with_default("user", "deploy"), Variable::new("host")]);
//! let mut values = HashMap::new();
//! values.insert("host".to_string(), "10.0.0.1".to_string());
//! assert_eq!(snippet.expand(&values).expect("expands"), "ssh deploy@10.0.0.1");
//! ```
//!
//! # Persistence
//!
//! [`SnippetStore`] mirrors `sshdeck-core`'s `HostStore`: a missing file loads as
//! empty, and `save` is atomic (temp file + rename).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Stable identifier for a snippet, independent of its label.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnippetId(String);

impl SnippetId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Derives an id from a label, mirroring `HostId::from_label`.
    pub fn from_label(label: &str) -> Self {
        let slug: String = label
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        let slug = slug.trim_matches('-').to_string();
        Self(if slug.is_empty() {
            "snippet".to_string()
        } else {
            slug
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnippetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A declared template variable and its optional fallback value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<String>,
}

impl Variable {
    /// A variable the caller must supply a value for.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            default: None,
        }
    }

    /// A variable that falls back to `default` when no value is supplied.
    pub fn with_default(name: impl Into<String>, default: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            default: Some(default.into()),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn default_value(&self) -> Option<&str> {
        self.default.as_deref()
    }
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    Placeholder(String),
}

/// A parsed command template.
///
/// Parsing extracts the `{{name}}` placeholders once; expansion walks the
/// resulting tokens, so substituted text is never re-scanned. Serialized as its
/// raw string, and parsed on the way back in, so a value loaded from disk can
/// never hold an unbalanced template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Template {
    raw: String,
    tokens: Vec<Token>,
    variables: Vec<String>,
}

impl Template {
    /// Parses `raw`, rejecting an unclosed `{{` and an empty placeholder.
    pub fn parse(raw: impl Into<String>) -> Result<Self, SnippetError> {
        let raw = raw.into();
        let mut tokens = Vec::new();
        let mut variables = Vec::new();
        let mut rest = raw.as_str();
        let mut consumed = 0usize;

        while let Some(start) = rest.find("{{") {
            if start > 0 {
                tokens.push(Token::Literal(rest[..start].to_string()));
            }
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return Err(SnippetError::UnbalancedDelimiter {
                    offset: consumed + start,
                });
            };
            let name = after[..end].trim();
            if name.is_empty() {
                return Err(SnippetError::EmptyPlaceholder {
                    offset: consumed + start,
                });
            }
            // A nested opener means the delimiter is unbalanced, not a name.
            if name.contains("{{") {
                return Err(SnippetError::UnbalancedDelimiter {
                    offset: consumed + start,
                });
            }
            tokens.push(Token::Placeholder(name.to_string()));
            if !variables.iter().any(|v| v == name) {
                variables.push(name.to_string());
            }
            let advance = start + 2 + end + 2;
            consumed += advance;
            rest = &rest[advance..];
        }
        if !rest.is_empty() {
            tokens.push(Token::Literal(rest.to_string()));
        }

        Ok(Self {
            raw,
            tokens,
            variables,
        })
    }

    /// The template exactly as written.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The placeholders present in the template, in first-appearance order,
    /// de-duplicated. This is what a UI prompts for.
    pub fn variables(&self) -> &[String] {
        &self.variables
    }

    /// Substitutes every placeholder.
    ///
    /// A placeholder must appear in `declared`; then `values` wins, then the
    /// variable's default, otherwise it is an error. Values are pushed into the
    /// output verbatim, never re-parsed.
    pub fn expand(
        &self,
        declared: &[Variable],
        values: &HashMap<String, String>,
    ) -> Result<String, SnippetError> {
        let mut out = String::with_capacity(self.raw.len());
        for token in &self.tokens {
            match token {
                Token::Literal(text) => out.push_str(text),
                Token::Placeholder(name) => {
                    let Some(variable) = declared.iter().find(|v| v.name == *name) else {
                        return Err(SnippetError::UnknownVariable { name: name.clone() });
                    };
                    if let Some(value) = values.get(name.as_str()) {
                        out.push_str(value);
                    } else if let Some(default) = variable.default.as_deref() {
                        out.push_str(default);
                    } else {
                        return Err(SnippetError::MissingVariable { name: name.clone() });
                    }
                }
            }
        }
        Ok(out)
    }
}

impl TryFrom<String> for Template {
    type Error = SnippetError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(raw)
    }
}

impl From<Template> for String {
    fn from(template: Template) -> Self {
        template.raw
    }
}

/// A saved command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    id: SnippetId,
    label: String,
    template: Template,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_working_dir: Option<PathBuf>,
    #[serde(default)]
    variables: Vec<Variable>,
}

impl Snippet {
    /// Builds a snippet. Empty labels and templates are rejected here, at the
    /// trust boundary, and the template must parse.
    pub fn new(
        label: impl Into<String>,
        template: impl Into<String>,
    ) -> Result<Self, SnippetError> {
        let label = label.into();
        if label.trim().is_empty() {
            return Err(SnippetError::EmptyLabel);
        }
        let raw = template.into();
        if raw.trim().is_empty() {
            return Err(SnippetError::EmptyTemplate);
        }
        Ok(Self {
            id: SnippetId::from_label(&label),
            label,
            template: Template::parse(raw)?,
            description: None,
            tags: Vec::new(),
            default_working_dir: None,
            variables: Vec::new(),
        })
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.default_working_dir = Some(dir.into());
        self
    }

    /// Declares the variables the template may reference.
    pub fn with_variables(mut self, variables: Vec<Variable>) -> Self {
        self.variables = variables;
        self
    }

    /// Re-checks the invariants that a loaded file could otherwise violate.
    pub fn validate(&self) -> Result<(), SnippetError> {
        if self.label.trim().is_empty() {
            return Err(SnippetError::EmptyLabel);
        }
        if self.template.raw().trim().is_empty() {
            return Err(SnippetError::EmptyTemplate);
        }
        Ok(())
    }

    pub fn id(&self) -> &SnippetId {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    pub fn default_working_dir(&self) -> Option<&Path> {
        self.default_working_dir.as_deref()
    }

    pub fn template(&self) -> &Template {
        &self.template
    }

    /// The template's placeholders, in first-appearance order.
    pub fn variables(&self) -> &[String] {
        self.template.variables()
    }

    /// The declared variables, with their defaults, for a UI to prompt with.
    pub fn declared_variables(&self) -> &[Variable] {
        &self.variables
    }

    /// Expands the template against `values`, falling back to declared defaults.
    pub fn expand(&self, values: &HashMap<String, String>) -> Result<String, SnippetError> {
        self.template.expand(&self.variables, values)
    }
}

/// Everything that can go wrong building or expanding a snippet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnippetError {
    #[error("snippet label must not be empty")]
    EmptyLabel,
    #[error("snippet template must not be empty")]
    EmptyTemplate,
    #[error("unbalanced placeholder delimiter in template at byte {offset}")]
    UnbalancedDelimiter { offset: usize },
    #[error("empty placeholder in template at byte {offset}")]
    EmptyPlaceholder { offset: usize },
    #[error("template references undeclared variable {{{name}}}")]
    UnknownVariable { name: String },
    #[error("no value for required variable {{{name}}}")]
    MissingVariable { name: String },
}

/// On-disk representation. `version` is carried so a future migration can see
/// what wrote the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SnippetFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    snippets: Vec<Snippet>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not read snippets at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write snippets at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("snippets at {path} are not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("snippet #{index} is invalid: {source}")]
    Invalid {
        index: usize,
        #[source]
        source: SnippetError,
    },
}

/// JSON-backed snippet collection on disk, mirroring `HostStore`.
///
/// Loading a missing file is not an error: a first run starts empty.
#[derive(Debug, Clone)]
pub struct SnippetStore {
    path: PathBuf,
    file: SnippetFile,
}

impl SnippetStore {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            file: SnippetFile::default(),
        }
    }

    /// Default location: `~/Library/Application Support/sshdeck/snippets.json`
    /// on macOS, `$XDG_CONFIG_HOME/sshdeck/snippets.json` elsewhere.
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
            .join("snippets.json")
    }

    pub fn at_default_path() -> Self {
        Self::new(Self::default_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snippets(&self) -> &[Snippet] {
        &self.file.snippets
    }

    pub fn snippets_mut(&mut self) -> &mut Vec<Snippet> {
        &mut self.file.snippets
    }

    pub fn len(&self) -> usize {
        self.file.snippets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.file.snippets.is_empty()
    }

    pub fn get(&self, id: &SnippetId) -> Option<&Snippet> {
        self.file.snippets.iter().find(|s| &s.id == id)
    }

    /// Reads the file from disk. A missing file yields an empty collection.
    pub fn load(&mut self) -> Result<(), StoreError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.file = SnippetFile::default();
                return Ok(());
            }
            Err(source) => {
                return Err(StoreError::Read {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        let file: SnippetFile =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Parse {
                path: self.path.clone(),
                source,
            })?;
        for (index, snippet) in file.snippets.iter().enumerate() {
            snippet
                .validate()
                .map_err(|source| StoreError::Invalid { index, source })?;
        }
        self.file = file;
        Ok(())
    }

    /// Writes atomically (temp file + rename) so an interrupted write cannot
    /// leave a truncated file behind.
    pub fn save(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut payload =
            serde_json::to_vec_pretty(&self.file).map_err(|source| StoreError::Write {
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

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sshdeck-snippets-{name}-{}", std::process::id()))
    }

    #[test]
    fn expand_substitutes_all_values() {
        let snippet = Snippet::new("Deploy", "ssh {{user}}@{{host}}")
            .expect("valid")
            .with_variables(vec![Variable::new("user"), Variable::new("host")]);

        let expanded = snippet
            .expand(&values(&[("user", "deploy"), ("host", "10.0.0.1")]))
            .expect("all variables supplied");
        assert_eq!(expanded, "ssh deploy@10.0.0.1");
    }

    #[test]
    fn expand_falls_back_to_default() {
        let snippet = Snippet::new("Logs", "tail -n {{lines}} /var/log/{{file}}")
            .expect("valid")
            .with_variables(vec![
                Variable::with_default("lines", "100"),
                Variable::new("file"),
            ]);

        let expanded = snippet
            .expand(&values(&[("file", "syslog")]))
            .expect("default fills the gap");
        assert_eq!(expanded, "tail -n 100 /var/log/syslog");
    }

    #[test]
    fn expand_missing_required_variable_is_an_error() {
        let snippet = Snippet::new("Who", "echo {{who}}")
            .expect("valid")
            .with_variables(vec![Variable::new("who")]);

        let error = snippet.expand(&values(&[])).expect_err("must fail");
        assert!(matches!(
            error,
            SnippetError::MissingVariable { ref name } if name == "who"
        ));
    }

    #[test]
    fn expand_unknown_placeholder_is_an_error() {
        let snippet = Snippet::new("Who", "echo {{who}}")
            .expect("valid")
            .with_variables(vec![Variable::new("name")]);

        let error = snippet
            .expand(&values(&[("who", "me")]))
            .expect_err("undeclared placeholder must fail even when a value exists");
        assert!(matches!(
            error,
            SnippetError::UnknownVariable { ref name } if name == "who"
        ));
    }

    #[test]
    fn unbalanced_delimiter_is_a_parse_error() {
        let error = Template::parse("echo {{oops").expect_err("must fail");
        assert!(matches!(error, SnippetError::UnbalancedDelimiter { .. }));
        assert!(matches!(
            Snippet::new("Broken", "echo }}oops"),
            Ok(_) // a stray closer is literal text, not a delimiter
        ));
        assert!(Snippet::new("Broken", "echo {{oops").is_err());
    }

    #[test]
    fn expansion_is_not_recursive() {
        let snippet = Snippet::new("Echo", "echo {{value}}")
            .expect("valid")
            .with_variables(vec![Variable::new("value")]);

        let expanded = snippet
            .expand(&values(&[("value", "{{value}}")]))
            .expect("literal insertion");
        assert_eq!(expanded, "echo {{value}}");
    }

    #[test]
    fn variables_reports_placeholders_present() {
        let template = Template::parse("{{alpha}} {{ beta }} {{alpha}}").expect("valid");
        let names: Vec<&str> = template.variables().iter().map(String::as_str).collect();
        assert_eq!(names, vec!["alpha", "beta"]);

        let snippet = Snippet::new("T", "{{alpha}} {{ beta }} {{alpha}}").expect("valid");
        let names: Vec<&str> = snippet.variables().iter().map(String::as_str).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn empty_label_or_template_is_rejected() {
        assert!(matches!(
            Snippet::new("", "ls"),
            Err(SnippetError::EmptyLabel)
        ));
        assert!(matches!(
            Snippet::new("   ", "ls"),
            Err(SnippetError::EmptyLabel)
        ));
        assert!(matches!(
            Snippet::new("List", "   "),
            Err(SnippetError::EmptyTemplate)
        ));
        assert!(Snippet::new("List", "ls -la").is_ok());
    }

    #[test]
    fn store_round_trips_through_disk() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("snippets.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = SnippetStore::new(&path);
        store.load().expect("missing file loads as empty");
        assert!(store.is_empty());

        let snippet = Snippet::new("Deploy", "ssh {{host}}")
            .expect("valid")
            .with_variables(vec![Variable::new("host")])
            .with_working_dir("/srv");
        store.snippets_mut().push(snippet);
        store.save().expect("save succeeds");

        let mut reloaded = SnippetStore::new(&path);
        reloaded.load().expect("reload succeeds");
        assert_eq!(reloaded.len(), 1);
        let snippet = &reloaded.snippets()[0];
        assert_eq!(snippet.label(), "Deploy");
        assert_eq!(snippet.template().raw(), "ssh {{host}}");
        assert_eq!(
            snippet.default_working_dir(),
            Some(Path::new("/srv")),
            "working dir survives JSON"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = temp_dir("missing");
        let path = dir.join("snippets.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = SnippetStore::new(&path);
        store.load().expect("missing file is not an error");
        assert!(store.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_save_leaves_no_temp_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("snippets.json");
        let _ = std::fs::remove_dir_all(&dir);

        let mut store = SnippetStore::new(&path);
        store.load().expect("missing file loads as empty");
        store
            .snippets_mut()
            .push(Snippet::new("List", "ls -la").expect("valid"));
        store.save().expect("save succeeds");

        assert!(path.exists(), "the real file is written");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed away"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
