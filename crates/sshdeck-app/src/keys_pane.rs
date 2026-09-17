//! SSH keys and known-hosts management UI.
//!
//! Contract for the wiring pass (do not change without updating `main.rs`):
//! `KeysPane::new(window, cx) -> Self`, plus `impl Render for KeysPane`.
//! The root view constructs it with `cx.new(|cx| KeysPane::new(window, cx))`.
//!
//! Security contract: `sshdeck_core::session` fails closed on unknown and
//! changed host keys. This pane is the human-in-the-loop path that *follows*
//! an explicit confirmation, and it never weakens that default:
//!
//! * A brand-new host key is written only by the crate's `learn`, and only
//!   from a click on "Check & trust".
//! * A changed key is a man-in-the-middle signal. `learn` writes nothing on a
//!   conflict; the pane shows the recorded (old) and offered (new) fingerprints
//!   and requires a second click on a danger button before `trust_changed` runs.
//!
//! Everything that touches disk runs on the background executor; nothing here
//! blocks a frame.
//!
//! Presentation (`docs/re/`, Termius `3.13.24` Keychain and `3.13.37` Known
//! Hosts):
//!
//! * One toolbar row (`56px`, `cx.theme().popover`) holding the section switch,
//!   the contextual actions and the icon controls, then the body on
//!   `cx.theme().sidebar` (`#edf1f2` in the light theme) — the reference's
//!   grey content behind white cards.
//! * Content is a `flex_wrap` card grid (white, `10px` radius, `shadow_xs`,
//!   `300px` basis) with a `40px` navy glyph tile, matching the reference's key
//!   and known-host tiles. A list view is offered as well (44px rows, hairline
//!   separators) because a fingerprint needs the horizontal room.
//! * `Certificate`, `Touch ID` and `FIDO2` are rendered **disabled** with a
//!   tooltip: the reference shows them, `sshdeck_core` has no x509, passkey or
//!   FIDO2 store, and shipping them enabled would be a dead write path. This is
//!   the same dead-control convention `logs_pane` uses.
//! * A changed host key is loud, never a tint: a danger-bordered card with the
//!   old and new fingerprints side by side in labelled boxes, a danger button
//!   for the second confirmation, and a `CHANGED` pill on the row it belongs to.
//!
//! Hardcoded values (no theme token exists):
//! * The navy identity tile `rgba(0x1c4774ff)`. Termius uses the same navy for
//!   key, identity and known-host tiles in both modes; the bundled theme has no
//!   token for it. Every other colour comes from `cx.theme()` (`popover`,
//!   `sidebar`, `border`, `muted`, `muted_foreground`, `foreground`, `accent`,
//!   `primary`, `primary_foreground`, `danger`, `danger_foreground`).

use std::path::{Path, PathBuf};

use gpui_kit::component::{
    alert::Alert,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    notification::Notification,
    scroll::{Scrollable, ScrollableElement as _},
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    WindowExt as _,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, rgba, AnyElement, App, AppContext as _, ClipboardItem, Context, Div, Entity,
    Focusable as _, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Stateful, Styled as _, Subscription, Window,
};
use sshdeck_core::keys::{self, KeyKind};
use sshdeck_core::known_hosts::{self, KnownHostsError, Learned};
use sshdeck_core::HostStore;

/// The conventional SSH port used when the trust form's port field is empty.
const DEFAULT_PORT: u16 = 22;
/// Keys are small text files; a larger file is not one, so skip it rather than
/// read it into memory.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024;
/// Characters kept at each end when a fingerprint is shortened.
const SHORT_FINGERPRINT_EDGE: usize = 8;
/// Grid cards grow from this basis to fill the row, so the grid reflows with
/// the window instead of clipping.
const CARD_BASIS: f32 = 300.;
/// Termius' navy identity tile, RGBA (the alpha byte is part of the literal:
/// `rgba` reads `0xRRGGBBAA`). The one literal in this pane.
const TILE_BG: u32 = 0x1c4774ff;

/// Glyphs the bundled default icon set does not carry.
///
/// `gpui_kit::component::IconName` exposes only the ~100 default icons and the
/// app's asset source (`gpui_kit::assets::Assets`) embeds exactly those, so a
/// key-, host-, fingerprint- or security-key-shaped glyph cannot be named. The
/// geometry below is ours; only the 24x24 grid follows the bundled Lucide
/// style, and `Icon::data` renders raw SVG without touching the asset source.
mod glyph {
    pub const KEY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="16" r="4"/><path d="M11 13 20 4"/><path d="M16.5 7.5 19 10"/></svg>"#;
    pub const HOST: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 12.5a7.5 7.5 0 0 1 15 0"/><path d="M8.5 13.5a3.5 3.5 0 0 1 7 0"/><path d="M8.5 13.5V19"/><path d="M15.5 13.5V19"/></svg>"#;
    pub const GRID: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><rect x="3" y="3" width="8" height="8" rx="2"/><rect x="13" y="3" width="8" height="8" rx="2"/><rect x="3" y="13" width="8" height="8" rx="2"/><rect x="13" y="13" width="8" height="8" rx="2"/></svg>"#;
    pub const LIST: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><rect x="3" y="5" width="18" height="4" rx="2"/><rect x="3" y="10" width="18" height="4" rx="2"/><rect x="3" y="15" width="18" height="4" rx="2"/></svg>"#;
    pub const FINGERPRINT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5a7 7 0 0 1 7 7v3"/><path d="M12 9a3 3 0 0 1 3 3v6"/><path d="M5 12a7 7 0 0 1 7-7"/><path d="M9 12a3 3 0 0 1 1.5-2.6"/></svg>"#;
    pub const SECURITY_KEY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="3" width="12" height="16" rx="3"/><circle cx="12" cy="14" r="2.5"/><path d="M10 7.5h4"/></svg>"#;
}

/// A private key found on disk, with everything the pane can derive from it.
#[derive(Clone)]
struct KeyEntry {
    path: PathBuf,
    /// `ssh-ed25519`, `ssh-rsa`, … as reported by the authorized_keys line.
    kind: String,
    /// `SHA256:...` from `sshdeck_core::keys::fingerprint`.
    fingerprint: String,
    /// The public half as an `authorized_keys` line, ready to copy.
    authorized: String,
}

/// One recorded `known_hosts` line, with its fingerprint.
#[derive(Clone)]
struct KnownEntry {
    /// The host pattern exactly as recorded (`example.com,10.0.0.1`, `[h]:2222`).
    hosts: String,
    key_type: String,
    /// `None` when the line's key is not an OpenSSH public key we can parse.
    fingerprint: Option<String>,
    /// `@revoked` lines must never be treated as a usable trust anchor.
    revoked: bool,
}

/// A changed host key awaiting the user's replacement decision.
#[derive(Clone)]
struct PendingChange {
    host: String,
    port: u16,
    /// Recorded fingerprint(s); `learn` joins several with ", ".
    old: String,
    /// One recorded fingerprint, the value `trust_changed` verifies against.
    old_expected: String,
    new: String,
    /// Re-parsed when the user confirms, so no `PublicKey` is stored in state.
    key_line: String,
}

/// Which of the two sections is showing.
///
/// Public because `main.rs` drives the left nav, where Keychain and Known Hosts
/// are separate entries: it can deep-link the pane with [`KeysPane::show`]
/// instead of relying on the in-pane switch.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeysSection {
    Keys,
    Hosts,
}

/// How the entries are laid out. Termius defaults to the card grid.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Grid,
    List,
}

/// The outcome of a user-initiated trust attempt.
enum LearnOutcome {
    AlreadyKnown,
    Added,
    Changed {
        old: String,
        old_expected: String,
        new: String,
    },
    Failed(String),
}

/// SSH key and known-hosts management. Constructed by the root view.
pub struct KeysPane {
    section: KeysSection,
    view: ViewMode,
    keys: Vec<KeyEntry>,
    /// Fingerprint of the selected key; a fingerprint is stable across reloads,
    /// an index is not.
    selected: Option<String>,
    known: Vec<KnownEntry>,
    filter: Entity<InputState>,
    host_input: Entity<InputState>,
    port_input: Entity<InputState>,
    key_input: Entity<InputState>,
    import_input: Entity<InputState>,
    /// The import-path row is only drawn while the user asked for it.
    show_import: bool,
    /// The trust form is only drawn while the user asked for it.
    show_trust: bool,
    /// A failed `learn` that needs an explicit replacement decision.
    pending: Option<PendingChange>,
    /// True while a background load or key operation is in flight.
    busy: bool,
    /// Keeps the filter re-rendering as it is typed; dropped with the pane.
    _subscriptions: Vec<Subscription>,
}

impl KeysPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search keys and hosts"));
        let host_input = cx.new(|cx| InputState::new(window, cx).placeholder("hostname or IP"));
        let port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("port")
                .default_value("22")
        });
        let key_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("ssh-ed25519 AAAA… host key to trust")
        });
        let import_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("path to an OpenSSH private key"));

        // Re-render as the query changes; the filter is applied in `render`, so
        // no filtered copy is kept in state.
        let subscriptions = vec![cx.subscribe_in(&filter, window, |_, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];

        let mut pane = Self {
            section: KeysSection::Keys,
            view: ViewMode::Grid,
            keys: Vec::new(),
            selected: None,
            known: Vec::new(),
            filter,
            host_input,
            port_input,
            key_input,
            import_input,
            show_import: false,
            show_trust: false,
            pending: None,
            busy: true,
            _subscriptions: subscriptions,
        };
        pane.reload(window, cx);
        pane
    }

    /// Shows one of the two sections. Wiring hook for the left nav.
    pub fn show(&mut self, section: KeysSection, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            cx.notify();
        }
    }

    /// Number of keys currently listed. Reader method for the wiring pass.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// Number of known-host entries currently listed.
    pub fn known_host_count(&self) -> usize {
        self.known.len()
    }

    /// Whether a changed host key is waiting for the user's decision.
    pub fn has_pending_change(&self) -> bool {
        self.pending.is_some()
    }

    /// Reloads keys and known hosts off the UI thread.
    fn reload(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let (keys, known) = cx
                .background_executor()
                .spawn(async move { (load_keys(), load_known_hosts()) })
                .await;
            this.update_in(cx, |pane, _window, cx| {
                pane.keys = keys;
                pane.known = known;
                pane.busy = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn selected_key(&self) -> Option<&KeyEntry> {
        let selected = self.selected.as_deref()?;
        self.keys.iter().find(|key| key.fingerprint == selected)
    }

    /// The lowercased search text; empty means "everything".
    fn query(&self, cx: &App) -> String {
        self.filter.read(cx).value().trim().to_lowercase()
    }

    /// Generates a real key with `sshdeck_core::keys` and writes it 0600.
    fn generate(&mut self, kind: KeyKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        let dir = keys_dir();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { generate_into(&dir, kind) })
                .await;
            this.update_in(cx, |pane, window, cx| {
                pane.busy = false;
                match result {
                    Ok(entry) => {
                        pane.selected = Some(entry.fingerprint.clone());
                        pane.keys.push(entry);
                        pane.keys.sort_by(|a, b| a.path.cmp(&b.path));
                        pane.show_import = false;
                        window.push_notification(
                            Notification::success("Generated a new SSH key"),
                            cx,
                        );
                    }
                    Err(message) => window.push_notification(Notification::error(message), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Imports an existing private key by copying and normalising it.
    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let source = self.import_input.read(cx).value().trim().to_string();
        if source.is_empty() {
            window.push_notification(
                Notification::warning("Enter the path of an OpenSSH private key to import"),
                cx,
            );
            return;
        }
        self.busy = true;
        cx.notify();
        let dir = keys_dir();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { import_into(&dir, &source) })
                .await;
            this.update_in(cx, |pane, window, cx| {
                pane.busy = false;
                match result {
                    Ok(entry) => {
                        pane.selected = Some(entry.fingerprint.clone());
                        pane.keys.push(entry);
                        pane.keys.sort_by(|a, b| a.path.cmp(&b.path));
                        pane.show_import = false;
                        window.push_notification(Notification::success("Imported the key"), cx);
                    }
                    Err(message) => window.push_notification(Notification::error(message), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Trusts a key the user typed: `known` classifies, `learn` writes.
    fn trust_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let host = self.host_input.read(cx).value().trim().to_string();
        let key_line = self.key_input.read(cx).value().trim().to_string();
        // Bound to a local so no borrow of the input outlives the match arm
        // that reports a bad port.
        let port_text = self.port_input.read(cx).value().trim().to_string();
        let port = match port_text.as_str() {
            "" => DEFAULT_PORT,
            value => match value.parse::<u16>() {
                Ok(port) if port > 0 => port,
                _ => {
                    window.push_notification(
                        Notification::warning("Enter a port between 1 and 65535"),
                        cx,
                    );
                    return;
                }
            },
        };
        if host.is_empty() || key_line.is_empty() {
            window.push_notification(
                Notification::warning("A host and a public key line are both required"),
                cx,
            );
            return;
        }
        if public_key_fingerprint(&key_line).is_none() {
            window.push_notification(
                Notification::warning("That is not a valid OpenSSH public key line"),
                cx,
            );
            return;
        }
        let Some(path) = known_hosts::default_path() else {
            window.push_notification(Notification::error("No HOME, so no known_hosts path"), cx);
            return;
        };

        self.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let task_host = host.clone();
            let task_key = key_line.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { trust_or_learn(&task_host, port, &task_key, &path) })
                .await;
            this.update_in(cx, |pane, window, cx| {
                pane.busy = false;
                match outcome {
                    LearnOutcome::AlreadyKnown => window.push_notification(
                        Notification::info(format!("{host}:{port} is already trusted")),
                        cx,
                    ),
                    LearnOutcome::Added => {
                        window.push_notification(
                            Notification::success(format!(
                                "Trusted a new host key for {host}:{port}"
                            )),
                            cx,
                        );
                        pane.key_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        pane.show_trust = false;
                        pane.reload(window, cx);
                    }
                    LearnOutcome::Changed {
                        old,
                        old_expected,
                        new,
                    } => {
                        // Nothing was written: `learn` refuses on a conflict.
                        pane.pending = Some(PendingChange {
                            host: host.clone(),
                            port,
                            old,
                            old_expected,
                            new,
                            key_line: key_line.clone(),
                        });
                    }
                    LearnOutcome::Failed(message) => {
                        window.push_notification(Notification::error(message), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Replaces a changed key after the user has seen both fingerprints.
    fn confirm_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let Some(path) = known_hosts::default_path() else {
            window.push_notification(Notification::error("No HOME, so no known_hosts path"), cx);
            return;
        };
        self.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let task = pending.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    replace_public_line(
                        &task.host,
                        task.port,
                        &task.old_expected,
                        &task.key_line,
                        &path,
                    )
                })
                .await;
            this.update_in(cx, |pane, window, cx| {
                pane.busy = false;
                match result {
                    Ok(()) => {
                        pane.pending = None;
                        window.push_notification(
                            Notification::success("Replaced the recorded host key"),
                            cx,
                        );
                        pane.reload(window, cx);
                    }
                    Err(message) => window.push_notification(Notification::error(message), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── chrome ────────────────────────────────────────────────────────────

    /// The single toolbar row: section switch, contextual actions, icon controls.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(56.))
            .px_3()
            .flex_shrink_0()
            .bg(cx.theme().popover)
            .border_b_1()
            .border_color(border)
            .child(self.render_section_switch(cx))
            .child(self.render_actions(cx))
            .child(div().flex_1())
            .child(self.render_controls(cx))
    }

    /// The Keychain / Known Hosts switch, drawn as the reference's two marks.
    fn render_section_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().flex_row().items_center().gap_1();
        for (id, mark, label, section) in [
            ("switch-keys", glyph::KEY, "Keychain", KeysSection::Keys),
            (
                "switch-hosts",
                glyph::FINGERPRINT,
                "Known hosts",
                KeysSection::Hosts,
            ),
        ] {
            row = row.child(
                Button::new(id)
                    .ghost()
                    .icon(Icon::default().data(mark))
                    .tooltip(label)
                    .selected(self.section == section)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    })),
            );
        }
        row
    }

    /// The section's own actions, mirroring the reference's left cluster.
    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().flex_row().items_center().gap_1();
        match self.section {
            KeysSection::Keys => {
                row = row
                    .child(
                        Button::new("generate-ed25519")
                            .icon(IconName::Plus)
                            .label("New key")
                            .tooltip("Generate an ed25519 key")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.generate(KeyKind::Ed25519, window, cx);
                            })),
                    )
                    .child(
                        Button::new("generate-rsa")
                            .ghost()
                            .label("RSA key")
                            .tooltip("Generate an RSA key")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.generate(KeyKind::Rsa, window, cx);
                            })),
                    )
                    .child(
                        Button::new("import-key")
                            .ghost()
                            .label("Import")
                            .tooltip("Import an OpenSSH private key by path")
                            .selected(self.show_import)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_import = !this.show_import;
                                cx.notify();
                            })),
                    )
                    // The reference surfaces these three. The core has no x509,
                    // passkey or FIDO2 store, so they are shown disabled rather
                    // than wired to nothing (same rule as `logs_pane`).
                    .child(
                        Button::new("keys-certificates")
                            .ghost()
                            .icon(IconName::FileText)
                            .label("Certificate")
                            .tooltip("Certificates are not supported yet")
                            .disabled(true),
                    )
                    .child(
                        Button::new("keys-touch-id")
                            .ghost()
                            .icon(Icon::default().data(glyph::FINGERPRINT))
                            .label("Touch ID")
                            .tooltip("Passkeys over Touch ID are not supported yet")
                            .disabled(true),
                    )
                    .child(
                        Button::new("keys-fido2")
                            .ghost()
                            .icon(Icon::default().data(glyph::SECURITY_KEY))
                            .label("FIDO2")
                            .tooltip("FIDO2 security keys are not supported yet")
                            .disabled(true),
                    );
            }
            KeysSection::Hosts => {
                row = row
                    .child(
                        Button::new("import-known-hosts")
                            .ghost()
                            .label("Import")
                            .tooltip("Import known_hosts file")
                            .selected(self.show_import)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_import = !this.show_import;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("trust-host-key-form")
                            .icon(IconName::Plus)
                            .label("Trust a host key")
                            .tooltip("Check a public key against known_hosts and record it")
                            .selected(self.show_trust)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_trust = !this.show_trust;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("known-hosts-refresh")
                            .ghost()
                            .icon(IconName::RotateCw)
                            .label("Reload")
                            .tooltip("Re-read known_hosts")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    );
            }
        }
        row
    }

    /// Search, layout and reload controls, right-aligned as in the reference.
    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .child(
                Button::new("keys-search")
                    .ghost()
                    .icon(IconName::Search)
                    .tooltip("Search")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let handle = this.filter.read(cx).focus_handle(cx);
                        handle.focus(window, cx);
                    })),
            )
            .child(
                Button::new("keys-view-grid")
                    .ghost()
                    .icon(Icon::default().data(glyph::GRID))
                    .tooltip("Grid view")
                    .selected(self.view == ViewMode::Grid)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = ViewMode::Grid;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("keys-view-list")
                    .ghost()
                    .icon(Icon::default().data(glyph::LIST))
                    .tooltip("List view")
                    .selected(self.view == ViewMode::List)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = ViewMode::List;
                        cx.notify();
                    })),
            )
            .child(div().w(px(1.)).h(px(20.)).bg(border))
            .child(
                Button::new("keys-refresh")
                    .ghost()
                    .icon(IconName::RotateCw)
                    .tooltip("Reload keys and known hosts")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
    }

    /// The filter field, pinned at the top of the content area as the reference
    /// does with its search bar on the Hosts screen.
    fn render_filter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .flex_shrink_0()
            .px_6()
            .pt_6()
            .pb_4()
            .child(
                Input::new(&self.filter).small().cleanable(true).prefix(
                    Icon::new(IconName::Search)
                        .small()
                        .text_color(cx.theme().muted_foreground),
                ),
            )
    }

    // ── keys section ──────────────────────────────────────────────────────

    fn render_keys(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let query = self.query(cx);
        let visible: Vec<&KeyEntry> = self
            .keys
            .iter()
            .filter(|key| {
                let name = key_name(key);
                matches_query(
                    &[name.as_str(), key.kind.as_str(), key.fingerprint.as_str()],
                    &query,
                )
            })
            .collect();

        let tiles: Vec<AnyElement> = visible
            .iter()
            .map(|key| {
                let fingerprint = key.fingerprint.clone();
                let selected = self.selected.as_deref() == Some(fingerprint.as_str());
                let title = key_name(key);
                let meta = format!(
                    "{} · {}",
                    key_type_label(&key.kind),
                    short_fingerprint(&fingerprint)
                );
                let id = SharedString::from(format!("key-{fingerprint}"));
                let tile = match self.view {
                    ViewMode::Grid => card(id, glyph::KEY, title, meta, selected, false, cx),
                    ViewMode::List => list_row(id, glyph::KEY, title, meta, selected, false, cx),
                };
                tile.on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(fingerprint.clone());
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();

        let mut body = div().flex().flex_col().gap_4().w_full();
        if self.show_import {
            body = body.child(self.render_import(cx));
        }
        body = body.child(section_heading("Keys"));
        if visible.is_empty() {
            body = body.child(empty_state(
                glyph::KEY,
                if self.keys.is_empty() {
                    "No private keys yet"
                } else {
                    "No key matches the search"
                },
                if self.keys.is_empty() {
                    "Private keys in ~/.ssh and the sshdeck keys directory appear here. Generate one, or import an existing OpenSSH key."
                } else {
                    "Clear the search to see every key again."
                },
                cx,
            ));
        } else {
            body = body.child(self.layout(tiles));
        }
        if let Some(detail) = self.render_key_detail(cx) {
            body = body.child(detail);
        }
        if !visible.is_empty() && !self.keys.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child("Generated and imported keys are written owner-only (0600)."),
            );
        }

        scroll_body("keys-scroll").child(body).into_any_element()
    }

    /// The import row, drawn only while the user asked for it.
    fn render_import(&self, cx: &mut Context<Self>) -> AnyElement {
        panel(cx)
            .flex()
            .flex_col()
            .gap_3()
            .child(eyebrow("IMPORT AN OPENSSH PRIVATE KEY", cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.import_input).small()))
                    .child(
                        Button::new("import-key-run")
                            .icon(IconName::Inbox)
                            .label("Import")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.import(window, cx))),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("A copy is normalised into the sshdeck keys directory; the original file is left untouched."),
            )
            .into_any_element()
    }

    fn render_key_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entry = self.selected_key()?;
        let fingerprint = entry.fingerprint.clone();
        let authorized = entry.authorized.clone();
        let meta = format!("{} · {}", entry.kind, entry.path.display());

        let copy_key = authorized.clone();
        Some(
            panel(cx)
                .flex()
                .flex_col()
                .gap_3()
                .child(eyebrow("PUBLIC KEY (authorized_keys)", cx))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .font_family("Menlo")
                                .text_size(px(13.))
                                .child(SharedString::from(authorized)),
                        )
                        .child(
                            Button::new("copy-authorized")
                                .ghost()
                                .icon(IconName::Copy)
                                .tooltip("Copy public key")
                                .on_click(cx.listener(move |_, _, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_key.clone(),
                                    ));
                                    window.push_notification(
                                        Notification::success("Public key copied"),
                                        cx,
                                    );
                                })),
                        ),
                )
                .child(eyebrow("SHA256 FINGERPRINT", cx))
                .child(
                    div()
                        .font_family("Menlo")
                        .text_size(px(13.))
                        .child(SharedString::from(fingerprint)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(meta)),
                )
                .into_any_element(),
        )
    }

    // ── known-hosts section ───────────────────────────────────────────────

    fn render_known_hosts(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.query(cx);
        let visible: Vec<&KnownEntry> = self
            .known
            .iter()
            .filter(|entry| {
                matches_query(
                    &[
                        entry.hosts.as_str(),
                        entry.key_type.as_str(),
                        entry.fingerprint.as_deref().unwrap_or(""),
                    ],
                    &query,
                )
            })
            .collect();

        let tiles: Vec<AnyElement> = visible
            .iter()
            .map(|entry| {
                let changed = self.pending.as_ref().is_some_and(|pending| {
                    hosts_pattern_matches(&entry.hosts, &pending.host, pending.port)
                });
                let alarm = entry.revoked || changed;
                let shown = entry
                    .fingerprint
                    .as_deref()
                    .map(short_fingerprint)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "fingerprint unavailable".to_string());
                let title = entry.hosts.clone();
                let meta = format!("{} · {}", entry.key_type, shown);
                let id = SharedString::from(format!("known-{}-{title}", entry.key_type));
                let row = match self.view {
                    ViewMode::Grid => card(id, glyph::FINGERPRINT, title, meta, false, alarm, cx),
                    ViewMode::List => {
                        list_row(id, glyph::FINGERPRINT, title, meta, false, alarm, cx)
                    }
                };
                match (entry.revoked, changed) {
                    (true, _) => row.child(state_pill("REVOKED", cx.theme().danger, cx)),
                    (false, true) => row.child(state_pill("CHANGED", cx.theme().danger, cx)),
                    (false, false) => row,
                }
                .into_any_element()
            })
            .collect();

        let mut body = div().flex().flex_col().gap_4().w_full();
        if let Some(pending) = self.pending.clone() {
            body = body.child(self.render_pending(&pending, cx));
        }
        if self.show_trust {
            body = body.child(self.render_trust_form(cx));
        }
        body = body.child(section_heading("Known Hosts"));
        if visible.is_empty() {
            body = body.child(empty_state(
                glyph::FINGERPRINT,
                if self.known.is_empty() {
                    "No known hosts yet"
                } else {
                    "No host matches the search"
                },
                if self.known.is_empty() {
                    "A host key is recorded here only after you check it and trust it."
                } else {
                    "Clear the search to see every recorded host again."
                },
                cx,
            ));
        } else {
            body = body.child(self.layout(tiles));
        }

        scroll_body("known-hosts-scroll")
            .child(body)
            .into_any_element()
    }

    /// The changed-key warning: both fingerprints, then a danger button.
    ///
    /// This is the man-in-the-middle signal, so it is deliberately loud — a
    /// danger border, the two fingerprints side by side, and a second explicit
    /// click before `trust_changed` writes anything.
    fn render_pending(&self, pending: &PendingChange, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let border = cx.theme().border;
        let popover = cx.theme().popover;
        let foreground = cx.theme().foreground;
        let message = format!(
            "The host key recorded for {}:{} is not the one now offered. Only replace it if \
             you have verified the new key out of band — an unexpected change can be a \
             man-in-the-middle attack.",
            pending.host, pending.port
        );
        let fingerprint_box = move |label: &'static str, value: &str, alarm: bool| {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .flex_1()
                .min_w(px(0.))
                .p_3()
                .rounded(px(6.))
                .border_1()
                .border_color(if alarm { danger } else { border })
                .bg(popover)
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if alarm { danger } else { muted })
                        .child(label),
                )
                .child(
                    div()
                        .font_family("Menlo")
                        .text_size(px(13.))
                        .overflow_hidden()
                        .text_color(if alarm { danger } else { foreground })
                        .child(SharedString::from(value.to_string())),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded(px(10.))
            .border_2()
            .border_color(danger)
            .bg(popover)
            .shadow_sm()
            .child(
                Alert::error("changed-host-key", message)
                    .title("Host key changed — possible man-in-the-middle"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_stretch()
                    .gap_3()
                    .child(fingerprint_box("RECORDED (OLD)", &pending.old, false))
                    .child(fingerprint_box("OFFERED (NEW)", &pending.new, true)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("replace-host-key")
                            .danger()
                            .icon(IconName::TriangleAlert)
                            .label("Replace recorded key")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_change(window, cx);
                            })),
                    )
                    .child(
                        Button::new("cancel-host-key")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pending = None;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .child("The recorded key is replaced only by this click."),
                    ),
            )
            .into_any_element()
    }

    /// Trusts a key the user types: `known` classifies, `learn` writes.
    fn render_trust_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        panel(cx)
            .flex()
            .flex_col()
            .gap_3()
            .child(eyebrow("TRUST A HOST KEY", cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.host_input).small()))
                    .child(div().w(px(90.)).child(Input::new(&self.port_input).small())),
            )
            .child(Input::new(&self.key_input).small())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("trust-host-key-run")
                            .icon(IconName::Check)
                            .label("Check & trust")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trust_key(window, cx);
                            })),
                    )
                    .child(div().text_size(px(12.)).text_color(muted).child(
                        "Nothing is written until this click, and a changed key needs a second confirmation.",
                    )),
            )
            .into_any_element()
    }

    /// Grid or list, per the toolbar's layout control.
    fn layout(&self, tiles: Vec<AnyElement>) -> AnyElement {
        match self.view {
            ViewMode::Grid => div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_4()
                .w_full()
                .children(tiles)
                .into_any_element(),
            ViewMode::List => div()
                .flex()
                .flex_col()
                .w_full()
                .children(tiles)
                .into_any_element(),
        }
    }
}

impl Render for KeysPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .text_size(px(14.))
            // The grey content surface the reference frames its white cards with.
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(self.render_filter(cx))
            .child(match self.section {
                KeysSection::Keys => self.render_keys(cx),
                KeysSection::Hosts => self.render_known_hosts(cx),
            })
    }
}

// ── view helpers ──────────────────────────────────────────────────────────

/// A vertically scrolling content column with the reference's 24px inset.
fn scroll_body(id: &'static str) -> Scrollable<Stateful<Div>> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_4()
        .flex_1()
        .min_h(px(0.))
        .px_6()
        .pb_6()
        .overflow_y_scrollbar()
}

/// A white card surface: `10px` radius, subtle shadow, no default border.
fn panel(cx: &App) -> Div {
    div()
        .p_4()
        .rounded(px(10.))
        .bg(cx.theme().popover)
        .shadow_xs()
}

/// The navy square Termius sets behind a key, identity or host glyph.
fn glyph_tile(glyph: &'static [u8], size: f32, cx: &App) -> impl IntoElement {
    let inner = size * 0.6;
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .size(px(size))
        .rounded(px(size * 0.25))
        .bg(rgba(TILE_BG))
        .child(
            Icon::default()
                .data(glyph)
                .w(px(inner))
                .h(px(inner))
                .text_color(cx.theme().primary_foreground),
        )
}

/// One Termius card: navy tile, name, and a secondary line.
///
/// Returns the element so the caller can attach its own click handler and any
/// trailing badge without this helper knowing what the entry means.
fn card(
    id: SharedString,
    glyph: &'static [u8],
    title: String,
    meta: String,
    selected: bool,
    danger: bool,
    cx: &App,
) -> Stateful<Div> {
    let muted = cx.theme().muted_foreground;
    let accent = cx.theme().accent;
    let primary = cx.theme().primary;
    let danger_color = cx.theme().danger;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .p_2p5()
        .h(px(60.))
        .flex_basis(px(CARD_BASIS))
        .flex_grow_1()
        .flex_shrink_0()
        .min_w(px(220.))
        .rounded(px(10.))
        .bg(cx.theme().popover)
        .shadow_xs()
        .cursor_pointer()
        .when(danger, |el| el.border_1().border_color(danger_color))
        .when(selected, |el| el.border_1().border_color(primary))
        .hover(move |el| el.bg(accent))
        .child(glyph_tile(glyph, 40., cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .child(
                    div()
                        .text_size(px(14.))
                        .truncate()
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted)
                        .truncate()
                        .child(SharedString::from(meta)),
                ),
        )
}

/// One row of the list layout: 44px, hairline separator, hover fill.
fn list_row(
    id: SharedString,
    glyph: &'static [u8],
    title: String,
    meta: String,
    selected: bool,
    danger: bool,
    cx: &App,
) -> Stateful<Div> {
    let muted = cx.theme().muted_foreground;
    let border = cx.theme().border;
    let accent = cx.theme().accent;
    let selected_bg = cx.theme().muted;
    let danger_color = cx.theme().danger;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .min_h(px(44.))
        .px_3()
        .w_full()
        .cursor_pointer()
        .border_b_1()
        .border_color(border)
        .when(selected, |el| el.bg(selected_bg))
        .when(danger, |el| el.border_l_2().border_color(danger_color))
        .hover(move |el| el.bg(accent))
        .child(glyph_tile(glyph, 28., cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .child(
                    div()
                        .text_size(px(14.))
                        .truncate()
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted)
                        .truncate()
                        .child(SharedString::from(meta)),
                ),
        )
}

/// A solid state pill (`REVOKED`, `CHANGED`).
fn state_pill(label: &'static str, color: Hsla, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded(px(6.))
        .bg(color)
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().danger_foreground)
        .child(label)
}

/// A section heading, as the reference's bold "Keys" / "Known Hosts".
fn section_heading(label: &'static str) -> Div {
    div()
        .flex_shrink_0()
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label)
}

/// A small uppercase caption above a value, as the reference's field labels.
fn eyebrow(label: &'static str, cx: &App) -> Div {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// A centred empty state, matching the port-forwarding and logs panes.
fn empty_state(glyph: &'static [u8], title: &'static str, body: &'static str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    let foreground = cx.theme().foreground;
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .w_full()
        .py_16()
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
                        .data(glyph)
                        .w(px(32.))
                        .h(px(32.))
                        .text_color(foreground),
                ),
        )
        .child(div().text_size(px(20.)).text_color(foreground).child(title))
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(px(14.))
                .text_color(muted)
                .child(body),
        )
}

// ── filesystem helpers ────────────────────────────────────────────────────

/// The directory the app owns for keys it generates or imports.
fn keys_dir() -> PathBuf {
    HostStore::default_path()
        .parent()
        .map(|dir| dir.join("keys"))
        .unwrap_or_else(|| PathBuf::from("sshdeck-keys"))
}

/// `~/.ssh` plus the app's own keys directory.
fn key_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".ssh"));
    }
    dirs.push(keys_dir());
    dirs
}

/// Files that are never a private key, so skip them without a parse attempt.
fn is_ignored_key_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    if name.starts_with('.') {
        return true;
    }
    if matches!(
        name,
        "config"
            | "known_hosts"
            | "known_hosts.old"
            | "authorized_keys"
            | "authorized_keys2"
            | "environment"
            | "rc"
    ) {
        return true;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(ext) if ext.eq_ignore_ascii_case("pub") || ext.eq_ignore_ascii_case("old")
    )
}

/// Scans the search directories and parses every readable private key.
fn load_keys() -> Vec<KeyEntry> {
    let mut entries: Vec<KeyEntry> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for dir in key_search_dirs() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read_dir.flatten() {
            let path = item.path();
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_KEY_FILE_BYTES {
                continue;
            }
            if is_ignored_key_file(&path) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // An unparseable file (encrypted key, public key, random blob) is
            // simply not a key we can show. Skipping is not a failure.
            let Ok(key) = keys::from_openssh_private(&text) else {
                continue;
            };
            let Ok(authorized) = keys::authorized_key(&key) else {
                continue;
            };
            let fingerprint = keys::fingerprint(key.public_key());
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen.insert(canonical) {
                continue;
            }
            let kind = authorized
                .split_whitespace()
                .next()
                .unwrap_or("ssh-ed25519")
                .to_string();
            entries.push(KeyEntry {
                path,
                kind,
                fingerprint,
                authorized,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Parses the recorded `known_hosts` file into display entries.
fn load_known_hosts() -> Vec<KnownEntry> {
    let Some(path) = known_hosts::default_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_known_hosts_line).collect()
}

/// Parses one known_hosts line. Returns `None` for comments and blanks.
///
/// Format (OpenSSH): `[markers] hostnames keytype base64 [comment]`. Markers
/// such as `@revoked` and `@cert-authority` are recognised so a revoked anchor
/// is never presented as a usable one.
fn parse_known_hosts_line(line: &str) -> Option<KnownEntry> {
    let mut rest = line.trim();
    if rest.is_empty() || rest.starts_with('#') {
        return None;
    }
    let mut revoked = false;
    loop {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let token = parts.next().unwrap_or("");
        let Some(name) = token.strip_prefix('@') else {
            break;
        };
        if name == "revoked" {
            revoked = true;
        }
        rest = parts.next().unwrap_or("").trim_start();
        if rest.is_empty() {
            return None;
        }
    }
    let mut fields = rest.split_whitespace();
    let hosts = fields.next()?.to_string();
    let key_type = fields.next()?.to_string();
    let base64 = fields.next()?.to_string();
    let comment: Vec<&str> = fields.collect();
    let key_line = if comment.is_empty() {
        format!("{key_type} {base64}")
    } else {
        format!("{key_type} {base64} {}", comment.join(" "))
    };
    Some(KnownEntry {
        hosts,
        key_type,
        fingerprint: public_key_fingerprint(&key_line),
        revoked,
    })
}

/// SHA256 fingerprint of an `authorized_keys`-style public key line.
///
/// The parsed key's concrete type is inferred from `known_hosts::fingerprint`,
/// so the app never needs to name `russh`'s `PublicKey`. The fingerprint itself
/// always comes from the crate; none is computed here.
fn public_key_fingerprint(key_line: &str) -> Option<String> {
    let key = key_line.parse().ok()?;
    Some(known_hosts::fingerprint(&key))
}

/// Shortens a fingerprint for a list row without losing the ends that make it
/// distinguishable from a neighbour.
fn short_fingerprint(fingerprint: &str) -> String {
    let Some(rest) = fingerprint.strip_prefix("SHA256:") else {
        return fingerprint.to_string();
    };
    if rest.len() <= SHORT_FINGERPRINT_EDGE * 2 + 1 {
        return fingerprint.to_string();
    }
    format!(
        "SHA256:{}…{}",
        &rest[..SHORT_FINGERPRINT_EDGE],
        &rest[rest.len() - SHORT_FINGERPRINT_EDGE..]
    )
}

/// The name a key is shown under: its file name, as Termius shows `docker.pem`.
fn key_name(key: &KeyEntry) -> String {
    key.path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("key")
        .to_string()
}

/// The label Termius prints under a key's name: `Type RSA`, `Type ED25519`.
fn key_type_label(kind: &str) -> String {
    let short = match kind {
        "ssh-ed25519" => "ED25519",
        "ssh-rsa" | "rsa-sha2-256" | "rsa-sha2-512" => "RSA",
        "ecdsa-sha2-nistp256" | "ecdsa-sha2-nistp384" | "ecdsa-sha2-nistp521" => "ECDSA",
        "sk-ssh-ed25519@openssh.com" | "sk-ecdsa-sha2-nistp256@openssh.com" => "ED25519-SK",
        "ssh-dss" => "DSA",
        other => other,
    };
    format!("Type {short}")
}

/// Case-insensitive substring test over the fields a row shows.
fn matches_query(fields: &[&str], query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || fields
            .iter()
            .any(|field| field.to_lowercase().contains(query.as_str()))
}

/// Whether a `known_hosts` host pattern covers `host:port`.
///
/// The file stores either `host` or `[host]:port`, comma-separated when several
/// names share a key; used to mark the row a pending change belongs to.
fn hosts_pattern_matches(patterns: &str, host: &str, port: u16) -> bool {
    let bracketed = format!("[{host}]:{port}");
    patterns.split(',').any(|pattern| {
        pattern.eq_ignore_ascii_case(host) || pattern.eq_ignore_ascii_case(bracketed.as_str())
    })
}

/// A filename safe to write into the keys directory.
fn safe_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty()
        || cleaned == "."
        || cleaned == ".."
        || cleaned.trim_matches('.').is_empty()
    {
        "imported_key".to_string()
    } else {
        cleaned
    }
}

/// A path under `dir` that does not exist yet.
fn unique_path(dir: &Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}_sshdeck"));
    if !first.exists() {
        return first;
    }
    (1u32..10_000)
        .map(|n| dir.join(format!("{stem}_sshdeck_{n}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| dir.join(format!("{stem}_sshdeck_new")))
}

/// Generates a key and writes it. Runs on the background executor.
fn generate_into(dir: &Path, kind: KeyKind) -> Result<KeyEntry, String> {
    let key = keys::generate(kind).map_err(|err| format!("could not generate key: {err}"))?;
    let pem =
        keys::to_openssh_private(&key).map_err(|err| format!("could not encode key: {err}"))?;
    let authorized =
        keys::authorized_key(&key).map_err(|err| format!("could not read public key: {err}"))?;
    let fingerprint = keys::fingerprint(key.public_key());
    let stem = match kind {
        KeyKind::Ed25519 => "id_ed25519",
        KeyKind::Rsa => "id_rsa",
    };
    let path = unique_path(dir, stem);
    keys::save_private_key(&path, &pem).map_err(|err| format!("could not save key: {err}"))?;
    let kind_label = authorized
        .split_whitespace()
        .next()
        .unwrap_or("ssh-ed25519")
        .to_string();
    Ok(KeyEntry {
        path,
        kind: kind_label,
        fingerprint,
        authorized,
    })
}

/// Validates and copies an existing private key. Runs on the background executor.
fn import_into(dir: &Path, source: &str) -> Result<KeyEntry, String> {
    let text =
        std::fs::read_to_string(source).map_err(|err| format!("could not read {source}: {err}"))?;
    let key = keys::from_openssh_private(&text)
        .map_err(|err| format!("not an OpenSSH private key: {err}"))?;
    // Save a normalised copy; the original file is left untouched.
    let pem =
        keys::to_openssh_private(&key).map_err(|err| format!("could not encode key: {err}"))?;
    let authorized =
        keys::authorized_key(&key).map_err(|err| format!("could not read public key: {err}"))?;
    let fingerprint = keys::fingerprint(key.public_key());
    let stem = Path::new(source)
        .file_name()
        .and_then(|name| name.to_str())
        .map(safe_component)
        .unwrap_or_else(|| "imported_key".to_string());
    let path = unique_path(dir, &stem);
    keys::save_private_key(&path, &pem).map_err(|err| format!("could not save key: {err}"))?;
    let kind = authorized
        .split_whitespace()
        .next()
        .unwrap_or("ssh-ed25519")
        .to_string();
    Ok(KeyEntry {
        path,
        kind,
        fingerprint,
        authorized,
    })
}

/// `known` classifies, then `learn` performs the only write. On a conflict the
/// structured `KeyChanged` is surfaced and nothing is written.
///
/// The parsed key's concrete type is inferred from the crate's `&PublicKey`
/// parameters, so the app never names `russh`'s `PublicKey`.
fn trust_or_learn(host: &str, port: u16, key_line: &str, path: &Path) -> LearnOutcome {
    let key = match key_line.parse() {
        Ok(key) => key,
        Err(err) => return LearnOutcome::Failed(format!("not a valid OpenSSH public key: {err}")),
    };
    // Read-only classification first: an already-recorded key needs no write.
    if matches!(known_hosts::known(host, port, &key, path), Ok(true)) {
        return LearnOutcome::AlreadyKnown;
    }
    // Unknown, or a changed key. `learn` is the only writer; on a conflict it
    // returns `KeyChanged` and writes nothing.
    match known_hosts::learn(host, port, &key, path) {
        Ok(Learned::Added) => LearnOutcome::Added,
        Ok(Learned::AlreadyKnown) => LearnOutcome::AlreadyKnown,
        Err(KnownHostsError::KeyChanged { old, new, .. }) => {
            let old_expected = old.split(", ").next().unwrap_or(old.as_str()).to_string();
            LearnOutcome::Changed {
                old,
                old_expected,
                new,
            }
        }
        Err(err) => LearnOutcome::Failed(err.to_string()),
    }
}

/// Replaces a recorded key after the user confirms the exact old fingerprint.
fn replace_public_line(
    host: &str,
    port: u16,
    expected_old: &str,
    key_line: &str,
    path: &Path,
) -> Result<(), String> {
    let key = match key_line.parse() {
        Ok(key) => key,
        Err(err) => return Err(format!("not a valid OpenSSH public key: {err}")),
    };
    known_hosts::trust_changed(host, port, expected_old, &key, path).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // An ed25519 public key, lifted from `sshdeck_core::known_hosts` tests.
    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";

    #[test]
    fn public_key_fingerprints_come_from_the_crate() {
        let fingerprint =
            public_key_fingerprint(&format!("ssh-ed25519 {KEY}")).expect("a valid key");
        assert!(fingerprint.starts_with("SHA256:"), "got {fingerprint}");
        assert!(public_key_fingerprint("definitely not a key").is_none());
        assert!(public_key_fingerprint("").is_none());
    }

    #[test]
    fn known_hosts_lines_parse_and_flag_revoked_entries() {
        let entry = parse_known_hosts_line(&format!(
            "example.com,10.0.0.1 ssh-ed25519 {KEY} user@example.com"
        ))
        .expect("a key line");
        assert_eq!(entry.hosts, "example.com,10.0.0.1");
        assert_eq!(entry.key_type, "ssh-ed25519");
        assert!(!entry.revoked);
        assert!(entry
            .fingerprint
            .expect("fingerprint")
            .starts_with("SHA256:"));

        assert!(parse_known_hosts_line("# a comment").is_none());
        assert!(parse_known_hosts_line("   ").is_none());

        let revoked =
            parse_known_hosts_line(&format!("@revoked host ssh-ed25519 {KEY}")).expect("revoked");
        assert!(revoked.revoked);
        assert_eq!(revoked.hosts, "host");
    }

    #[test]
    fn shortening_a_fingerprint_keeps_both_ends_readable() {
        let fingerprint = "SHA256:0123456789abcdefghijklmnopqrstuvwxyz";
        let short = short_fingerprint(fingerprint);
        assert!(short.starts_with("SHA256:01234567"));
        assert!(short.ends_with("wxyz"));
        assert!(short.contains('…'));
        assert_ne!(short, fingerprint);

        // Short input is left alone rather than mangled.
        assert_eq!(short_fingerprint("SHA256:short"), "SHA256:short");
        assert_eq!(short_fingerprint("MD5:aa:bb"), "MD5:aa:bb");
    }

    #[test]
    fn imported_file_names_are_sanitised() {
        assert_eq!(safe_component("id_ed25519"), "id_ed25519");
        assert_eq!(safe_component("my key"), "my_key");
        assert_eq!(safe_component(".."), "imported_key");
        assert_eq!(safe_component(""), "imported_key");
    }

    #[test]
    fn key_types_are_labelled_the_way_the_reference_does() {
        assert_eq!(key_type_label("ssh-ed25519"), "Type ED25519");
        assert_eq!(key_type_label("ssh-rsa"), "Type RSA");
        assert_eq!(key_type_label("rsa-sha2-512"), "Type RSA");
        assert_eq!(key_type_label("ecdsa-sha2-nistp256"), "Type ECDSA");
        // An unknown type is shown as recorded rather than silently relabelled.
        assert_eq!(key_type_label("ssh-future"), "Type ssh-future");
    }

    #[test]
    fn the_filter_matches_case_insensitively_across_its_fields() {
        assert!(matches_query(&["docker.pem", "ssh-rsa"], ""));
        assert!(matches_query(&["docker.pem", "ssh-rsa"], "DOCKER"));
        assert!(matches_query(&["docker.pem", "ssh-rsa"], "rsa"));
        assert!(!matches_query(&["docker.pem", "ssh-rsa"], "ed25519"));
    }

    #[test]
    fn a_pending_change_marks_only_the_pattern_it_belongs_to() {
        assert!(hosts_pattern_matches("example.com", "example.com", 22));
        assert!(hosts_pattern_matches(
            "a.example.com,example.com",
            "example.com",
            22
        ));
        assert!(hosts_pattern_matches(
            "[example.com]:2222",
            "example.com",
            2222
        ));
        assert!(hosts_pattern_matches(
            "[EXAMPLE.com]:2222",
            "example.com",
            2222
        ));
        // Same host, different port, is a different entry.
        assert!(!hosts_pattern_matches(
            "[example.com]:2222",
            "example.com",
            22
        ));
        assert!(!hosts_pattern_matches(
            "other.example.com",
            "example.com",
            22
        ));
    }
}
