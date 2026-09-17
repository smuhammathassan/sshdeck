//! Snippets pane.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `SnippetsPane::new(window, cx) -> Self`, plus `impl Render for SnippetsPane`.
//! The root view constructs it with `cx.new(|cx| SnippetsPane::new(window, cx))`.
//!
//! A `Snippet` is a label plus a `{{name}}` command template with an explicit
//! `Variable` list. The pane lists saved snippets, lets the user supply values
//! for the declared variables, expands through the crate's `expand` (surfacing
//! the typed `SnippetError` verbatim), and persists via `SnippetStore`.

use std::collections::HashMap;

use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, px, rgb, AnyElement, App, AppContext as _, ClipboardItem, Context, Div, Entity,
    FocusHandle, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window,
};
use sshdeck_snippets::{Snippet, SnippetError, SnippetId, SnippetStore, Variable};

/// The dark-blue glyph background for snippet cards, matching the keychain
/// cards in the reference. No theme token covers it, so it is hardcoded.
fn glyph_bg() -> Hsla {
    // #244a67 — a dark-blue rounded square, white icon on top.
    rgb(0x244a67).into()
}

/// Parses the variable declaration field: `name, other=default, third`.
///
/// An entry without `=` is a required variable (`Variable::new`); with `=` it
/// carries a default (`Variable::with_default`). Empty entries are dropped.
/// A declaration with an empty name is an error rather than a silent skip.
fn parse_variables_raw(raw: &str) -> Result<Vec<Variable>, String> {
    let mut out = Vec::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(out);
    }
    for part in trimmed.split(',') {
        let entry = part.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((name, default)) = entry.split_once('=') {
            let name = name.trim();
            let default = default.trim();
            if name.is_empty() {
                return Err(format!("variable name missing in \"{entry}\""));
            }
            // An empty default is treated as no default — `a=` means required.
            if default.is_empty() {
                out.push(Variable::new(name));
            } else {
                out.push(Variable::with_default(name, default));
            }
        } else {
            if entry.is_empty() {
                continue;
            }
            out.push(Variable::new(entry));
        }
    }
    // De-duplicate by name, keep first, so a typo `host, host=local` is not two.
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for variable in out {
        if seen.insert(variable.name().to_string()) {
            deduped.push(variable);
        }
    }
    Ok(deduped)
}

/// Turns a `Vec<Variable>` back into the comma form the field shows.
fn variables_to_string(variables: &[Variable]) -> String {
    variables
        .iter()
        .map(|variable| match variable.default_value() {
            Some(default) => format!("{}={}", variable.name(), default),
            None => variable.name().to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses a comma-separated tags field.
fn parse_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// The secondary line under a card title: description if present, otherwise the
/// template, otherwise the variable count.
fn secondary_line(snippet: &Snippet) -> String {
    if let Some(description) = snippet.description() {
        if !description.trim().is_empty() {
            return description.to_string();
        }
    }
    if !snippet.tags().is_empty() {
        return snippet.tags().join(", ");
    }
    let raw = snippet.template().raw().trim();
    if raw.len() > 64 {
        // Truncate long commands with an ellipsis rather than wrapping the card.
        format!("{}…", &raw[..64])
    } else if raw.is_empty() {
        let count = snippet.declared_variables().len();
        if count == 0 {
            "no variables".to_string()
        } else if count == 1 {
            "1 variable".to_string()
        } else {
            format!("{count} variables")
        }
    } else {
        raw.to_string()
    }
}

/// Snippets pane. Constructed by the root view.
pub struct SnippetsPane {
    store: SnippetStore,
    selected: Option<SnippetId>,
    /// Value inputs for the selected snippet, keyed by variable name.
    variable_inputs: HashMap<String, Entity<InputState>>,
    /// Last expand output, on success.
    expanded: Option<String>,
    /// Last expand error, rendered verbatim from `SnippetError`.
    expand_error: Option<String>,
    /// Whether the create/edit form is open.
    form_open: bool,
    /// `Some` when editing an existing snippet.
    editing: Option<SnippetId>,
    draft_label: Entity<InputState>,
    draft_template: Entity<InputState>,
    draft_description: Entity<InputState>,
    draft_tags: Entity<InputState>,
    draft_variables: Entity<InputState>,
    form_error: Option<String>,
    focus_handle: FocusHandle,
}

impl SnippetsPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let draft_label =
            cx.new(|cx| InputState::new(window, cx).placeholder("Label, e.g. Deploy"));
        let draft_template = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Command, e.g. ssh {{user}}@{{host}}")
        });
        let draft_description =
            cx.new(|cx| InputState::new(window, cx).placeholder("Description (optional)"));
        let draft_tags = cx
            .new(|cx| InputState::new(window, cx).placeholder("Tags, comma-separated (optional)"));
        let draft_variables = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Variables, e.g. host, user=deploy, port=22")
        });
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let mut store = SnippetStore::at_default_path();
        let load_result = store.load();
        if let Err(error) = load_result {
            let message = SharedString::from(format!("Could not load snippets: {error}"));
            cx.defer_in(window, move |_, window, cx| {
                window.push_notification(Notification::warning(message), cx);
            });
        }

        let mut pane = Self {
            store,
            selected: None,
            variable_inputs: HashMap::new(),
            expanded: None,
            expand_error: None,
            form_open: false,
            editing: None,
            draft_label,
            draft_template,
            draft_description,
            draft_tags,
            draft_variables,
            form_error: None,
            focus_handle,
        };
        // Select the first snippet if any, so the variable form is visible on open.
        if let Some(first) = pane.store.snippets().first().cloned() {
            let id = first.id().clone();
            pane.select_id(id, window, cx);
        }
        pane
    }

    /// How many snippets are currently stored.
    pub fn snippet_count(&self) -> usize {
        self.store.len()
    }

    /// The live snippet slice from the store.
    pub fn snippets(&self) -> &[Snippet] {
        self.store.snippets()
    }

    /// The selected snippet id, if any.
    pub fn selected(&self) -> Option<&SnippetId> {
        self.selected.as_ref()
    }

    /// Whether the create/edit form is open.
    pub fn is_form_open(&self) -> bool {
        self.form_open
    }

    /// Whether an existing snippet is being edited.
    pub fn editing(&self) -> Option<&SnippetId> {
        self.editing.as_ref()
    }

    /// The last successful expansion, if any.
    pub fn expanded(&self) -> Option<&str> {
        self.expanded.as_deref()
    }

    /// The last expand error, if any.
    pub fn expand_error(&self) -> Option<&str> {
        self.expand_error.as_deref()
    }

    fn selected_snippet(&self) -> Option<&Snippet> {
        let id = self.selected.as_ref()?;
        self.store.get(id)
    }

    fn select_id(&mut self, id: SnippetId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.expanded = None;
        self.expand_error = None;
        self.rebuild_variable_inputs(window, cx);
        cx.notify();
    }

    fn rebuild_variable_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.variable_inputs.clear();
        let Some(snippet) = self.selected_snippet().cloned() else {
            return;
        };
        for variable in snippet.declared_variables() {
            let placeholder = match variable.default_value() {
                Some(default) => format!("{} (default: {})", variable.name(), default),
                None => variable.name().to_string(),
            };
            let default_value = String::new();
            let input = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholder);
                if !default_value.is_empty() {
                    state.set_value(&default_value, window, cx);
                }
                state
            });
            self.variable_inputs
                .insert(variable.name().to_string(), input);
        }
        // Also ensure placeholders for variables that appear in the template but
        // were not declared — the crate will surface UnknownVariable, but we still
        // show them as inputs so the user can see the gap. Those variables have no
        // declaration, so we create an input with the bare name.
        for name in snippet.template().variables() {
            if !self.variable_inputs.contains_key(name) {
                let placeholder = name.clone();
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                self.variable_inputs.insert(name.clone(), input);
            }
        }
    }

    fn open_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.form_error = None;
        self.draft_label
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_template
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_description
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_tags
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.draft_variables
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.form_open = true;
        cx.notify();
        // Focus the label field after the form renders.
        let handle = self.draft_label.read(cx).focus_handle(cx);
        cx.defer_in(window, move |_, window, cx| {
            handle.focus(window, cx);
        });
    }

    fn open_edit(&mut self, id: &SnippetId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snippet) = self.store.get(id).cloned() else {
            return;
        };
        self.editing = Some(id.clone());
        self.form_error = None;
        let label = snippet.label().to_string();
        let template = snippet.template().raw().to_string();
        let description = snippet.description().unwrap_or("").to_string();
        let tags = snippet.tags().join(", ");
        let variables = variables_to_string(snippet.declared_variables());
        self.draft_label
            .update(cx, |state, cx| state.set_value(&label, window, cx));
        self.draft_template
            .update(cx, |state, cx| state.set_value(&template, window, cx));
        self.draft_description
            .update(cx, |state, cx| state.set_value(&description, window, cx));
        self.draft_tags
            .update(cx, |state, cx| state.set_value(&tags, window, cx));
        self.draft_variables
            .update(cx, |state, cx| state.set_value(&variables, window, cx));
        self.form_open = true;
        cx.notify();
        let handle = self.draft_label.read(cx).focus_handle(cx);
        cx.defer_in(window, move |_, window, cx| {
            handle.focus(window, cx);
        });
    }

    fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form_open = false;
        self.editing = None;
        self.form_error = None;
        cx.notify();
    }

    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.draft_label.read(cx).value().trim().to_string();
        let template = self.draft_template.read(cx).value().trim().to_string();
        let description_raw = self.draft_description.read(cx).value().trim().to_string();
        let tags_raw = self.draft_tags.read(cx).value().to_string();
        let variables_raw = self.draft_variables.read(cx).value().to_string();

        if label.is_empty() {
            self.form_error = Some("A label is required".to_string());
            cx.notify();
            return;
        }
        if template.is_empty() {
            self.form_error = Some("A command template is required".to_string());
            cx.notify();
            return;
        }

        let variables = match parse_variables_raw(&variables_raw) {
            Ok(variables) => variables,
            Err(message) => {
                self.form_error = Some(message);
                cx.notify();
                return;
            }
        };

        let tags = parse_tags(&tags_raw);

        // Build the snippet through the crate so its validation runs.
        let mut snippet = match Snippet::new(label.clone(), template.clone()) {
            Ok(snippet) => snippet,
            Err(error) => {
                self.form_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        if !description_raw.is_empty() {
            snippet = snippet.with_description(description_raw);
        }
        if !tags.is_empty() {
            snippet = snippet.with_tags(tags);
        }
        if !variables.is_empty() {
            snippet = snippet.with_variables(variables);
        }

        // If the template mentions a variable that was not declared, the crate
        // will reject it only at expand time (UnknownVariable). Catch it early
        // by trial-expanding with empty values and checking for that error — but
        // a missing required variable is not a form error, it is an expand-time
        // prompt, so allow MissingVariable here.
        let probe: HashMap<String, String> = HashMap::new();
        if let Err(error) = snippet.expand(&probe) {
            match error {
                SnippetError::UnknownVariable { .. }
                | SnippetError::UnbalancedDelimiter { .. }
                | SnippetError::EmptyPlaceholder { .. } => {
                    self.form_error = Some(error.to_string());
                    cx.notify();
                    return;
                }
                SnippetError::MissingVariable { .. } => {}
                SnippetError::EmptyLabel | SnippetError::EmptyTemplate => {
                    self.form_error = Some(error.to_string());
                    cx.notify();
                    return;
                }
            }
        }

        // Replace or insert.
        if let Some(editing_id) = self.editing.clone() {
            self.store
                .snippets_mut()
                .retain(|snippet| snippet.id() != &editing_id);
        }
        // If a different snippet already has this label-derived id, replace it.
        let new_id = snippet.id().clone();
        self.store
            .snippets_mut()
            .retain(|snippet| snippet.id() != &new_id);
        self.store.snippets_mut().push(snippet.clone());
        // Keep a stable order by label for the grid.
        self.store
            .snippets_mut()
            .sort_by(|a, b| a.label().cmp(b.label()));

        if let Err(error) = self.store.save() {
            self.form_error = Some(format!("Could not save: {error}"));
            window.push_notification(
                Notification::error(format!("Could not save snippets: {error}")),
                cx,
            );
            cx.notify();
            return;
        }

        self.form_open = false;
        self.editing = None;
        self.form_error = None;
        self.selected = Some(new_id.clone());
        self.rebuild_variable_inputs(window, cx);
        window.push_notification(Notification::success("Snippet saved"), cx);
        cx.notify();
    }

    fn delete(&mut self, id: &SnippetId, window: &mut Window, cx: &mut Context<Self>) {
        self.store
            .snippets_mut()
            .retain(|snippet| snippet.id() != id);
        if self.selected.as_ref() == Some(id) {
            self.selected = None;
            self.variable_inputs.clear();
            self.expanded = None;
            self.expand_error = None;
        }
        if self.editing.as_ref() == Some(id) {
            self.form_open = false;
            self.editing = None;
        }
        if let Err(error) = self.store.save() {
            window.push_notification(
                Notification::error(format!("Could not save snippets: {error}")),
                cx,
            );
            return;
        }
        window.push_notification(Notification::success("Snippet deleted"), cx);
        cx.notify();
    }

    fn expand_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snippet) = self.selected_snippet().cloned() else {
            return;
        };
        let mut values: HashMap<String, String> = HashMap::new();
        for (name, input) in &self.variable_inputs {
            let value = input.read(cx).value().trim().to_string();
            if !value.is_empty() {
                values.insert(name.clone(), value);
            }
        }
        match snippet.expand(&values) {
            Ok(expanded) => {
                self.expanded = Some(expanded.clone());
                self.expand_error = None;
                // Copying is not automatic; the user can press Copy.
                cx.notify();
            }
            Err(error) => {
                // Surface the typed error verbatim — MissingVariable,
                // UnknownVariable, UnbalancedDelimiter etc. — rather than a
                // generic "could not expand".
                self.expanded = None;
                self.expand_error = Some(error.to_string());
                cx.notify();
                // Also surface as a notification for visibility without losing
                // the inline message.
                window.push_notification(Notification::warning(error.to_string()), cx);
            }
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        // Left: + New snippet split button + Shell History (disabled, as in ref).
        // Right: three small icon controls (search, grid, calendar) disabled.
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.))
            .px_3()
            .flex_shrink_0()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        // Primary-ish New snippet on the left; the caret is a
                        // separate small button to mirror the reference's split.
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_0()
                            .child(
                                Button::new("snippets-new")
                                    .icon(IconName::Plus)
                                    .label("New snippet")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_new(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("snippets-new-caret")
                                    .icon(IconName::ChevronDown)
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_new(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        Button::new("snippets-shell-history")
                            .icon(IconName::Calendar)
                            .label("Shell History")
                            .small()
                            .ghost()
                            .disabled(true)
                            .tooltip("Shell history coming soon"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("snippets-search")
                            .ghost()
                            .icon(IconName::Search)
                            .small()
                            .disabled(true)
                            .tooltip("Search snippets"),
                    )
                    .child(
                        Button::new("snippets-layout")
                            .ghost()
                            .icon(IconName::LayoutDashboard)
                            .small()
                            .disabled(true)
                            .tooltip("Grid view"),
                    )
                    .child(
                        Button::new("snippets-calendar")
                            .ghost()
                            .icon(IconName::Calendar)
                            .small()
                            .disabled(true)
                            .tooltip("Calendar view"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .ml_2()
                            .child(format!("{}", self.store.len())),
                    ),
            )
    }

    fn render_form(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .p_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .w_full()
                    .p_4()
                    .rounded(px(10.))
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui_kit::FontWeight::BOLD)
                            .child(if self.editing.is_some() {
                                "Edit snippet"
                            } else {
                                "New snippet"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("LABEL"),
                            )
                            .child(Input::new(&self.draft_label).small()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("COMMAND TEMPLATE"),
                            )
                            .child(Input::new(&self.draft_template).small())
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("Use {{name}} placeholders. Values are inserted literally, never re-expanded."),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("DESCRIPTION (OPTIONAL)"),
                            )
                            .child(Input::new(&self.draft_description).small()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("TAGS (OPTIONAL)"),
                            )
                            .child(Input::new(&self.draft_tags).small()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("VARIABLES (OPTIONAL)"),
                            )
                            .child(Input::new(&self.draft_variables).small())
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("Comma-separated: name, name=default. Unlisted placeholders are an error at expand time."),
                            ),
                    )
                    .when_some(self.form_error.clone(), |el, error| {
                        el.child(Alert::error("snippet-form-error", error).title("Snippet not saved"))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("snippet-save")
                                    .small()
                                    .primary()
                                    .label("Save")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_form(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("snippet-cancel")
                                    .small()
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.close_form(cx))),
                            ),
                    ),
            )
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let cards: Vec<AnyElement> = self
            .store
            .snippets()
            .iter()
            .map(|snippet| self.render_card(snippet, cx).into_any_element())
            .collect();

        div()
            .id("snippets-grid")
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_3()
            .p_3()
            .overflow_y_scrollbar()
            .children(cards)
            .into_any_element()
    }

    fn render_card(&self, snippet: &Snippet, cx: &mut Context<Self>) -> impl IntoElement {
        let id = snippet.id().clone();
        let selected = self.selected.as_ref() == Some(&id);
        let label = snippet.label().to_string();
        let secondary = secondary_line(snippet);
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let card_bg = cx.theme().background;
        let selected_bg = cx.theme().muted;
        // Hardcoded glyph: dark-blue square, white icon.
        let glyph = glyph_bg();
        let edit_id = id.clone();
        let delete_id = id.clone();
        let select_id = id.clone();

        div()
            .id(SharedString::from(format!("snippet-card-{}", id.as_str())))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .w(px(320.))
            .h(px(72.))
            .px_3()
            .py_2()
            .rounded(px(10.))
            .bg(if selected { selected_bg } else { card_bg })
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .hover(|style| style.bg(selected_bg))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_id(select_id.clone(), window, cx);
            }))
            .child(
                // Dark-blue rounded square with a folder/file glyph.
                div()
                    .size(px(40.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.))
                    .bg(glyph)
                    .child(
                        Icon::new(IconName::File)
                            .small()
                            .text_color(Hsla::from(rgb(0xffffff))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .truncate()
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .truncate()
                            .font_family("Menlo")
                            .child(SharedString::from(secondary)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(format!(
                            "snippet-edit-{}",
                            edit_id.as_str()
                        )))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Settings2)
                        .tooltip("Edit snippet")
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_edit(&edit_id, window, cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "snippet-delete-{}",
                            delete_id.as_str()
                        )))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Delete snippet")
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.delete(&delete_id, window, cx);
                            },
                        )),
                    ),
            )
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let snippet = self.selected_snippet()?.clone();
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let card_bg = cx.theme().background;

        let declared = snippet.declared_variables().to_vec();
        let template_vars = snippet.template().variables().to_vec();
        let has_inputs = !self.variable_inputs.is_empty();

        // Build variable rows in declared order, then any template-only vars.
        let mut rows: Vec<AnyElement> = Vec::new();
        // First, declared variables in order.
        for variable in &declared {
            let name = variable.name().to_string();
            if let Some(input) = self.variable_inputs.get(&name) {
                let default_hint = variable
                    .default_value()
                    .map(|value| format!("  default: {value}"))
                    .unwrap_or_default();
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(120.))
                                .text_size(px(12.))
                                .text_color(muted)
                                .child(SharedString::from(format!("{}{}", name, default_hint))),
                        )
                        .child(div().flex_1().child(Input::new(input).small()))
                        .into_any_element(),
                );
            }
        }
        // Then any template vars not in declared (to surface UnknownVariable).
        for name in &template_vars {
            if declared.iter().any(|variable| variable.name() == name) {
                continue;
            }
            if let Some(input) = self.variable_inputs.get(name) {
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(120.))
                                .text_size(px(12.))
                                .text_color(cx.theme().danger)
                                .child(SharedString::from(format!("{name} (undeclared)"))),
                        )
                        .child(div().flex_1().child(Input::new(input).small()))
                        .into_any_element(),
                );
            }
        }

        let expanded = self.expanded.clone();
        let expand_error = self.expand_error.clone();
        let snippet_label = snippet.label().to_string();
        let template_raw = snippet.template().raw().to_string();

        Some(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_3()
                .m_3()
                .rounded(px(10.))
                .bg(card_bg)
                .border_1()
                .border_color(border)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui_kit::FontWeight::BOLD)
                                .child(SharedString::from(snippet_label)),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(muted)
                                .font_family("Menlo")
                                .child(SharedString::from(template_raw)),
                        )
                        .when_some(snippet.description().map(str::to_string), |el, description| {
                            el.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child(SharedString::from(description)),
                            )
                        }),
                )
                .when(has_inputs, |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("VARIABLES"),
                            )
                            .children(rows),
                    )
                })
                .when(!has_inputs, |el| {
                    el.child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .child("No variables declared — expand will insert the template as-is."),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("snippet-expand")
                                .small()
                                .primary()
                                .icon(IconName::Plus)
                                .label("Expand")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.expand_selected(window, cx);
                                })),
                        )
                        .child(
                            Button::new("snippet-copy")
                                .small()
                                .ghost()
                                .icon(IconName::Copy)
                                .label("Copy")
                                .when_some(expanded.clone(), |button, value| {
                                    button.on_click(cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            value.clone(),
                                        ));
                                    }))
                                })
                                .disabled(expanded.is_none()),
                        )
                        .child(
                            Button::new("snippet-edit-detail")
                                .small()
                                .ghost()
                                .icon(IconName::Settings2)
                                .label("Edit")
                                .on_click({
                                    let id = snippet.id().clone();
                                    cx.listener(move |this, _, window, cx| {
                                        this.open_edit(&id, window, cx);
                                    })
                                }),
                        ),
                )
                .when_some(expand_error, |el, error| {
                    el.child(Alert::error("snippet-expand-error", error.clone()).title(error))
                })
                .when_some(expanded, |el, value| {
                    el.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("EXPANDED"),
                            )
                            .child(
                                div()
                                    .p_2()
                                    .rounded(px(6.))
                                    .bg(cx.theme().muted)
                                    .font_family("Menlo")
                                    .text_size(px(13.))
                                    .child(SharedString::from(value)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child("Values are inserted literally; a value containing {{...}} is not re-expanded."),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_empty(&self, cx: &App) -> Div {
        empty_state(
            cx,
            "No snippets yet",
            "Save your most used commands as snippets to reuse them in one click.",
        )
    }
}

impl Render for SnippetsPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Light content surface #edf1f2 is `sidebar` in the light theme; cards
        // and toolbar are `background` (#ffffff).
        let background = cx.theme().sidebar;
        let foreground = cx.theme().foreground;
        let form_open = self.form_open;
        let has_snippets = !self.store.is_empty();

        let body: AnyElement = if form_open {
            self.render_form(cx).into_any_element()
        } else if !has_snippets {
            self.render_empty(cx).into_any_element()
        } else {
            // Grid plus the selected detail, stacked vertically and scrollable.
            let detail = self.render_detail(cx);
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scrollbar()
                .child(self.render_grid(cx))
                .when_some(detail, |el, detail| el.child(detail))
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .text_color(foreground)
            .text_size(px(14.))
            .track_focus(&self.focus_handle)
            .child(self.render_toolbar(cx))
            .child(body)
    }
}

/// Centred empty state, mirroring the port-forwarding pane's `empty_state`.
fn empty_state(cx: &App, title: &str, detail: &str) -> Div {
    let muted = cx.theme().muted_foreground;
    let foreground = cx.theme().foreground;

    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .flex_1()
        .min_h(px(0.))
        .p_6()
        .child(
            div()
                .size(px(72.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(16.))
                .bg(cx.theme().muted)
                .child(Icon::new(IconName::File).large().text_color(foreground)),
        )
        .child(
            div()
                .text_size(px(20.))
                .font_weight(gpui_kit::FontWeight::BOLD)
                .text_color(foreground)
                .child(SharedString::from(title.to_string())),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(px(14.))
                .text_color(muted)
                .child(SharedString::from(detail.to_string())),
        )
        .child(
            // Hint to use the toolbar button.
            div()
                .text_size(px(12.))
                .text_color(muted)
                .child("Use “New snippet” above to create one."),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variables_splits_name_and_default() {
        let variables = parse_variables_raw("host, user=deploy, port=22").expect("valid");
        assert_eq!(variables.len(), 3);
        assert_eq!(variables[0].name(), "host");
        assert!(variables[0].default_value().is_none());
        assert_eq!(variables[1].name(), "user");
        assert_eq!(variables[1].default_value(), Some("deploy"));
        assert_eq!(variables[2].name(), "port");
        assert_eq!(variables[2].default_value(), Some("22"));

        let empty = parse_variables_raw("   ").expect("empty");
        assert!(empty.is_empty());

        let dup = parse_variables_raw("host, host=other").expect("dedup");
        assert_eq!(dup.len(), 1);
        assert_eq!(dup[0].name(), "host");
        assert!(dup[0].default_value().is_none(), "first wins");

        assert!(parse_variables_raw("=bad").is_err());
        assert!(parse_variables_raw("a=, b").expect("ok")[0].name() == "a");
    }

    #[test]
    fn variables_round_trip_through_string() {
        let original = vec![
            Variable::new("host"),
            Variable::with_default("user", "deploy"),
        ];
        let encoded = variables_to_string(&original);
        assert_eq!(encoded, "host, user=deploy");
        let decoded = parse_variables_raw(&encoded).expect("round trip");
        assert_eq!(decoded, original);
    }

    #[test]
    fn secondary_line_prefers_description_over_template() {
        let snippet = Snippet::new("Deploy", "ssh {{host}}")
            .expect("valid")
            .with_description("Run deploy")
            .with_tags(vec!["prod".to_string()])
            .with_variables(vec![Variable::new("host")]);
        assert_eq!(secondary_line(&snippet), "Run deploy");

        let tagged = Snippet::new("List", "ls -la")
            .expect("valid")
            .with_tags(vec!["files".to_string(), "list".to_string()]);
        assert_eq!(secondary_line(&tagged), "files, list");

        let plain = Snippet::new("Echo", "echo {{value}}")
            .expect("valid")
            .with_variables(vec![Variable::new("value")]);
        assert_eq!(secondary_line(&plain), "echo {{value}}");
    }

    #[test]
    fn parse_tags_splits_and_trims() {
        assert_eq!(parse_tags("a, b, c"), vec!["a", "b", "c"]);
        assert_eq!(parse_tags("  "), Vec::<String>::new());
        assert_eq!(parse_tags("one,, two"), vec!["one", "two"]);
    }
}
