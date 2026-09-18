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
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, InteractiveElementExt as _, Sizable as _,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, rgb, AnyElement, App, AppContext as _, Context, Div, Entity, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window,
};
use sshdeck_core::Host;
use sshdeck_sftp::{
    DirEntry, FileKind, SftpClient, SftpError, TransferEvent, TransferId, TransferState,
};

use crate::glyph;

/// Directory listings are truncated to this many entries so a pathological
/// directory cannot become a memory cliff (docs/BUDGET.md).
const MAX_DIR_ENTRIES: usize = 1_000;

/// Below this window width the 50/50 split collapses to a single pane with a
/// Local/Remote tab switcher.
const SINGLE_PANE_W: f32 = 750.0;
/// Below this window width the Kind column hides.
const HIDE_KIND_W: f32 = 600.0;
/// Below this window width the Size column hides too.
const HIDE_SIZE_W: f32 = 450.0;

/// Whether the dual pane collapses to one pane with a tab switcher.
fn is_single_pane(win_w: f32) -> bool {
    win_w < SINGLE_PANE_W
}

/// Whether the Kind column fits at this window width.
fn show_kind_col(win_w: f32) -> bool {
    win_w >= HIDE_KIND_W
}

/// Whether the Size column fits at this window width.
fn show_size_col(win_w: f32) -> bool {
    win_w >= HIDE_SIZE_W
}

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

type ConnectHandler = Box<dyn Fn(Host, &mut Window, &mut App)>;
type ShowLogsHandler = Box<dyn Fn(&mut Window, &mut App)>;

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
    host_search: Entity<InputState>,
    /// A picked host whose transport is still opening (`reconcile_sftp` sets
    /// this; `set_client` clears it). Renders the connecting branch.
    connecting: Option<String>,
    on_connect: Option<ConnectHandler>,
    /// Invoked by the SFTP connecting block's "Show logs" pill; wired by
    /// `main.rs` (the pane cannot reach the left nav itself).
    on_show_logs: Option<ShowLogsHandler>,
    /// Which pane the narrow tab switcher shows. Ignored at wide widths.
    show_local: bool,
    // Browser chrome state: per-pane filter inputs, date sort direction,
    // and whether the Actions dropdown is open — per pane so the two
    // browsers never fight over one control.
    local_filter: Entity<InputState>,
    remote_filter: Entity<InputState>,
    local_sort_asc: bool,
    remote_sort_asc: bool,
    local_actions_open: bool,
    remote_actions_open: bool,
    // Host picker state: selected row, tag-only filter, alpha sort.
    picker_selected: Option<String>,
    picker_tagged_only: bool,
    picker_sort_alpha: bool,
    /// Keeps the host search re-rendering as it is typed; dropped with the pane.
    _subscriptions: Vec<Subscription>,
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
        let host_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search hosts"));
        let local_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
        let remote_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
        let mut subscriptions =
            vec![cx.subscribe_in(&host_search, window, |_, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })];
        for filter in [&local_filter, &remote_filter] {
            subscriptions.push(cx.subscribe_in(filter, window, |_, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }

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
            host_search,
            connecting: None,
            on_connect: None,
            on_show_logs: None,
            show_local: true,
            local_filter,
            remote_filter,
            local_sort_asc: true,
            remote_sort_asc: false,
            local_actions_open: false,
            remote_actions_open: false,
            picker_selected: None,
            picker_tagged_only: false,
            picker_sort_alpha: false,
            _subscriptions: subscriptions,
        }
    }

    /// Supplies the list of inventory hosts for the right-hand "Select Host" picker.
    #[allow(dead_code)]
    pub fn set_available_hosts(&mut self, hosts: Vec<Host>, cx: &mut Context<Self>) {
        self.available_hosts = hosts;
        cx.notify();
    }

    /// Sets the label of the connected remote host (e.g. "Ali Jawwad").
    #[allow(dead_code)]
    pub fn set_remote_host_label(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        self.remote_host_label = label;
        cx.notify();
    }

    /// Registers the callback invoked when the user selects a host in the right-pane picker.
    #[allow(dead_code)]
    pub fn set_on_connect<F>(&mut self, f: F)
    where
        F: Fn(Host, &mut Window, &mut App) + 'static,
    {
        self.on_connect = Some(Box::new(f));
    }

    /// Registers the callback behind the SFTP connecting block's "Show logs" pill.
    #[allow(dead_code)]
    pub fn set_on_show_logs<F>(&mut self, f: F)
    where
        F: Fn(&mut Window, &mut App) + 'static,
    {
        self.on_show_logs = Some(Box::new(f));
    }

    /// Toggles the local Date Modified sort and re-sorts the listing.
    fn toggle_local_sort(&mut self, cx: &mut Context<Self>) {
        self.local_sort_asc = !self.local_sort_asc;
        sort_local_entries(&mut self.local_entries, self.local_sort_asc);
        cx.notify();
    }

    /// Toggles the remote Date Modified sort and re-sorts the listing.
    fn toggle_remote_sort(&mut self, cx: &mut Context<Self>) {
        self.remote_sort_asc = !self.remote_sort_asc;
        sort_remote_entries(&mut self.entries, self.remote_sort_asc);
        cx.notify();
    }

    /// The live SFTP handle, if one has been supplied.
    #[allow(dead_code)]
    pub fn client(&self) -> Option<&SftpClient> {
        self.client.as_deref()
    }

    /// The canonical path currently being listed on the remote server; empty until first load.
    #[allow(dead_code)]
    pub fn current_path(&self) -> &str {
        &self.cwd
    }

    /// Marks a picked host as connecting; cleared by [`Self::set_client`].
    /// The wiring pass sets this while the transport opens so the right pane
    /// shows a connecting branch instead of the empty state.
    #[allow(dead_code)]
    pub fn set_connecting(&mut self, label: Option<String>, cx: &mut Context<Self>) {
        self.connecting = label;
        cx.notify();
    }

    /// Attaches or detaches the remote SFTP transport.
    pub fn set_client(&mut self, client: Option<SftpClient>, cx: &mut Context<Self>) {
        self.watch_generation = self.watch_generation.wrapping_add(1);
        self.connecting = None;
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

    /// Reloads the current remote directory.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    /// Uploads the selected local file or directory to the remote directory.
    pub fn upload_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(idx)) = (self.client.clone(), self.local_selected) else {
            return;
        };
        let Some(entry) = self.local_entries.get(idx) else {
            return;
        };
        let local_path = self.local_cwd.join(&entry.name);
        let remote_path = format!("{}/{}", self.cwd.trim_end_matches('/'), entry.name);
        let is_dir = entry.is_dir;

        cx.spawn(async move |pane, cx| {
            let res = if is_dir {
                client.upload_dir(local_path, remote_path).await
            } else {
                client.upload(local_path, remote_path).await
            };
            pane.update(cx, |this, cx| match res {
                Ok(_) => this.reload(cx),
                Err(err) => {
                    this.error = Some(err.to_string());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Downloads the selected remote file or directory to the local directory.
    pub fn download_selected(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(idx)) = (self.client.clone(), self.selected) else {
            return;
        };
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let remote_path = format!("{}/{}", self.cwd.trim_end_matches('/'), entry.name());
        let local_path = self.local_cwd.join(entry.name());
        let is_dir = entry.is_dir();

        cx.spawn(async move |pane, cx| {
            let res = if is_dir {
                client.download_dir(remote_path, local_path).await
            } else {
                client.download(remote_path, local_path).await
            };
            pane.update(cx, |this, cx| match res {
                Ok(_) => {
                    let local_cwd = this.local_cwd.clone();
                    this.load_local(local_cwd, cx);
                }
                Err(err) => {
                    this.error = Some(err.to_string());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ── Local browser rendering ───────────────────────────────────────────

    fn render_local_header(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().table_row_border;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;

        let bar = div()
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
                            .bg(rgb(0x1c4774))
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
                    .gap_1()
                    .child(
                        div().w(px(140.)).child(
                            Input::new(&self.local_filter)
                                .small()
                                .bordered(false)
                                .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                        ),
                    )
                    .child(
                        Button::new("local-actions")
                            .ghost()
                            .small()
                            .label("Actions")
                            .icon(IconName::ChevronDown)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.local_actions_open = !this.local_actions_open;
                                cx.notify();
                            })),
                    ),
            );

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .child(bar)
            .when(self.local_actions_open, |el| {
                let can_upload = self.client.is_some() && self.local_selected.is_some();
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .border_b_1()
                        .border_color(border)
                        .when(can_upload, |el| {
                            el.child(
                                Button::new("local-action-upload")
                                    .ghost()
                                    .small()
                                    .label("Upload to Remote")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.local_actions_open = false;
                                        this.upload_selected(cx);
                                    })),
                            )
                        })
                        .child(
                            Button::new("local-action-refresh")
                                .ghost()
                                .small()
                                .label("Refresh")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let path = this.local_cwd.clone();
                                    this.local_actions_open = false;
                                    this.load_local(path, cx);
                                })),
                        )
                        .child(
                            Button::new("local-action-home")
                                .ghost()
                                .small()
                                .label("Go to home")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let home = std::env::var("HOME")
                                        .map(PathBuf::from)
                                        .unwrap_or_else(|_| PathBuf::from("/"));
                                    this.local_actions_open = false;
                                    this.navigate_local(home, cx);
                                })),
                        )
                        .child(
                            Button::new("local-action-up")
                                .ghost()
                                .small()
                                .label("Go up")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.local_actions_open = false;
                                    this.navigate_local_up(cx);
                                })),
                        ),
                )
            })
    }

    fn render_local_path_bar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().table_row_border;
        let muted = cx.theme().muted_foreground;
        let folder_tint = rgb(0x5aa9ff);

        let can_back = self.local_history_idx > 0;
        let can_forward = self.local_history_idx + 1 < self.local_history.len();

        let mut crumbs: Vec<AnyElement> = Vec::new();
        let mut path_accum = PathBuf::from("/");
        let valid_comps: Vec<_> = self
            .local_cwd
            .components()
            .filter(|comp| {
                let name = comp.as_os_str().to_string_lossy();
                !name.is_empty() && name != "/"
            })
            .collect();

        for (idx, comp) in valid_comps.into_iter().enumerate() {
            let name = comp.as_os_str().to_string_lossy().to_string();
            path_accum.push(&name);
            let target_path = path_accum.clone();

            if idx > 0 {
                crumbs.push(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(muted)
                        .into_any_element(),
                );
            }

            crumbs.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
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
                        .truncate()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_local(target_path.clone(), cx);
                        })),
                    )
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
                    .min_w(px(0.))
                    .overflow_hidden()
                    .overflow_x_scrollbar()
                    .children(crumbs),
            )
    }

    fn render_local_body(&self, win_w: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().table_row_border;
        let muted = cx.theme().muted_foreground;
        let hover_bg = cx.theme().accent;
        let folder_tint = rgb(0x5aa9ff);
        let show_kind = show_kind_col(win_w);
        let show_size = show_size_col(win_w);
        let sort_caret = if self.local_sort_asc { "▲" } else { "▼" };
        let filter = self.local_filter.read(cx).value().to_string();

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(36.))
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().min_w(px(0.)).child("Name"))
            .child(
                div().w(px(170.)).child(
                    Button::new("local-sort-date")
                        .ghost()
                        .xsmall()
                        .label(SharedString::from(format!("Date Modified {sort_caret}")))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_local_sort(cx))),
                ),
            )
            .when(show_size, |el| el.child(div().w(px(90.)).child("Size")))
            .when(show_kind, |el| el.child(div().w(px(80.)).child("Kind")));

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
                    .min_h(px(36.))
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
                        div().flex().flex_col().flex_1().min_w(px(0.)).child(
                            div()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(".."),
                        ),
                    )
                    .child(div().w(px(170.)))
                    .when(show_size, |el| el.child(div().w(px(90.))))
                    .when(show_kind, |el| el.child(div().w(px(80.))))
                    .into_any_element(),
            );
        }

        for (idx, entry) in self
            .local_entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches_filter(&entry.name, &filter))
        {
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
            // Documented `#f0f3f5`; the accent token (`#edf1f2`) is one shade off.
            let selected_bg: Hsla = rgb(0xf0f3f5).into();

            rows.push(
                div()
                    .id(SharedString::from(format!("local-item-{name}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .min_h(px(36.))
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
                    .when(show_size, |el| {
                        el.child(
                            div()
                                .w(px(90.))
                                .text_xs()
                                .text_color(muted)
                                .font_family("Menlo")
                                .child(size.clone()),
                        )
                    })
                    .when(show_kind, |el| {
                        el.child(
                            div()
                                .w(px(80.))
                                .text_xs()
                                .text_color(muted)
                                .child(kind.clone()),
                        )
                    })
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
        let border = cx.theme().table_row_border;
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let label = self
            .remote_host_label
            .clone()
            .unwrap_or_else(|| "Remote Host".to_string());

        let bar = div()
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
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .size(px(28.))
                            .flex_shrink_0()
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
                            .overflow_hidden()
                            .truncate()
                            .child(label),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .flex_shrink_0()
                    .child(
                        div().w(px(140.)).child(
                            Input::new(&self.remote_filter)
                                .small()
                                .bordered(false)
                                .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                        ),
                    )
                    .child(
                        Button::new("remote-actions")
                            .ghost()
                            .small()
                            .label("Actions")
                            .icon(IconName::ChevronDown)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.remote_actions_open = !this.remote_actions_open;
                                cx.notify();
                            })),
                    ),
            );

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .child(bar)
            .when(self.remote_actions_open, |el| {
                let can_download = self.selected.is_some();
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .border_b_1()
                        .border_color(border)
                        .when(can_download, |el| {
                            el.child(
                                Button::new("remote-action-download")
                                    .ghost()
                                    .small()
                                    .label("Download to Local")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.remote_actions_open = false;
                                        this.download_selected(cx);
                                    })),
                            )
                        })
                        .child(
                            Button::new("remote-action-refresh")
                                .ghost()
                                .small()
                                .label("Refresh")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.remote_actions_open = false;
                                    this.refresh(cx);
                                })),
                        )
                        .child(
                            Button::new("remote-action-up")
                                .ghost()
                                .small()
                                .label("Go up")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.remote_actions_open = false;
                                    this.go_up(cx);
                                })),
                        ),
                )
            })
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().table_row_border;
        let muted = cx.theme().muted_foreground;
        let folder_tint = rgb(0x5aa9ff);
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
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(muted)
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
                    .truncate()
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
                    .min_w(px(0.))
                    .overflow_hidden()
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

    fn render_body(&self, win_w: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().table_row_border;
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let show_kind = show_kind_col(win_w);
        let show_size = show_size_col(win_w);
        let sort_caret = if self.remote_sort_asc { "▲" } else { "▼" };
        let filter = self.remote_filter.read(cx).value().to_string();

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
            .h(px(36.))
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().min_w(px(0.)).child("Name"))
            .child(
                div().w(px(170.)).child(
                    Button::new("remote-sort-date")
                        .ghost()
                        .xsmall()
                        .label(SharedString::from(format!("Date Modified {sort_caret}")))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_remote_sort(cx))),
                ),
            )
            .when(show_size, |el| el.child(div().w(px(90.)).child("Size")))
            .when(show_kind, |el| el.child(div().w(px(80.)).child("Kind")));

        let mut rows: Vec<AnyElement> = Vec::new();

        if parent_path(&self.cwd).is_some() {
            let hover_bg = cx.theme().accent;
            let folder_tint = rgb(0x5aa9ff);
            rows.push(
                div()
                    .id("remote-entry-parent")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .min_h(px(36.))
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
                        div().flex().flex_col().flex_1().min_w(px(0.)).child(
                            div()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child(".."),
                        ),
                    )
                    .child(div().w(px(170.)))
                    .when(show_size, |el| el.child(div().w(px(90.))))
                    .when(show_kind, |el| el.child(div().w(px(80.))))
                    .into_any_element(),
            );
        }

        for (index, entry) in self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches_filter(entry.name(), &filter))
        {
            rows.push(
                self.render_entry(index, entry, show_kind, show_size, cx)
                    .into_any_element(),
            );
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
        show_kind: bool,
        show_size: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().table_row_border;
        let hover_bg = cx.theme().accent;
        // Documented `#f0f3f5`; the accent token (`#edf1f2`) is one shade off.
        let selected_bg: Hsla = rgb(0xf0f3f5).into();
        let folder_tint = rgb(0x5aa9ff);

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
            .min_h(px(36.))
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
            .when(show_size, |el| {
                el.child(
                    div()
                        .w(px(90.))
                        .text_xs()
                        .text_color(muted)
                        .font_family("Menlo")
                        .child(size.clone()),
                )
            })
            .when(show_kind, |el| {
                el.child(div().w(px(80.)).text_xs().text_color(muted).child(kind))
            })
    }

    fn render_empty_right(&self, win_w: f32, cx: &mut Context<Self>) -> Div {
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
            // White in the light theme; the host picker behind "Select host"
            // keeps the content surface.
            .bg(cx.theme().background)
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
                    .max_w(px((win_w * 0.9).min(340.)))
                    .text_center()
                    .text_size(px(14.))
                    .text_color(muted)
                    .child("Start by connecting to a saved host\nto manage your files with SFTP."),
            )
            .child(
                // Default (secondary) variant: its light fill is `#e6ebed`,
                // the reference's Select-host background.
                Button::new("sftp-select-host-btn")
                    .label("Select host")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_host_picker = true;
                        cx.notify();
                    })),
            )
    }

    fn render_host_picker(&self, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().table_row_border;
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
                div()
                    .id("host-picker-local")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1p5()
                    .h(px(28.))
                    .px_3()
                    .rounded(px(6.))
                    .bg(cx.theme().primary)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Back to the local side: the empty state behind the picker.
                        this.show_host_picker = false;
                        cx.notify();
                    }))
                    .child(
                        Icon::default()
                            .data(glyph::LOCAL_HOST)
                            .small()
                            .text_color(Hsla::from(rgb(0xffffff))),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(0xffffff))
                            .child("Local"),
                    ),
            );

        let search_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(48.))
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(border)
            .child(
                div().flex_1().min_w(px(0.)).child(
                    Input::new(&self.host_search)
                        .small()
                        .bordered(false)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small().text_color(muted)),
                ),
            )
            .child(
                Button::new("host-picker-tag-filter")
                    .ghost()
                    .small()
                    .icon(Icon::default().data(glyph::TAG).small())
                    .tooltip("Only hosts with tags")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker_tagged_only = !this.picker_tagged_only;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("host-picker-alpha-sort")
                    .ghost()
                    .small()
                    .icon(Icon::default().data(glyph::CALENDAR).small())
                    .tooltip("Sort alphabetically")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker_sort_alpha = !this.picker_sort_alpha;
                        cx.notify();
                    })),
            );

        let section_title = div()
            .px_4()
            .pt_3()
            .pb_1()
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_size(px(14.))
            .child("Hosts");

        let query = self.host_search.read(cx).value().trim().to_lowercase();
        let tagged_only = self.picker_tagged_only;
        let sort_alpha = self.picker_sort_alpha;
        let selected_id = self.picker_selected.clone();
        let mut hosts: Vec<&Host> = self
            .available_hosts
            .iter()
            .filter(|host| {
                (query.is_empty()
                    || host.label.to_lowercase().contains(&query)
                    || host.username.to_lowercase().contains(&query))
                    && (!tagged_only || !host.tags.is_empty())
            })
            .collect();
        if sort_alpha {
            hosts.sort_by_key(|a| a.label.to_lowercase());
        }
        let host_rows: Vec<_> = hosts
            .into_iter()
            .map(|host| {
                let host_clone = host.clone();
                let label = host.label.clone();
                let connecting_label = label.clone();
                let subtitle = picker_subtitle(host);
                let id = format!("host-picker-item-{}", host.id.as_str());
                let selected = selected_id.as_deref() == Some(host.id.as_str());

                div()
                    .id(SharedString::from(id))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    // Documented `#f0f3f5`; the accent token is one shade off.
                    .when(selected, |el| el.bg(rgb(0xf0f3f5)))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Show the connecting branch immediately; `set_client`
                        // clears it once the transport lands or is dropped.
                        this.picker_selected = Some(host_clone.id.as_str().to_string());
                        this.connecting = Some(connecting_label.clone());
                        this.show_host_picker = false;
                        cx.notify();
                        if let Some(cb) = &this.on_connect {
                            cb(host_clone.clone(), window, cx);
                        }
                    }))
                    .child(
                        div()
                            .size(px(40.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.))
                            .bg(rgb(0xe95420))
                            .child(
                                Icon::default()
                                    .data(glyph::UBUNTU)
                                    .size(px(22.))
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
                                    .text_size(px(14.))
                                    .text_color(foreground)
                                    .child(label),
                            )
                            .child(div().text_size(px(12.)).text_color(muted).child(subtitle)),
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
                    .children(host_rows)
                    .when(self.available_hosts.is_empty(), |el| {
                        el.child(
                            div()
                                .px_4()
                                .py_2()
                                .text_size(px(13.))
                                .text_color(muted)
                                .child("No hosts in the vault yet."),
                        )
                    }),
            )
    }

    /// The connecting branch: the picked host's emblem, endpoint, rail and
    /// pills, with the pane chrome (header + toolbar) kept visible instead of
    /// swapping the whole pane for a spinner.
    fn render_connecting(&self, label: &str, win_w: f32, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;
        let rail_grey = rgb(0x5a5e73);
        let endpoint = self
            .available_hosts
            .iter()
            .find(|host| host.label == label)
            .map(|host| format!("SSH {}:{}", host.address, host.port))
            .unwrap_or_else(|| format!("SSH {label}"));

        // Neutral grey pills wrapping ghost buttons, so the button chrome
        // stays flat while the pill carries the fill.
        let pill = |label: Button| {
            div()
                .h(px(36.))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .bg(rgb(0xe6ebed))
                .child(label)
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().background)
            .child(self.render_remote_header(cx))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.))
                    .p_6()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .w_full()
                            .max_w(px((win_w * 0.9).min(560.)))
                            .min_w(px(0.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_3()
                                    .w_full()
                                    .min_w(px(0.))
                                    .child(
                                        div()
                                            .size(px(40.))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(10.))
                                            .bg(rgb(0xe95420))
                                            .child(
                                                Icon::default()
                                                    .data(glyph::UBUNTU)
                                                    .size(px(24.))
                                                    .text_color(Hsla::from(rgb(0xffffff))),
                                            ),
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
                                                    .text_size(px(15.))
                                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                                    .text_color(foreground)
                                                    .truncate()
                                                    .child(SharedString::from(label.to_string())),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(muted)
                                                    .truncate()
                                                    .child(SharedString::from(endpoint)),
                                            ),
                                    )
                                    .child(pill(
                                        Button::new("sftp-connecting-logs")
                                            .ghost()
                                            .small()
                                            .label("Show logs")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                if let Some(cb) = &this.on_show_logs {
                                                    cb(window, cx);
                                                }
                                            })),
                                    )),
                            )
                            // ponytail: static rail, not an animation — the only
                            // budgeted timer is the cursor blink. Upgrade with a
                            // rotation step driven by the pane's notify loop.
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .w_full()
                                    .child(
                                        div()
                                            .size(px(28.))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_full()
                                            .bg(cx.theme().primary)
                                            .child(
                                                Icon::default()
                                                    .data(glyph::PLUG)
                                                    .size(px(16.))
                                                    .text_color(rgb(0xffffff)),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w(px(260.))
                                            .flex_1()
                                            .h(px(3.))
                                            .rounded_full()
                                            .bg(rail_grey),
                                    )
                                    .child(
                                        div()
                                            .size(px(28.))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_full()
                                            .bg(rail_grey)
                                            .child(
                                                Icon::default()
                                                    .data(glyph::TERMINAL_PROMPT)
                                                    .size(px(16.))
                                                    .text_color(rgb(0xffffff)),
                                            ),
                                    ),
                            )
                            .child(
                                div().flex().flex_row().items_center().child(pill(
                                    Button::new("sftp-connecting-cancel")
                                        .ghost()
                                        .small()
                                        .label("Close")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            // Close returns to the picker; `reconcile_sftp`
                                            // keeps the detached state until the next
                                            // attach attempt.
                                            this.connecting = None;
                                            this.show_host_picker = true;
                                            cx.notify();
                                        })),
                                )),
                            ),
                    ),
            )
    }

    fn render_right_pane(&self, win_w: f32, cx: &mut Context<Self>) -> AnyElement {
        if self.client.is_some() {
            div()
                .flex()
                .flex_col()
                .size_full()
                .overflow_hidden()
                .child(self.render_remote_header(cx))
                .child(self.render_toolbar(cx))
                .child(self.render_body(win_w, cx))
                .into_any_element()
        } else if let Some(label) = self.connecting.clone() {
            self.render_connecting(&label, win_w, cx).into_any_element()
        } else if self.show_host_picker {
            self.render_host_picker(cx).into_any_element()
        } else {
            self.render_empty_right(win_w, cx).into_any_element()
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;
        let win_w = f32::from(window.bounds().size.width);

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
            .child(self.render_local_body(win_w, cx));

        let right = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(self.render_right_pane(win_w, cx));

        // Narrow windows collapse the 50/50 split to one pane with a
        // Local/Remote tab switcher; wide windows keep both panes.
        let panes: AnyElement = if is_single_pane(win_w) {
            let switcher = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(
                    Button::new("sftp-tab-local")
                        .small()
                        .label("Local")
                        .when(!self.show_local, |b| b.ghost())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_local = true;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("sftp-tab-remote")
                        .small()
                        .label("Remote")
                        .when(self.show_local, |b| b.ghost())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_local = false;
                            cx.notify();
                        })),
                );
            div()
                .flex()
                .flex_col()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .child(switcher)
                .child(if self.show_local { left } else { right })
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_row()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .child(left)
                .child(right)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(background)
            .text_color(foreground)
            .track_focus(&self.focus_handle)
            .child(panes)
            .when(!self.transfers.is_empty(), |this| {
                this.child(self.render_transfers(cx))
            })
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

/// Orders two modification times with unscannable (`None`) entries pinned
/// last in both directions (`Option::cmp` would put them first ascending).
fn cmp_modified(a: Option<SystemTime>, b: Option<SystemTime>, asc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(x), Some(y)) => {
            if asc {
                x.cmp(&y)
            } else {
                y.cmp(&x)
            }
        }
    }
}

/// Sorts a local listing by modification time; `None` sorts last in both
/// directions so unscannable entries never jump to the top.
fn sort_local_entries(entries: &mut [LocalEntry], asc: bool) {
    entries.sort_by(|a, b| cmp_modified(a.modified, b.modified, asc));
}

/// Sorts a remote listing by modification time, same `None`-last rule.
fn sort_remote_entries(entries: &mut [DirEntry], asc: bool) {
    entries.sort_by(|a, b| cmp_modified(a.modified(), b.modified(), asc));
}

/// The picker's subtitle line: `ssh` plus the host's own tags plus the login
/// name (`Host` has no protocol field, so tags carry e.g. `telnet` locally).
fn picker_subtitle(host: &Host) -> String {
    // Mirrors `host_subtitle` in main.rs: protocols, then one login per
    // protocol (`ssh, telnet, root, root`).
    let mut tokens: Vec<String> = host.protocols().to_vec();
    if tokens.is_empty() {
        tokens.push("ssh".to_string());
    }
    if !host.username.is_empty() {
        let logins = std::iter::repeat_n(host.username.clone(), tokens.len());
        tokens.extend(logins);
    }
    tokens.join(", ")
}

/// Case-insensitive name filter for both browsers; empty query keeps all.
fn matches_filter(name: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty() || name.to_lowercase().contains(&query)
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
    fn sort_entries_toggle_none_last_both_directions() {
        let at = |secs: u64| Some(UNIX_EPOCH + Duration::from_secs(secs));
        let mut entries = vec![
            LocalEntry {
                name: "b".into(),
                is_dir: false,
                size: 1,
                permissions: "-rw-r--r--".into(),
                modified: None,
            },
            LocalEntry {
                name: "a".into(),
                is_dir: false,
                size: 1,
                permissions: "-rw-r--r--".into(),
                modified: at(20),
            },
            LocalEntry {
                name: "c".into(),
                is_dir: false,
                size: 1,
                permissions: "-rw-r--r--".into(),
                modified: at(10),
            },
        ];
        sort_local_entries(&mut entries, true);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["c", "a", "b"]
        );
        sort_local_entries(&mut entries, false);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
    }

    #[test]
    fn picker_subtitle_lists_protocols_and_user() {
        let mut host = Host::new("box", "10.0.0.1");
        host.username = "root".into();
        assert_eq!(picker_subtitle(&host), "ssh, root");
        host.protocols.push("telnet".into());
        host.username = "root".into();
        assert_eq!(picker_subtitle(&host), "ssh, telnet, root, root");
    }

    #[test]
    fn matches_filter_is_case_insensitive_and_empty_passes() {
        assert!(matches_filter("Downloads", ""));
        assert!(matches_filter("Downloads", "  down "));
        assert!(matches_filter("Downloads", "LOAD"));
        assert!(!matches_filter("Downloads", "music"));
    }

    #[test]
    fn responsive_breakpoints_collapse_panes_then_columns() {
        assert!(is_single_pane(749.9));
        assert!(!is_single_pane(750.0));
        assert!(show_kind_col(600.0));
        assert!(!show_kind_col(599.9));
        assert!(show_size_col(450.0));
        assert!(!show_size_col(449.9));
    }

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

    #[test]
    fn upload_download_target_paths_format_correctly() {
        let cwd = "/home/user/";
        let entry_name = "test.txt";
        let remote_path = format!("{}/{}", cwd.trim_end_matches('/'), entry_name);
        assert_eq!(remote_path, "/home/user/test.txt");

        let root_cwd = "/";
        let root_remote_path = format!("{}/{}", root_cwd.trim_end_matches('/'), entry_name);
        assert_eq!(root_remote_path, "/test.txt");

        let local_cwd = PathBuf::from("/Users/test");
        let local_path = local_cwd.join(entry_name);
        assert_eq!(local_path, PathBuf::from("/Users/test/test.txt"));
    }
}
