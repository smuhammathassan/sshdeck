//! Shell autocompletion for sshdeck, driven by CLI tool specifications.
//!
//! [`complete`] is pure: a command-line buffer, a byte cursor and a [`SpecSet`]
//! go in, ranked [`Candidate`]s come out. No UI, no network, no shell.
//!
//! # Spec schema and provenance
//!
//! The on-disk schema mirrors the public TypeScript interfaces published in
//! `@withfig/autocomplete-types` (`Fig.Subcommand`, `Fig.Option`, `Fig.Arg`,
//! `Fig.Suggestion`) from the [withfig/autocomplete] project, which is **MIT
//! licensed**. Only the static subset is read:
//!
//! | Field | On | Meaning |
//! | --- | --- | --- |
//! | `name` | subcommand, option, suggestion | string, or array of aliases |
//! | `description` | subcommand, option, arg, suggestion | display text |
//! | `subcommands` | subcommand | nested commands (recursive) |
//! | `options` | subcommand | flags / options |
//! | `args` | option, or object-or-array on a subcommand | argument slots |
//! | `suggestions` | arg | known static argument values |
//!
//! Verified against `@withfig/autocomplete-types@1.31.0` (`index.d.ts`, the
//! `Fig.*` declarations) and real upstream files: `src/cat.ts`, `src/echo.ts`
//! and `src/docker.ts` on the `master` branch of `withfig/autocomplete`, plus
//! the compiled `build/cat.js` in `@withfig/autocomplete@2.692.3`.
//!
//! Upstream ships compiled **ESM JavaScript, not JSON** — verified: that
//! package contains 1486 `.js` files, a `build/index.json` that is only a list
//! of spec *names*, and no spec bodies as JSON. The JSON this crate loads is
//! therefore sshdeck's own static serialisation of the schema above; the
//! upstream-JS-to-JSON conversion is out of scope here `(unconfirmed)` beyond
//! the field names. Nothing from the upstream spec corpus is vendored: the
//! files under `tests/specs/` are hand-written for tests only.
//!
//! [withfig/autocomplete]: https://github.com/withfig/autocomplete
//!
//! # Deliberate limits (`ponytail:`)
//!
//! The ceiling is a static, offline engine. Not handled, with the upgrade path:
//! shell quoting/escaping (needs a real tokenizer), `generators` / `loadSpec` /
//! `generateSpec` and arg templates like `filepaths` (need a shell and the
//! network), `isPersistent` flag inheritance, `hidden`, `requiresEquals` /
//! `--opt=value` argument form, and completing the command name itself.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::Path;

use serde_json::Value;

/// What a [`Candidate`] inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// A subcommand of the resolved command.
    Subcommand,
    /// A flag of the resolved command.
    Flag,
    /// A known value for the argument of a flag.
    FlagArgument,
}

/// One completion suggestion for the current buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    name: String,
    description: Option<String>,
    kind: CandidateKind,
    replace: Range<usize>,
}

impl Candidate {
    /// The text to insert.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Display text for the suggestion, if the spec provides one.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Which kind of suggestion this is.
    #[must_use]
    pub fn kind(&self) -> CandidateKind {
        self.kind
    }

    /// Byte range of the buffer this candidate replaces, so a caller can
    /// splice [`Candidate::name`] in place of `buffer[replace]`.
    #[must_use]
    pub fn replace(&self) -> Range<usize> {
        self.replace.clone()
    }
}

/// A command in a completion spec: the root spec, or a nested subcommand.
#[derive(Debug, Clone, Default)]
pub struct Command {
    names: Vec<String>,
    description: Option<String>,
    subcommands: Vec<Command>,
    flags: Vec<Flag>,
}

impl Command {
    /// Parses one spec document. Returns `None` for malformed JSON or a spec
    /// with no usable `name`; never panics.
    #[must_use]
    pub fn parse(json: &str) -> Option<Self> {
        Self::from_value(&serde_json::from_str::<Value>(json).ok()?)
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let names = names(object.get("name")?);
        if names.is_empty() {
            return None;
        }
        Some(Self {
            names,
            description: text(object.get("description")),
            subcommands: list(object.get("subcommands"))
                .iter()
                .filter_map(Self::from_value)
                .collect(),
            flags: list(object.get("options"))
                .iter()
                .filter_map(Flag::from_value)
                .collect(),
        })
    }

    /// Every name of this command; the first is canonical.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The canonical name, or `""` for a spec that somehow has none.
    #[must_use]
    pub fn name(&self) -> &str {
        self.names.first().map_or("", String::as_str)
    }

    /// Description shown alongside the command.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Nested subcommands.
    #[must_use]
    pub fn subcommands(&self) -> &[Command] {
        &self.subcommands
    }

    /// Flags (options) accepted by this command.
    #[must_use]
    pub fn flags(&self) -> &[Flag] {
        &self.flags
    }

    fn find_subcommand(&self, token: &str) -> Option<&Command> {
        self.subcommands.iter().find(|command| {
            command
                .names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(token))
        })
    }

    fn find_flag(&self, token: &str) -> Option<&Flag> {
        self.flags
            .iter()
            .find(|flag| flag.names.iter().any(|name| name == token))
    }
}

/// A flag / option, with its short and long names grouped as one item.
#[derive(Debug, Clone, Default)]
pub struct Flag {
    names: Vec<String>,
    description: Option<String>,
    args: Vec<Arg>,
}

impl Flag {
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let names = names(object.get("name")?);
        if names.is_empty() {
            return None;
        }
        Some(Self {
            names,
            description: text(object.get("description")),
            args: args_value(object.get("args")),
        })
    }

    /// Every spelling of this flag, e.g. `["-m", "--message"]`.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The first spelling, or `""` for a flag that somehow has none.
    #[must_use]
    pub fn name(&self) -> &str {
        self.names.first().map_or("", String::as_str)
    }

    /// Description shown alongside the flag.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Argument slots for this flag; non-empty means it consumes a value.
    #[must_use]
    pub fn args(&self) -> &[Arg] {
        &self.args
    }

    /// Whether this flag consumes a value.
    #[must_use]
    pub fn takes_argument(&self) -> bool {
        !self.args.is_empty()
    }
}

/// An argument slot, holding any statically known values.
#[derive(Debug, Clone, Default)]
pub struct Arg {
    name: Option<String>,
    description: Option<String>,
    suggestions: Vec<Suggestion>,
}

impl Arg {
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            name: text(object.get("name")),
            description: text(object.get("description")),
            suggestions: list(object.get("suggestions"))
                .iter()
                .filter_map(Suggestion::from_value)
                .collect(),
        })
    }

    /// Human-readable argument name, e.g. `"message"`.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Description shown while the argument is being typed.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Static values the spec knows for this argument.
    #[must_use]
    pub fn suggestions(&self) -> &[Suggestion] {
        &self.suggestions
    }
}

/// A statically known argument value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    name: String,
    description: Option<String>,
}

impl Suggestion {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(name) => Some(Self {
                name: name.clone(),
                description: None,
            }),
            Value::Object(object) => {
                let name = names(object.get("name")?).into_iter().next()?;
                Some(Self {
                    name,
                    description: text(object.get("description")),
                })
            }
            _ => None,
        }
    }

    /// The value to insert.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Description shown alongside the value.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }
}

/// A set of specs, looked up by command name.
#[derive(Debug, Default)]
pub struct SpecSet {
    by_command: HashMap<String, Command>,
}

impl SpecSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses one spec and inserts it under each of its names. Returns whether
    /// the spec was usable.
    pub fn insert_json(&mut self, json: &str) -> bool {
        match Command::parse(json) {
            Some(command) => {
                self.insert(command);
                true
            }
            None => false,
        }
    }

    /// Loads every `*.json` file in `dir`, keyed by command name.
    ///
    /// Files that cannot be read or parsed are skipped so one bad spec does not
    /// hide the rest. Errors reading the directory itself are returned.
    pub fn load_dir(dir: &Path) -> io::Result<Self> {
        let mut set = Self::new();
        for entry in std::fs::read_dir(dir)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if let Ok(json) = std::fs::read_to_string(&path) {
                set.insert_json(&json);
            }
        }
        Ok(set)
    }

    /// The spec for `command`, matched case-insensitively against every name.
    #[must_use]
    pub fn get(&self, command: &str) -> Option<&Command> {
        self.by_command.get(&command.to_ascii_lowercase())
    }

    /// Number of distinct command names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_command.len()
    }

    /// Whether no spec has been loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_command.is_empty()
    }

    fn insert(&mut self, command: Command) {
        for name in command.names() {
            self.by_command
                .insert(name.to_ascii_lowercase(), command.clone());
        }
    }
}

/// Suggests completions for `buffer` with the cursor at byte offset `cursor`.
///
/// Resolves the command, walks into subcommands, and returns matching
/// subcommands, flags and known flag-argument values. A flag already present in
/// the buffer is never suggested again. An empty result just means "nothing to
/// offer": an unknown command, a cursor mid-token on the command itself, or a
/// buffer with no spec all yield `vec![]`, not an error.
///
/// `cursor` is clamped to the buffer, and moved back to a char boundary if it
/// lands inside a multi-byte character. Candidates are ranked with exact prefix
/// matches before substring matches; otherwise spec order is preserved.
#[must_use]
pub fn complete(specs: &SpecSet, buffer: &str, cursor: usize) -> Vec<Candidate> {
    let cursor = clamp_boundary(buffer, cursor);
    let head = &buffer[..cursor];
    let tokens = tokenize(head);
    let at_boundary = head.chars().next_back().is_none_or(char::is_whitespace);

    let (partial, replace, resolved) = if at_boundary {
        ("", cursor..cursor, tokens.as_slice())
    } else if let Some((last, rest)) = tokens.split_last() {
        (last.text, last.range.clone(), rest)
    } else {
        ("", cursor..cursor, tokens.as_slice())
    };

    // Nothing but the command token so far: no completion (we do not complete
    // the command name itself).
    if resolved.is_empty() {
        return Vec::new();
    }

    let Some(root) = specs.get(command_name(resolved[0].text)) else {
        return Vec::new();
    };
    let Some(node) = resolve(root, &resolved[1..]) else {
        return Vec::new();
    };

    // The token before the cursor opens a value slot: complete its argument.
    if let Some(flag) = resolved.last().and_then(|token| node.find_flag(token.text)) {
        if flag.takes_argument() {
            return rank(flag_argument_candidates(flag, partial, &replace));
        }
    }

    rank(gather(node, resolved, partial, &replace))
}

/// A whitespace-delimited token with its byte range in the buffer.
struct Token<'a> {
    range: Range<usize>,
    text: &'a str,
}

fn tokenize(head: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, character) in head.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                tokens.push(Token {
                    range: start..index,
                    text: &head[start..index],
                });
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        tokens.push(Token {
            range: start..head.len(),
            text: &head[start..],
        });
    }
    tokens
}

/// Clamps `cursor` into `buffer` and back to the nearest char boundary.
fn clamp_boundary(buffer: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(buffer.len());
    while !buffer.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// The bare command name, dropping any directory prefix (`/usr/bin/git`).
fn command_name(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// A `name` field: one string, or an array of alias strings. Anything else is
/// ignored rather than failing the whole entry.
fn names(value: &Value) -> Vec<String> {
    match value {
        Value::String(name) => vec![name.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// A string-valued field such as `description`.
fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(String::from)
}

/// An array field, or an empty slice when it is missing or malformed.
fn list(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[])
}

/// An `args` field: a single arg object or an array of them.
fn args_value(value: Option<&Value>) -> Vec<Arg> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(Arg::from_value).collect(),
        Some(object @ Value::Object(_)) => Arg::from_value(object).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Walks `tokens` from `root` into subcommands, skipping flags and, where
/// known, the values they consume. `None` means a positional argument was
/// reached, after which there is nothing command-shaped left to suggest.
fn resolve<'a>(root: &'a Command, tokens: &[Token<'_>]) -> Option<&'a Command> {
    let mut node = root;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].text;
        if token.starts_with('-') {
            if let Some(flag) = node.find_flag(token) {
                if flag.takes_argument() && index + 1 < tokens.len() {
                    index += 2;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        node = node.find_subcommand(token)?;
        index += 1;
    }
    Some(node)
}

/// Subcommands and flags of `node`, minus flags already present, filtered by
/// the token under the cursor.
fn gather(
    node: &Command,
    tokens: &[Token<'_>],
    partial: &str,
    replace: &Range<usize>,
) -> Vec<(u8, Candidate)> {
    let mut out = Vec::new();
    // A `-` partial means flags; anything else means subcommands. This keeps a
    // bare `cmd ` listing subcommands rather than flooding the user with flags,
    // which stay one keystroke away.
    let typing_flag = partial.starts_with('-');

    if !typing_flag {
        for subcommand in &node.subcommands {
            // One candidate per subcommand: the best-matching alias, or the
            // canonical name when nothing is typed yet.
            let mut best: Option<(u8, &str)> = None;
            for name in subcommand.names() {
                if let Some(score) = score(name, partial) {
                    if best.is_none_or(|(best, _)| score < best) {
                        best = Some((score, name.as_str()));
                    }
                }
            }
            if let Some((score, name)) = best {
                out.push((
                    score,
                    Candidate {
                        name: name.to_owned(),
                        description: subcommand.description.clone(),
                        kind: CandidateKind::Subcommand,
                        replace: replace.clone(),
                    },
                ));
            }
        }
    }

    if typing_flag {
        for flag in &node.flags {
            if flag_present(flag, tokens) {
                continue;
            }
            // One candidate per spelling, so a short and a long form both show.
            for name in flag.names() {
                if let Some(score) = score(name, partial) {
                    out.push((
                        score,
                        Candidate {
                            name: name.clone(),
                            description: flag.description.clone(),
                            kind: CandidateKind::Flag,
                            replace: replace.clone(),
                        },
                    ));
                }
            }
        }
    }

    out
}

/// Static values for the flag whose value slot the cursor is in.
fn flag_argument_candidates(
    flag: &Flag,
    partial: &str,
    replace: &Range<usize>,
) -> Vec<(u8, Candidate)> {
    let mut out = Vec::new();
    for arg in &flag.args {
        for suggestion in &arg.suggestions {
            if let Some(score) = score(suggestion.name(), partial) {
                out.push((
                    score,
                    Candidate {
                        name: suggestion.name.clone(),
                        description: suggestion
                            .description
                            .clone()
                            .or_else(|| arg.description.clone()),
                        kind: CandidateKind::FlagArgument,
                        replace: replace.clone(),
                    },
                ));
            }
        }
    }
    out
}

/// Whether any spelling of `flag` already appears in the buffer.
fn flag_present(flag: &Flag, tokens: &[Token<'_>]) -> bool {
    flag.names
        .iter()
        .any(|name| tokens.iter().any(|token| token.text == name))
}

/// `0` for a case-insensitive prefix match, `1` for a substring match, `None`
/// for no match at all. An empty partial matches everything.
fn score(name: &str, partial: &str) -> Option<u8> {
    if partial.is_empty() {
        return Some(0);
    }
    let name = name.to_ascii_lowercase();
    let partial = partial.to_ascii_lowercase();
    if name.starts_with(&partial) {
        Some(0)
    } else if name.contains(&partial) {
        Some(1)
    } else {
        None
    }
}

/// Ranks by score, preserving spec order within a score, and drops duplicate
/// names (a subcommand and a flag can collide in theory).
fn rank(mut scored: Vec<(u8, Candidate)>) -> Vec<Candidate> {
    scored.sort_by_key(|(score, _)| *score);
    let mut seen = HashSet::new();
    scored
        .into_iter()
        .filter_map(|(_, candidate)| seen.insert(candidate.name.clone()).then_some(candidate))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT: &str = include_str!("../tests/specs/git.json");
    const DOCKER: &str = include_str!("../tests/specs/docker.json");

    fn set(json: &str) -> SpecSet {
        let mut set = SpecSet::new();
        assert!(set.insert_json(json), "fixture should parse");
        set
    }

    fn names(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(Candidate::name).collect()
    }

    #[test]
    fn parse_small_spec_finds_subcommands() {
        let spec = Command::parse(GIT).expect("git spec parses");
        assert_eq!(spec.name(), "git");
        assert_eq!(
            spec.description(),
            Some("Distributed version control system")
        );
        let subcommands: Vec<&str> = spec.subcommands().iter().map(Command::name).collect();
        assert!(subcommands.contains(&"commit"));
        assert!(subcommands.contains(&"checkout"));

        let commit = spec
            .subcommands()
            .iter()
            .find(|command| command.name() == "commit")
            .expect("commit subcommand");
        let message = commit
            .flags()
            .iter()
            .find(|flag| flag.name() == "-m")
            .expect("message flag");
        assert_eq!(message.names(), &["-m", "--message"]);
        assert!(message.takes_argument());
    }

    #[test]
    fn bare_command_lists_subcommands() {
        let specs = set(GIT);
        let candidates = complete(&specs, "git ", 4);
        let found = names(&candidates);
        assert!(found.contains(&"commit"));
        assert!(found.contains(&"checkout"));
        assert!(found.contains(&"status"));
        // Subcommands only: flags need the `-` prefix to be interesting.
        assert!(candidates
            .iter()
            .all(|candidate| candidate.kind() == CandidateKind::Subcommand));
    }

    #[test]
    fn partial_flag_filters_to_matching_flags() {
        let specs = set(GIT);
        let candidates = complete(&specs, "git commit --am", 15);
        assert_eq!(names(&candidates), ["--amend"]);
        assert_eq!(candidates[0].kind(), CandidateKind::Flag);
        assert_eq!(
            candidates[0].description(),
            Some("Replace the tip of the current branch")
        );

        // Filtering is case-insensitive.
        let upper = complete(&specs, "git commit --AM", 15);
        assert_eq!(names(&upper), ["--amend"]);
    }

    #[test]
    fn flag_already_present_is_not_suggested_again() {
        let specs = set(GIT);
        let candidates = complete(&specs, "git commit -m hi -", 18);
        let found = names(&candidates);
        assert!(!found.contains(&"-m"), "present flag suggested again");
        assert!(
            !found.contains(&"--message"),
            "alias of present flag suggested"
        );
        assert!(found.contains(&"-a"));
        assert!(found.contains(&"--amend"));
    }

    #[test]
    fn deep_completion_resolves_subcommand_flags() {
        let specs = set(DOCKER);
        let candidates = complete(&specs, "docker image ls --", 18);
        assert_eq!(names(&candidates), ["--digests"]);

        // A subcommand alias resolves too.
        let alias = complete(&specs, "docker c ls -", 13);
        assert_eq!(names(&alias), ["-a"]);
    }

    #[test]
    fn unknown_command_yields_no_candidates() {
        let specs = set(GIT);
        assert!(complete(&specs, "nope ", 5).is_empty());
        assert!(complete(&specs, "", 0).is_empty());
        assert!(complete(&specs, "git", 3).is_empty());
    }

    #[test]
    fn replacement_range_covers_partial_token_mid_buffer() {
        let specs = set(GIT);
        // Cursor sits inside `che`; `--orphan` after it must be ignored.
        let candidates = complete(&specs, "git che --orphan", 7);
        assert_eq!(names(&candidates), ["checkout"]);
        assert_eq!(candidates[0].replace(), 4..7);
        assert_eq!(&"git che --orphan"[candidates[0].replace()], "che");

        // At a token boundary the range is empty and sits at the cursor.
        let fresh = complete(&specs, "git ", 4);
        assert!(!fresh.is_empty());
        assert!(fresh.iter().all(|candidate| candidate.replace() == (4..4)));
    }

    #[test]
    fn prefix_matches_rank_above_substring_matches() {
        let specs = set(GIT);
        // `add` is a prefix match; `status` only contains the `a`.
        let candidates = complete(&specs, "git a", 5);
        assert_eq!(names(&candidates), ["add", "status"]);
    }

    #[test]
    fn flag_argument_values_are_suggested() {
        let specs = set(GIT);
        let candidates = complete(&specs, "git --color a", 13);
        assert_eq!(names(&candidates), ["always", "auto"]);
        assert_eq!(candidates[0].kind(), CandidateKind::FlagArgument);
        assert_eq!(candidates[0].description(), Some("When to use color"));
    }

    #[test]
    fn malformed_entries_are_skipped() {
        let json = r#"{
            "name": "tool",
            "subcommands": [
                { "name": "good", "description": "kept" },
                { "description": "missing name" },
                { "name": 7 },
                "not an object",
                null
            ],
            "options": [
                { "name": "--ok" },
                { "description": "missing name" },
                null
            ]
        }"#;
        let spec = Command::parse(json).expect("spec parses");
        assert_eq!(spec.subcommands().len(), 1);
        assert_eq!(spec.subcommands()[0].name(), "good");
        assert_eq!(spec.flags().len(), 1);
        assert_eq!(spec.flags()[0].name(), "--ok");

        assert!(Command::parse("{ not json").is_none());
        assert!(Command::parse(r#"{"description": "no name"}"#).is_none());
    }

    #[test]
    fn loads_specs_from_directory_and_skips_unreadable_ones() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/specs");
        let specs = SpecSet::load_dir(&dir).expect("fixture directory readable");
        assert_eq!(specs.len(), 3);
        assert!(specs.get("git").is_some());
        assert!(specs.get("GIT").is_some());
        assert!(specs.get("docker").is_some());
        assert!(specs.get("cargo").is_some());
        // `broken.json` is invalid JSON and must not have stopped the load.
        assert!(specs.get("broken").is_none());
    }
}
