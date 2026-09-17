//! The remote file browser pane.
//!
//! A self-contained gpui-kit view over one live [`SftpClient`]: it lists a
//! remote directory (name, size, kind, permissions, modified time), navigates
//! with a breadcrumb trail and a parent button, and surfaces the client's
//! transfer queue with progress and per-transfer cancellation.
//!
//! # Wiring contract
//!
//! `SftpPane::new` deliberately takes no transport. The host view owns the SSH
//! session, so only the host can open SFTP on that session — `SftpClient::connect`
//! is async and session-scoped. The host supplies a live handle with
//! [`SftpPane::set_client`] and clears it again with `None`:
//!
//! ```ignore
//! let pane = cx.new(|cx| SftpPane::new(window, cx));
//! // ... later, once SFTP is up on the session:
//! pane.update(cx, |pane, cx| pane.set_client(Some(client), cx));
//! ```
//!
//! Until a handle is supplied the pane renders a "not connected" state rather
//! than a blank listing. [`SftpPane::client`] and [`SftpPane::current_path`] are
//! the matching readers.
//!
//! Every remote call runs on a GPUI foreground task, so no `await` blocks the
//! UI thread. Listings are capped at [`MAX_DIR_ENTRIES`] and the transfer view
//! at [`MAX_TRANSFERS`], so a pathological remote directory cannot grow the
//! process without bound (docs/BUDGET.md).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, Context, Div, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, Styled as _, Window,
};
use sshdeck_sftp::{
    DirEntry, FileKind, SftpClient, SftpError, TransferEvent, TransferId, TransferState,
};

/// Directory listings are truncated to this many entries so a pathological
/// remote directory cannot become the memory cliff docs/BUDGET.md warns about.
const MAX_DIR_ENTRIES: usize = 1_000;

/// The transfer view keeps at most this many rows; the oldest is evicted.
const MAX_TRANSFERS: usize = 64;

/// One transfer as the pane last saw it.
///
/// `TransferEvent` carries only an id and a state, so that is all a row can
/// show; the crate does not expose the enqueued `Transfer` back to consumers.
struct TransferRow {
    id: TransferId,
    state: TransferState,
}

/// The remote file browser.
pub struct SftpPane {
    /// `None` until the host supplies one; the pane then shows "not connected".
    client: Option<Arc<SftpClient>>,
    /// Canonical absolute path of the directory being shown.
    cwd: String,
    entries: Vec<DirEntry>,
    /// Whether `entries` was truncated by [`MAX_DIR_ENTRIES`].
    truncated: bool,
    selected: Option<usize>,
    loading: bool,
    /// A listing or navigation failure, rendered instead of a blank pane.
    error: Option<String>,
    transfers: Vec<TransferRow>,
    /// Bumped per listing so a superseded load cannot overwrite a newer one.
    load_generation: u64,
    /// Bumped per attached client so a stale transfer stream is ignored.
    watch_generation: u64,
    focus_handle: FocusHandle,
}

impl SftpPane {
    /// Creates a disconnected pane.
    ///
    /// The host must call [`Self::set_client`] once SFTP is up; until then the
    /// pane renders its "not connected" state.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            client: None,
            cwd: String::new(),
            entries: Vec::new(),
            truncated: false,
            selected: None,
            loading: false,
            error: None,
            transfers: Vec::new(),
            load_generation: 0,
            watch_generation: 0,
            focus_handle,
        }
    }

    /// The live SFTP handle, if one has been supplied.
    pub fn client(&self) -> Option<&SftpClient> {
        self.client.as_deref()
    }

    /// The canonical path currently being listed; empty until the first load.
    pub fn current_path(&self) -> &str {
        &self.cwd
    }

    /// Attaches or detaches the SFTP transport.
    ///
    /// Passing `Some` begins watching the transfer queue and loads the default
    /// directory. Passing `None` returns the pane to its "not connected" state
    /// and drops every listing and transfer row it was holding.
    pub fn set_client(&mut self, client: Option<SftpClient>, cx: &mut Context<Self>) {
        // Any in-flight load or transfer stream for the old handle is now stale.
        self.watch_generation = self.watch_generation.wrapping_add(1);
        match client {
            Some(client) => {
                let client = Arc::new(client);
                let events = client.transfers();
                self.client = Some(client);
                self.cwd.clear();
                self.entries.clear();
                self.truncated = false;
                self.selected = None;
                self.error = None;
                self.transfers.clear();
                self.watch_transfers(events, cx);
                self.refresh(cx);
            }
            None => {
                self.client = None;
                self.cwd.clear();
                self.entries.clear();
                self.truncated = false;
                self.selected = None;
                self.loading = false;
                self.error = None;
                self.transfers.clear();
            }
        }
        cx.notify();
    }

    /// Reloads the current directory (the default directory before any load).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let path = if self.cwd.is_empty() {
            ".".to_string()
        } else {
            self.cwd.clone()
        };
        self.load(path, cx);
    }

    /// Navigates to an absolute path.
    fn navigate(&mut self, path: String, cx: &mut Context<Self>) {
        self.load(path, cx);
    }

    /// Enters the named child of the current directory.
    fn activate(&mut self, name: &str, cx: &mut Context<Self>) {
        match sshdeck_sftp::join(&self.cwd, name) {
            Ok(path) => self.load(path, cx),
            Err(error) => {
                self.error = Some(format!("Cannot open {name}: {error}"));
                cx.notify();
            }
        }
    }

    /// Jumps to the parent of the current directory, when there is one.
    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = parent_path(&self.cwd) {
            self.load(parent, cx);
        }
    }

    /// Starts an async canonicalize-then-list for `path`.
    ///
    /// The remote calls run on a GPUI foreground task and only await the
    /// client's channels, so the UI thread itself never blocks. The result is
    /// applied only when no newer load has superseded it.
    fn load(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        self.loading = true;
        self.error = None;
        self.selected = None;
        cx.notify();

        cx.spawn(async move |pane, cx| {
            let result = async {
                // Resolve first so symlinks and `..` become one path, then list
                // that path rather than a hand-built string.
                let canonical = client.canonicalize(&path).await?;
                let entries = client.list(&canonical).await?;
                Ok::<_, SftpError>((canonical, entries))
            }
            .await;

            pane.update(cx, |pane, cx| {
                if pane.load_generation != generation {
                    return;
                }
                pane.loading = false;
                match result {
                    Ok((canonical, entries)) => {
                        pane.cwd = canonical;
                        pane.set_entries(entries);
                        pane.error = None;
                    }
                    Err(error) => {
                        pane.entries.clear();
                        pane.truncated = false;
                        pane.error = Some(describe_failure(&path, &error));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stores a listing, truncating it to the budgeted maximum.
    fn set_entries(&mut self, mut entries: Vec<DirEntry>) {
        // `list` already sorts directories first, so truncation keeps them and
        // drops the tail; `list`'s own Vec is the only transient allocation.
        self.truncated = entries.len() > MAX_DIR_ENTRIES;
        entries.truncate(MAX_DIR_ENTRIES);
        self.entries = entries;
    }

    /// Forwards transfer-queue events into the view while the client lives.
    fn watch_transfers(
        &mut self,
        events: async_channel::Receiver<TransferEvent>,
        cx: &mut Context<Self>,
    ) {
        let watch = self.watch_generation;
        cx.spawn(async move |pane, cx| {
            while let Ok(event) = events.recv().await {
                let keep_going = pane
                    .update(cx, |pane, cx| {
                        if pane.watch_generation != watch {
                            return false;
                        }
                        pane.apply_transfer(event);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Folds one transfer event into the bounded row list.
    fn apply_transfer(&mut self, event: TransferEvent) {
        let id = event.id();
        let state = event.state().clone();
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            row.state = state;
            return;
        }
        if self.transfers.len() >= MAX_TRANSFERS {
            self.transfers.remove(0);
        }
        self.transfers.push(TransferRow { id, state });
    }

    /// Asks the client to cancel one transfer.
    fn cancel(&self, id: TransferId, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |_pane, _cx| {
            // The queue is the source of truth: it emits the final `Cancelled`
            // event itself, so the immediate result needs no further handling.
            let _ = client.cancel_transfer(id).await;
        })
        .detach();
    }

    /// The header: parent/refresh actions and the breadcrumb trail.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let has_parent = parent_path(&self.cwd).is_some();
        let connected = self.client.is_some();

        let up = Button::new("sftp-up")
            .ghost()
            .small()
            .icon(IconName::ArrowUp)
            .tooltip("Parent folder")
            .disabled(!connected || !has_parent)
            .on_click(cx.listener(|this, _, _, cx| this.go_up(cx)));

        let refresh = Button::new("sftp-refresh")
            .ghost()
            .small()
            .icon(IconName::Replace)
            .tooltip("Refresh")
            .disabled(!connected)
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));

        let crumbs: Vec<Button> = breadcrumbs(&self.cwd)
            .into_iter()
            .map(|(label, path)| {
                let crumb_label = label.clone();
                let crumb_path = path.clone();
                Button::new(format!("sftp-crumb-{path}"))
                    .ghost()
                    .xsmall()
                    .label(crumb_label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate(crumb_path.clone(), cx);
                    }))
            })
            .collect();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(36.))
            .px_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(up)
            .child(refresh)
            .child(
                div()
                    .id("sftp-breadcrumbs")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .overflow_x_scrollbar()
                    .children(crumbs)
                    .when(self.cwd.is_empty(), |el| {
                        el.child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(muted)
                                .child("Not connected"),
                        )
                    }),
            )
    }

    /// The listing body, or the state that stands in for it.
    fn render_body(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;

        let body = div()
            .id("sftp-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scrollbar();

        // No transport: say so rather than show an empty folder.
        if self.client.is_none() {
            return body
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(IconName::FolderClosed).large().text_color(muted))
                .child(div().text_sm().child("Not connected"))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Open an SFTP session to browse remote files"),
                );
        }

        // A failure outranks any listing: it explains why there is none.
        if let Some(error) = &self.error {
            return body
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .large()
                        .text_color(danger),
                )
                .child(div().text_sm().child(error.clone()))
                .child(
                    Button::new("sftp-retry")
                        .small()
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                );
        }

        if self.loading && self.entries.is_empty() {
            return body
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Progress::new("sftp-loading")
                        .loading(true)
                        .small()
                        .w(px(160.)),
                )
                .child(div().text_xs().text_color(muted).child("Loading…"));
        }

        if self.entries.is_empty() {
            return body
                .items_center()
                .justify_center()
                .gap_2()
                .child(Icon::new(IconName::FolderOpen).large().text_color(muted))
                .child(div().text_sm().child("This folder is empty"));
        }

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().child("NAME"))
            .child(div().w(px(80.)).child("KIND"))
            .child(div().w(px(80.)).child("SIZE"))
            .child(div().w(px(110.)).child("PERMISSIONS"))
            .child(div().w(px(110.)).child("MODIFIED"));

        let now = SystemTime::now();
        let rows: Vec<Div> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| self.render_entry(index, entry, now, cx))
            .collect();

        let mut container = body.child(header).children(rows);
        if self.truncated {
            container = container.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Showing the first {MAX_DIR_ENTRIES} entries")),
            );
        }
        container
    }

    /// One directory row.
    fn render_entry(
        &self,
        index: usize,
        entry: &DirEntry,
        now: SystemTime,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;
        let selected_bg = cx.theme().muted;

        let name = entry.name().to_string();
        let is_dir = entry.is_dir();
        let icon = kind_icon(entry.kind());
        let kind = kind_label(entry.kind());
        let mode = entry.mode_string();
        let size = if is_dir {
            "—".to_string()
        } else {
            human_size(entry.size())
        };
        let modified = entry
            .modified()
            .map(|time| relative_age(time, now))
            .unwrap_or_else(|| "—".to_string());
        let activate = name.clone();
        let selected = self.selected == Some(index);

        div()
            .id(format!("sftp-entry-{name}"))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .w_full()
            .px_2()
            .py_1()
            .cursor_pointer()
            .border_b_1()
            .border_color(border)
            .when(selected, |el| el.bg(selected_bg))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(index);
                cx.notify();
            }))
            .on_double_click(cx.listener(move |this, _, _, cx| {
                if is_dir {
                    this.activate(&activate, cx);
                }
            }))
            .child(
                Icon::new(icon)
                    .small()
                    .text_color(if is_dir { foreground } else { muted }),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(name),
            )
            .child(div().w(px(80.)).text_xs().text_color(muted).child(kind))
            .child(div().w(px(80.)).text_xs().text_color(muted).child(size))
            .child(div().w(px(110.)).text_xs().text_color(muted).child(mode))
            .child(
                div()
                    .w(px(110.))
                    .text_xs()
                    .text_color(muted)
                    .child(modified),
            )
    }

    /// The transfer queue at the foot of the pane.
    fn render_transfers(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        let section = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .border_t_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child("TRANSFERS")
                    .child(self.transfers.len().to_string()),
            );

        if self.client.is_none() {
            return section;
        }

        if self.transfers.is_empty() {
            return section
                .px_2()
                .pb_2()
                .text_xs()
                .text_color(muted)
                .child("No transfers yet");
        }

        let rows: Vec<Div> = self
            .transfers
            .iter()
            .map(|row| self.render_transfer(row, cx))
            .collect();

        section.child(
            div()
                .id("sftp-transfers")
                .flex()
                .flex_col()
                .h(px(160.))
                .overflow_y_scrollbar()
                .children(rows),
        )
    }

    /// One transfer row: state, progress and a cancel action while active.
    fn render_transfer(&self, row: &TransferRow, cx: &mut Context<Self>) -> Div {
        let id = row.id;
        let active = matches!(
            row.state,
            TransferState::Queued | TransferState::Running { .. }
        );
        let label = match &row.state {
            TransferState::Queued => "Queued".to_string(),
            TransferState::Running { .. } => "Running".to_string(),
            TransferState::Complete => "Complete".to_string(),
            TransferState::Failed { message } => format!("Failed — {message}"),
            TransferState::Cancelled => "Cancelled".to_string(),
        };
        let state_color = match &row.state {
            TransferState::Complete => cx.theme().success,
            TransferState::Failed { .. } => cx.theme().danger,
            TransferState::Running { .. } => cx.theme().foreground,
            _ => cx.theme().muted_foreground,
        };
        let border = cx.theme().border;

        let progress_id = format!("sftp-transfer-{}", id.get());
        let mut progress = Progress::new(progress_id).small();
        match &row.state {
            TransferState::Running { done, total } => match total {
                Some(total) if *total > 0 => {
                    progress = progress.value((*done as f32 / *total as f32) * 100.0);
                }
                // No total: an honest indeterminate bar.
                _ => progress = progress.loading(true),
            },
            TransferState::Complete => progress = progress.value(100.0),
            _ => {}
        }

        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(div().text_xs().child(format!("Transfer #{}", id.get())))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().text_xs().text_color(state_color).child(label))
                            .child(
                                Button::new(format!("sftp-cancel-{}", id.get()))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip("Cancel transfer")
                                    .disabled(!active)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.cancel(id, cx);
                                    })),
                            ),
                    ),
            )
            .child(progress)
    }
}

impl Render for SftpPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .text_color(foreground)
            .track_focus(&self.focus_handle)
            .child(self.render_toolbar(cx))
            .child(self.render_body(cx))
            .child(self.render_transfers(cx))
    }
}

/// Formats a byte count for the size column, in binary units.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", value, UNITS[unit])
}

/// The parent of a POSIX remote path, or `None` at the root.
fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    let index = trimmed.rfind('/')?;
    if index == 0 {
        Some("/".to_string())
    } else {
        Some(trimmed[..index].to_string())
    }
}

/// The `(label, absolute path)` breadcrumb pairs for a canonical path.
fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut crumbs = Vec::new();
    if path.starts_with('/') {
        crumbs.push(("/".to_string(), "/".to_string()));
    }
    let mut accumulated = String::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        accumulated.push('/');
        accumulated.push_str(segment);
        crumbs.push((segment.to_string(), accumulated.clone()));
    }
    crumbs
}

/// A short "how long ago" label for the modified column.
fn relative_age(modified: SystemTime, now: SystemTime) -> String {
    let seconds = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// A one-word kind for the kind column.
fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Dir => "folder",
        FileKind::File => "file",
        FileKind::Symlink => "symlink",
        FileKind::Other => "other",
    }
}

/// The row icon for a kind.
fn kind_icon(kind: FileKind) -> IconName {
    match kind {
        FileKind::Dir => IconName::Folder,
        FileKind::File => IconName::File,
        FileKind::Symlink => IconName::ExternalLink,
        FileKind::Other => IconName::File,
    }
}

/// Turns an SFTP failure into a message the pane can show; permission problems
/// get a specific line because that is the common, actionable case.
fn describe_failure(path: &str, error: &SftpError) -> String {
    let text = error.to_string();
    if text.to_ascii_lowercase().contains("permission denied") {
        format!("Permission denied: {path}")
    } else {
        format!("Could not list {path}: {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn human_size_uses_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn parent_path_stops_at_the_root() {
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/user"), Some("/home".to_string()));
        assert_eq!(parent_path("/home/user/"), Some("/home".to_string()));
        assert_eq!(parent_path("relative"), None);
    }

    #[test]
    fn breadcrumbs_accumulate_absolute_paths() {
        assert_eq!(
            breadcrumbs("/home/user"),
            vec![
                ("/".to_string(), "/".to_string()),
                ("home".to_string(), "/home".to_string()),
                ("user".to_string(), "/home/user".to_string()),
            ]
        );
        assert!(breadcrumbs("").is_empty());
        assert_eq!(breadcrumbs("/"), vec![("/".to_string(), "/".to_string())]);
    }

    #[test]
    fn relative_age_buckets_into_minutes_hours_days() {
        let now = UNIX_EPOCH + Duration::from_secs(10 * 86_400);
        assert_eq!(relative_age(now, now), "just now");
        assert_eq!(
            relative_age(now - Duration::from_secs(5 * 60), now),
            "5m ago"
        );
        assert_eq!(
            relative_age(now - Duration::from_secs(3 * 3_600), now),
            "3h ago"
        );
        assert_eq!(
            relative_age(now - Duration::from_secs(2 * 86_400), now),
            "2d ago"
        );
    }
}
