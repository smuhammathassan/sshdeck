//! The dual-pane SFTP file browser.
//!
//! Visual parity with Termius dual-pane SFTP (Screenshots 14, 15, 17):
//! - Split 50/50: Left pane is the Local file browser; Right pane is either the
//!   "Connect to host" empty state, the Host selection drawer, or the live Remote browser.
//! - Local filesystem browser scans disk using `std::fs::read_dir`, surfacing
//!   entry name, permissions string (`drwxr-xr-x+`), modified date, size and kind.
//! - Breadcrumb trail navigation with `<` and `>` arrow buttons.
//! - Remote file browser surfaces the remote directory with transfer progress and queue.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Sizable as _,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, rgb, AnyElement, App, Context, Div, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window,
};
use sshdeck_core::Host;
use sshdeck_sftp::{
    DirEntry, FileKind, SftpClient, SftpError, TransferEvent, TransferId, TransferState,
};

use crate::glyph;

/// Directory listings are truncated to this many entries so a pathological
/// directory cannot become a memory cliff (docs/BUDGET.md).
const MAX_DIR_ENTRIES: usize = 1_000;

/// The transfer view keeps at most this many rows; the oldest is evicted.
const MAX_TRANSFERS: usize = 64;

/// One transfer as the pane last saw it.
struct TransferRow {
    id: TransferId,
    state: TransferState,
}

/// One entry in the local filesystem browser.
#[derive(Clone, Debug)]
pub struct LocalEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub permissions: String,
    pub modified: Option<SystemTime>,
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn format_unix_mode(mode: u32, is_dir: bool) -> String {
    let d = if is_dir { 'd' } else { '-' };
    let r1 = if mode & 0o400 != 0 { 'r' } else { '-' };
    let w1 = if mode & 0o200 != 0 { 'w' } else { '-' };
    let x1 = if mode & 0o100 != 0 { 'x' } else { '-' };
    let r2 = if mode & 0o040 != 0 { 'r' } else { '-' };
    let w2 = if mode & 0o020 != 0 { 'w' } else { '-' };
    let x2 = if mode & 0o010 != 0 { 'x' } else { '-' };
    let r3 = if mode & 0o004 != 0 { 'r' } else { '-' };
    let w3 = if mode & 0o002 != 0 { 'w' } else { '-' };
    let x3 = if mode & 0o001 != 0 { 'x' } else { '-' };
    format!("{d}{r1}{w1}{x1}{r2}{w2}{x2}{r3}{w3}{x3}")
}

fn format_mode(metadata: &std::fs::Metadata, is_dir: bool) -> String {
    #[cfg(unix)]
    {
        format_unix_mode(metadata.permissions().mode(), is_dir)
    }
    #[cfg(not(unix))]
    {
        if is_dir {
            "drwxr-xr-x".to_string()
        } else {
            "-rw-r--r--".to_string()
        }
    }
}

fn read_local_dir(path: &Path) -> Result<Vec<LocalEntry>, String> {
    let entries_iter = std::fs::read_dir(path).map_err(|e| e.to_string())?;
    let mut items = Vec::new();
    for res in entries_iter {
        let Ok(entry) = res else { continue };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let is_dir = metadata.is_dir();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let permissions = format_mode(&metadata, is_dir);
        let size = if is_dir { 0 } else { metadata.len() };
        let modified = metadata.modified().ok();
        items.push(LocalEntry {
            name,
            is_dir,
            size,
            permissions,
            modified,
        });
    }
    items.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(items)
}

/// The dual-pane SFTP browser.
pub struct SftpPane {
    /// `None` until a remote host is connected; the right pane then renders empty or host picker.
    client: Option<Arc<SftpClient>>,
    remote_host_label: Option<String>,
    cwd: String,
    entries: Vec<DirEntry>,
    truncated: bool,
    selected: Option<usize>,
    loading: bool,
    error: Option<String>,
    transfers: Vec<TransferRow>,
    load_generation: u64,
    watch_generation: u64,
    focus_handle: FocusHandle,

    // Local pane state
    local_cwd: PathBuf,
    local_entries: Vec<LocalEntry>,
    local_selected: Option<usize>,
    local_history: Vec<PathBuf>,
    local_history_idx: usize,
    local_error: Option<String>,

    // Remote host picker state
    show_host_picker: bool,
    available_hosts: Vec<Host>,
    on_connect: Option<Box<dyn Fn(Host, &mut Window, &mut App)>>,
}

impl SftpPane {
    /// Creates the SFTP pane, initializing the local file browser with the user's home directory.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let initial_local = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let local_entries = read_local_dir(&initial_local).unwrap_or_default();

        Self {
            client: None,
            remote_host_label: None,
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

            local_cwd: initial_local.clone(),
            local_entries,
            local_selected: None,
            local_history: vec![initial_local],
            local_history_idx: 0,
            local_error: None,

            show_host_picker: false,
            available_hosts: Vec::new(),
            on_connect: None,
        }
    }

    /// Supplies the list of inventory hosts for the right-hand "Select Host" picker.
    pub fn set_available_hosts(&mut self, hosts: Vec<Host>, cx: &mut Context<Self>) {
        self.available_hosts = hosts;
        cx.notify();
    }

    /// Sets the label of the connected remote host (e.g. "Ali Jawwad").
    pub fn set_remote_host_label(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        self.remote_host_label = label;
        cx.notify();
    }

    /// Registers the callback invoked when the user selects a host in the right-pane picker.
    pub fn set_on_connect<F>(&mut self, f: F)
    where
        F: Fn(Host, &mut Window, &mut App) + 'static,
    {
        self.on_connect = Some(Box::new(f));
    }

    /// The live SFTP handle, if one has been supplied.
    pub fn client(&self) -> Option<&SftpClient> {
        self.client.as_deref()
    }

    /// The canonical path currently being listed on the remote server; empty until first load.
    pub fn current_path(&self) -> &str {
        &self.cwd
    }

    /// Attaches or detaches the remote SFTP transport.
    pub fn set_client(&mut self, client: Option<SftpClient>, cx: &mut Context<Self>) {
        self.watch_generation = self.watch_generation.wrapping_add(1);
        match client {
            Some(client) => {
                let client = Arc::new(client);
                let events = client.transfers();
                self.client = Some(client);
                self.show_host_picker = false;
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
                self.remote_host_label = None;
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

    // ── Local navigation ──────────────────────────────────────────────────

    fn load_local(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match read_local_dir(&path) {
            Ok(entries) => {
                self.local_cwd = path;
                self.local_entries = entries;
                self.local_selected = None;
                self.local_error = None;
            }
            Err(err) => {
                self.local_error = Some(err);
            }
        }
        cx.notify();
    }

    fn navigate_local(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.local_history_idx + 1 < self.local_history.len() {
            self.local_history.truncate(self.local_history_idx + 1);
        }
        self.local_history.push(path.clone());
        self.local_history_idx = self.local_history.len() - 1;
        self.load_local(path, cx);
    }

    fn navigate_local_back(&mut self, cx: &mut Context<Self>) {
        if self.local_history_idx > 0 {
            self.local_history_idx -= 1;
            let path = self.local_history[self.local_history_idx].clone();
            self.load_local(path, cx);
        }
    }

    fn navigate_local_forward(&mut self, cx: &mut Context<Self>) {
        if self.local_history_idx + 1 < self.local_history.len() {
            self.local_history_idx += 1;
            let path = self.local_history[self.local_history_idx].clone();
            self.load_local(path, cx);
        }
    }

    fn navigate_local_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.local_cwd.parent() {
            self.navigate_local(parent.to_path_buf(), cx);
        }
    }

    // ── Remote navigation ─────────────────────────────────────────────────

    /// Reloads the current remote directory.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let path = if self.cwd.is_empty() {
            ".".to_string()
        } else {
            self.cwd.clone()
        };
        self.load(path, cx);
    }

    /// Navigates to an absolute remote path.
    fn navigate(&mut self, path: String, cx: &mut Context<Self>) {
        self.load(path, cx);
    }

    /// Enters the named child directory on the remote server.
    fn activate(&mut self, name: &str, cx: &mut Context<Self>) {
        match sshdeck_sftp::join(&self.cwd, name) {
            Ok(path) => self.load(path, cx),
            Err(error) => {
                self.error = Some(format!("Cannot open {name}: {error}"));
                cx.notify();
            }
        }
    }

    /// Jumps to the parent remote directory.
    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = parent_path(&self.cwd) {
            self.load(parent, cx);
        }
    }

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

    fn set_entries(&mut self, mut entries: Vec<DirEntry>) {
        self.truncated = entries.len() > MAX_DIR_ENTRIES;
        entries.truncate(MAX_DIR_ENTRIES);
        self.entries = entries;
    }

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

    fn cancel(&self, id: TransferId, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        cx.spawn(async move |_pane, _cx| {
            let _ = client.cancel_transfer(id).await;
        })
        .detach();
    }

    // ── Local browser rendering ───────────────────────────────────────────

    fn render_local_header(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(48.))
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.))
                            .bg(rgb(0x1d2033))
                            .child(
                                Icon::default()
                                    .data(glyph::LOCAL_HOST)
                                    .small()
                                    .text_color(Hsla::from(rgb(0xffffff))),
                            ),
                    )
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_size(px(14.))
                            .text_color(foreground)
                            .child("Local"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("local-filter-btn")
                            .ghost()
                            .small()
                            .icon(IconName::Search)
                            .label("Filter"),
                    )
                    .child(
                        Button::new("local-actions-btn")
                            .ghost()
                            .small()
                            .label("Actions ▾"),
                    ),
            )
    }

    fn render_local_path_bar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let folder_tint = rgb(0x3b82f6);

        let can_back = self.local_history_idx > 0;
        let can_forward = self.local_history_idx + 1 < self.local_history.len();

        let mut crumbs: Vec<AnyElement> = Vec::new();
        let mut path_accum = PathBuf::from("/");
        for comp in self.local_cwd.components() {
            let name = comp.as_os_str().to_string_lossy().to_string();
            if name.is_empty() || name == "/" {
                continue;
            }
            path_accum.push(&name);
            let target_path = path_accum.clone();

            crumbs.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        Icon::default()
                            .data(glyph::FOLDER)
                            .small()
                            .text_color(Hsla::from(folder_tint)),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "crumb-{}",
                            target_path.display()
                        )))
                        .ghost()
                        .xsmall()
                        .label(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_local(target_path.clone(), cx);
                        })),
                    )
                    .child(div().text_xs().text_color(muted).child("›"))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(40.))
            .px_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(
                Button::new("local-nav-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronLeft)
                    .disabled(!can_back)
                    .on_click(cx.listener(|this, _, _, cx| this.navigate_local_back(cx))),
            )
            .child(
                Button::new("local-nav-forward")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronRight)
                    .disabled(!can_forward)
                    .on_click(cx.listener(|this, _, _, cx| this.navigate_local_forward(cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .overflow_x_scrollbar()
                    .children(crumbs),
            )
    }

    fn render_local_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let hover_bg = cx.theme().accent;
        let folder_tint = rgb(0x3b82f6);

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().child("Name"))
            .child(div().w(px(170.)).child("Date Modified ▴"))
            .child(div().w(px(90.)).child("Size"))
            .child(div().w(px(80.)).child("Kind"));

        let mut rows: Vec<AnyElement> = Vec::new();

        // Parent directory row '..'
        if self.local_cwd.parent().is_some() {
            rows.push(
                div()
                    .id("local-entry-parent")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(border)
                    .hover(move |el| el.bg(hover_bg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate_local_up(cx);
                    }))
                    .child(
                        Icon::default()
                            .data(glyph::FOLDER)
                            .small()
                            .text_color(Hsla::from(folder_tint)),
                    )
                    .child(
                        div().flex().flex_col().flex_1().child(
                            div()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(".."),
                        ),
                    )
                    .child(div().w(px(170.)))
                    .child(div().w(px(90.)))
                    .child(div().w(px(80.)))
                    .into_any_element(),
            );
        }

        for (idx, entry) in self.local_entries.iter().enumerate() {
            let is_dir = entry.is_dir;
            let name = entry.name.clone();
            let activate_path = self.local_cwd.join(&name);
            let mode = entry.permissions.clone();
            let size = if is_dir {
                "- -".to_string()
            } else {
                human_size(entry.size)
            };
            let modified = entry
                .modified
                .map(format_modified)
                .unwrap_or_else(|| "—".to_string());
            let kind = if is_dir {
                "folder".to_string()
            } else {
                Path::new(&name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("file")
                    .to_string()
            };
            let selected = self.local_selected == Some(idx);
            let selected_bg = cx.theme().muted;

            rows.push(
                div()
                    .id(SharedString::from(format!("local-item-{name}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(border)
                    .when(selected, |el| el.bg(selected_bg))
                    .hover(move |el| el.bg(hover_bg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.local_selected = Some(idx);
                        cx.notify();
                    }))
                    .when(is_dir, |el| {
                        el.on_double_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_local(activate_path.clone(), cx);
                        }))
                    })
                    .child(
                        Icon::default()
                            .data(glyph::FOLDER)
                            .small()
                            .text_color(if is_dir {
                                Hsla::from(folder_tint)
                            } else {
                                muted
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .font_family("Menlo")
                                    .child(mode),
                            ),
                    )
                    .child(
                        div()
                            .w(px(170.))
                            .text_xs()
                            .text_color(muted)
                            .font_family("Menlo")
                            .child(modified),
                    )
                    .child(
                        div()
                            .w(px(90.))
                            .text_xs()
                            .text_color(muted)
                            .font_family("Menlo")
                            .child(size),
                    )
                    .child(div().w(px(80.)).text_xs().text_color(muted).child(kind))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scrollbar()
            .child(header)
            .children(rows)
    }

    // ── Remote pane rendering ─────────────────────────────────────────────

    fn render_remote_header(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let label = self
            .remote_host_label
            .clone()
            .unwrap_or_else(|| "Remote Host".to_string());

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(48.))
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(0xe95420))
                            .child(
                                Icon::default()
                                    .data(glyph::UBUNTU)
                                    .small()
                                    .text_color(Hsla::from(rgb(0xffffff))),
                            ),
                    )
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_size(px(14.))
                            .text_color(foreground)
                            .child(label),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("remote-filter-btn")
                            .ghost()
                            .small()
                            .icon(IconName::Search)
                            .label("Filter"),
                    )
                    .child(
                        Button::new("remote-actions-btn")
                            .ghost()
                            .small()
                            .label("Actions ▾"),
                    ),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let folder_tint = rgb(0x3b82f6);
        let has_parent = parent_path(&self.cwd).is_some();
        let connected = self.client.is_some();

        let up = Button::new("sftp-up")
            .ghost()
            .xsmall()
            .icon(IconName::ChevronLeft)
            .tooltip("Parent folder")
            .disabled(!connected || !has_parent)
            .on_click(cx.listener(|this, _, _, cx| this.go_up(cx)));

        let refresh = Button::new("sftp-refresh")
            .ghost()
            .xsmall()
            .icon(IconName::ChevronRight)
            .tooltip("Refresh")
            .disabled(!connected)
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));

        let mut trail: Vec<AnyElement> = Vec::new();
        for (index, (label, path)) in breadcrumb_crumbs(&self.cwd).into_iter().enumerate() {
            if index > 0 {
                trail.push(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("›")
                        .into_any_element(),
                );
            }
            trail.push(
                Icon::default()
                    .data(glyph::FOLDER)
                    .small()
                    .text_color(Hsla::from(folder_tint))
                    .into_any_element(),
            );
            trail.push(
                Button::new(format!("sftp-crumb-{path}"))
                    .ghost()
                    .xsmall()
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.navigate(path.clone(), cx);
                    }))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(40.))
            .px_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(up)
            .child(refresh)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .overflow_x_scrollbar()
                    .when(self.cwd.is_empty(), |el| {
                        el.child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(muted)
                                .child(if connected { "…" } else { "Not connected" }),
                        )
                    })
                    .children(trail),
            )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;

        let body = div()
            .id("sftp-list")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scrollbar();

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
            .px_3()
            .py_1()
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().child("Name"))
            .child(div().w(px(170.)).child("Date Modified ▾"))
            .child(div().w(px(90.)).child("Size"))
            .child(div().w(px(80.)).child("Kind"));

        let mut rows: Vec<AnyElement> = Vec::new();

        if parent_path(&self.cwd).is_some() {
            let hover_bg = cx.theme().accent;
            let folder_tint = rgb(0x3b82f6);
            rows.push(
                div()
                    .id("remote-entry-parent")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(border)
                    .hover(move |el| el.bg(hover_bg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go_up(cx);
                    }))
                    .child(
                        Icon::default()
                            .data(glyph::FOLDER)
                            .small()
                            .text_color(Hsla::from(folder_tint)),
                    )
                    .child(
                        div().flex().flex_col().flex_1().child(
                            div()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(".."),
                        ),
                    )
                    .child(div().w(px(170.)))
                    .child(div().w(px(90.)))
                    .child(div().w(px(80.)))
                    .into_any_element(),
            );
        }

        for (index, entry) in self.entries.iter().enumerate() {
            rows.push(self.render_entry(index, entry, cx).into_any_element());
        }

        let mut container = body.child(header).children(rows);
        if self.truncated {
            container = container.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Showing the first {MAX_DIR_ENTRIES} entries")),
            );
        }
        container
    }

    fn render_entry(
        &self,
        index: usize,
        entry: &DirEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let hover_bg = cx.theme().accent;
        let selected_bg = cx.theme().muted;
        let folder_tint = rgb(0x3b82f6);

        let name = entry.name().to_string();
        let is_dir = entry.is_dir();
        let kind = kind_label(entry.kind());
        let mode = entry.mode_string();
        let size = if is_dir {
            "- -".to_string()
        } else {
            human_size(entry.size())
        };
        let modified = entry
            .modified()
            .map(format_modified)
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
            .px_3()
            .py_2()
            .cursor_pointer()
            .border_b_1()
            .border_color(border)
            .when(selected, |el| el.bg(selected_bg))
            .hover(move |el| el.bg(hover_bg))
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
                Icon::default()
                    .data(glyph::FOLDER)
                    .small()
                    .text_color(if is_dir {
                        Hsla::from(folder_tint)
                    } else {
                        muted
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .font_family("Menlo")
                            .child(mode),
                    ),
            )
            .child(
                div()
                    .w(px(170.))
                    .text_xs()
                    .text_color(muted)
                    .font_family("Menlo")
                    .child(modified),
            )
            .child(
                div()
                    .w(px(90.))
                    .text_xs()
                    .text_color(muted)
                    .font_family("Menlo")
                    .child(size),
            )
            .child(div().w(px(80.)).text_xs().text_color(muted).child(kind))
    }

    fn render_empty_right(&self, cx: &mut Context<Self>) -> Div {
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .size_full()
            .p_6()
            .child(
                div()
                    .size(px(72.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(16.))
                    .bg(cx.theme().muted)
                    .child(
                        Icon::default()
                            .data(glyph::FOLDER)
                            .large()
                            .text_color(foreground),
                    ),
            )
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(gpui_kit::FontWeight::BOLD)
                    .text_color(foreground)
                    .child("Connect to host"),
            )
            .child(
                div()
                    .max_w(px(340.))
                    .text_center()
                    .text_size(px(14.))
                    .text_color(muted)
                    .child("Start by connecting to a saved host\nto manage your files with SFTP."),
            )
            .child(
                Button::new("sftp-select-host-btn")
                    .label("Select host")
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_host_picker = true;
                        cx.notify();
                    })),
            )
    }

    fn render_host_picker(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(48.))
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("host-picker-back")
                            .ghost()
                            .icon(IconName::ArrowLeft)
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_host_picker = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::BOLD)
                                    .text_size(px(14.))
                                    .child("Select Host"),
                            )
                            .child(div().text_size(px(11.)).text_color(muted).child("Vaults ▾")),
                    ),
            )
            .child(
                Button::new("host-picker-local-btn")
                    .primary()
                    .small()
                    .icon(Icon::default().data(glyph::LOCAL_HOST))
                    .label("Local"),
            );

        let search_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(40.))
            .px_3()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_color(muted)
                    .text_size(px(13.))
                    .child(Icon::new(IconName::Search).small())
                    .child("Search"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_color(muted)
                    .child(Icon::default().data(glyph::TAG).small())
                    .child(Icon::default().data(glyph::CALENDAR).small()),
            );

        let section_title = div()
            .px_4()
            .pt_3()
            .pb_1()
            .font_weight(gpui_kit::FontWeight::BOLD)
            .text_size(px(14.))
            .child("Hosts");

        let host_rows: Vec<_> = self
            .available_hosts
            .iter()
            .map(|host| {
                let host_clone = host.clone();
                let label = host.label.clone();
                let username = if host.username.is_empty() {
                    "root".to_string()
                } else {
                    host.username.clone()
                };
                let subtitle = format!("ssh, {username}");
                let id = format!("host-picker-item-{}", host.id.as_str());

                div()
                    .id(SharedString::from(id))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(cb) = &this.on_connect {
                            cb(host_clone.clone(), window, cx);
                        }
                    }))
                    .child(
                        div()
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(0xe95420))
                            .child(
                                Icon::default()
                                    .data(glyph::UBUNTU)
                                    .small()
                                    .text_color(Hsla::from(rgb(0xffffff))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_size(px(13.))
                                    .text_color(foreground)
                                    .child(label),
                            )
                            .child(div().text_size(px(11.)).text_color(muted).child(subtitle)),
                    )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .child(header)
            .child(search_bar)
            .child(section_title)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scrollbar()
                    .children(host_rows),
            )
    }

    fn render_right_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.client.is_some() {
            div()
                .flex()
                .flex_col()
                .size_full()
                .overflow_hidden()
                .child(self.render_remote_header(cx))
                .child(self.render_toolbar(cx))
                .child(self.render_body(cx))
                .into_any_element()
        } else if self.show_host_picker {
            self.render_host_picker(cx).into_any_element()
        } else {
            self.render_empty_right(cx).into_any_element()
        }
    }

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
        let border = cx.theme().border;

        let left = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .border_r_1()
            .border_color(border)
            .child(self.render_local_header(cx))
            .child(self.render_local_path_bar(cx))
            .child(self.render_local_body(cx));

        let right = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(self.render_right_pane(cx));

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .text_color(foreground)
            .track_focus(&self.focus_handle)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(left)
                    .child(right),
            )
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

/// The trail the pane renders: the crate-style breadcrumbs with the bare root
/// crumb dropped, so it starts at the first real segment like the reference.
fn breadcrumb_crumbs(path: &str) -> Vec<(String, String)> {
    let crumbs = breadcrumbs(path);
    if crumbs.len() > 1 {
        crumbs
            .into_iter()
            .filter(|(label, _)| label != "/")
            .collect()
    } else {
        crumbs
    }
}

/// `M/D/YYYY, h:MM AM/PM` for the Date Modified column.
fn format_modified(modified: SystemTime) -> String {
    let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return "—".to_string();
    };
    let seconds = since.as_secs();

    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let hour_24 = (time_of_day / 3_600) as u32;
    let minute = ((time_of_day % 3_600) / 60) as u32;

    let (year, month, day) = civil_from_days(days as i64);

    let (hour, am_pm) = match hour_24 {
        0 => (12, "AM"),
        1..=11 => (hour_24, "AM"),
        12 => (12, "PM"),
        _ => (hour_24 - 12, "PM"),
    };

    format!("{month}/{day}/{year}, {hour}:{minute:02} {am_pm}")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

fn describe_failure(path: &str, error: &SftpError) -> String {
    format!("Failed to read {path}: {error}")
}

fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Dir => "folder",
        FileKind::File => "file",
        FileKind::Symlink => "symlink",
        FileKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

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
    fn breadcrumb_crumbs_drop_the_bare_root() {
        assert_eq!(
            breadcrumb_crumbs("/home/user"),
            vec![
                ("home".to_string(), "/home".to_string()),
                ("user".to_string(), "/home/user".to_string()),
            ]
        );
        assert_eq!(
            breadcrumb_crumbs("/"),
            vec![("/".to_string(), "/".to_string())]
        );
        assert!(breadcrumb_crumbs("").is_empty());
    }

    #[test]
    fn format_modified_renders_utc_calendar_time() {
        let at = |seconds| format_modified(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(at(0), "1/1/1970, 12:00 AM");
        assert_eq!(at(43_200), "1/1/1970, 12:00 PM");
        assert_eq!(at(1_789_635_780), "9/17/2026, 9:03 AM");
    }
}
