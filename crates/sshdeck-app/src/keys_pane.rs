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

use std::path::{Path, PathBuf};

use gpui_kit::component::{
    alert::Alert,
    button::{Button, ButtonVariants as _},
    input::{Input, InputState},
    notification::Notification,
    scroll::ScrollableElement as _,
    tab::{Tab, TabBar},
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::{FluentBuilder as _, StatefulInteractiveElement as _};
use gpui_kit::{
    div, px, AnyElement, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Keys,
    Hosts,
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
    section: Section,
    keys: Vec<KeyEntry>,
    /// Fingerprint of the selected key; a fingerprint is stable across reloads,
    /// an index is not.
    selected: Option<String>,
    known: Vec<KnownEntry>,
    host_input: Entity<InputState>,
    port_input: Entity<InputState>,
    key_input: Entity<InputState>,
    import_input: Entity<InputState>,
    /// A failed `learn` that needs an explicit replacement decision.
    pending: Option<PendingChange>,
    /// True while a background load or key operation is in flight.
    busy: bool,
}

impl KeysPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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

        let mut pane = Self {
            section: Section::Keys,
            keys: Vec::new(),
            selected: None,
            known: Vec::new(),
            host_input,
            port_input,
            key_input,
            import_input,
            pending: None,
            busy: true,
        };
        pane.reload(window, cx);
        pane
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

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.section {
            Section::Keys => 0,
            Section::Hosts => 1,
        };
        TabBar::new("keys-pane-tabs")
            .segmented()
            .selected_index(selected)
            .on_click(cx.listener(|this, index, _, cx| {
                this.section = if *index == 1 {
                    Section::Hosts
                } else {
                    Section::Keys
                };
                cx.notify();
            }))
            .child(Tab::new().label("SSH keys"))
            .child(Tab::new().label("Known hosts"))
    }

    fn render_keys(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;

        let rows: Vec<AnyElement> = self
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let fingerprint = key.fingerprint.clone();
                let selected = self.selected.as_deref() == Some(fingerprint.as_str());
                let name = key
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("key")
                    .to_string();
                let meta = format!("{} · {}", key.kind, short_fingerprint(&fingerprint));
                div()
                    .id(SharedString::from(format!("key-row-{index}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |el| el.bg(cx.theme().muted))
                    .child(Icon::new(IconName::HardDrive).small().text_color(muted))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(SharedString::from(name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .font_family("Menlo")
                                    .child(SharedString::from(meta)),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(fingerprint.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        let detail = self.render_key_detail(cx);

        div()
            .id("keys-scroll")
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .overflow_y_scrollbar()
            .child(self.render_key_actions(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(self.keys.is_empty(), |el| {
                        el.child(div().text_color(muted).child(
                            "No private keys found in ~/.ssh or the sshdeck keys directory.",
                        ))
                    })
                    .children(rows),
            )
            .when_some(detail, |el, detail| el.child(detail))
            .into_any_element()
    }

    fn render_key_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("generate-ed25519")
                            .small()
                            .primary()
                            .icon(IconName::Plus)
                            .label("Generate ed25519")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.generate(KeyKind::Ed25519, window, cx);
                            })),
                    )
                    .child(
                        Button::new("generate-rsa")
                            .small()
                            .ghost()
                            .label("Generate RSA")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.generate(KeyKind::Rsa, window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.import_input).small()))
                    .child(
                        Button::new("import-key")
                            .small()
                            .label("Import")
                            .icon(IconName::File)
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.import(window, cx))),
                    ),
            )
            .child(div().text_xs().text_color(muted).child(
                "Generated and imported keys are written owner-only (0600) under the sshdeck config directory.",
            ))
    }

    fn render_key_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entry = self.selected_key()?;
        let fingerprint = entry.fingerprint.clone();
        let authorized = entry.authorized.clone();
        let meta = format!("{} · {}", entry.kind, entry.path.display());
        let muted = cx.theme().muted_foreground;

        let copy_key = authorized.clone();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_sm()
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().sidebar)
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("PUBLIC KEY (authorized_keys)"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .font_family("Menlo")
                                .text_sm()
                                .child(SharedString::from(authorized)),
                        )
                        .child(
                            Button::new("copy-authorized")
                                .xsmall()
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
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("SHA256 FINGERPRINT"),
                )
                .child(
                    div()
                        .font_family("Menlo")
                        .text_sm()
                        .child(SharedString::from(fingerprint)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(meta)),
                )
                .into_any_element(),
        )
    }

    fn render_known_hosts(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;

        let rows: Vec<AnyElement> = self
            .known
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let revoked = entry.revoked;
                let shown = entry
                    .fingerprint
                    .as_deref()
                    .map(short_fingerprint)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "fingerprint unavailable".to_string());
                let meta = format!("{} · {}", entry.key_type, shown);
                div()
                    .id(SharedString::from(format!("known-row-{index}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .when(revoked, |el| el.bg(cx.theme().danger.opacity(0.08)))
                    .child(
                        Icon::new(if revoked {
                            IconName::CircleX
                        } else {
                            IconName::Globe
                        })
                        .small()
                        .text_color(if revoked {
                            cx.theme().danger
                        } else {
                            muted
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_hidden()
                            .child(SharedString::from(entry.hosts.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .font_family("Menlo")
                                    .child(SharedString::from(meta)),
                            ),
                    )
                    .when(revoked, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().danger)
                                .text_color(cx.theme().danger_foreground)
                                .child("REVOKED"),
                        )
                    })
                    .into_any_element()
            })
            .collect();

        let pending_card = self
            .pending
            .clone()
            .map(|pending| self.render_pending(&pending, cx));

        div()
            .id("known-hosts-scroll")
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .overflow_y_scrollbar()
            .when_some(pending_card, |el, card| el.child(card))
            .child(self.render_trust_form(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(self.known.is_empty(), |el| {
                        el.child(
                            div()
                                .text_color(muted)
                                .child("No known hosts recorded yet."),
                        )
                    })
                    .children(rows),
            )
            .into_any_element()
    }

    /// The changed-key warning: old and new fingerprints, then a danger button.
    fn render_pending(&self, pending: &PendingChange, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let message = format!(
            "The host key recorded for {}:{} is not the one now offered. Only replace it if \
             you have verified the new key out of band — an unexpected change can be a \
             man-in-the-middle attack.",
            pending.host, pending.port
        );
        let fingerprint_row = |label: &'static str, value: &str, danger: bool| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(label))
                .child(
                    div()
                        .font_family("Menlo")
                        .text_sm()
                        .text_color(if danger {
                            cx.theme().danger
                        } else {
                            cx.theme().foreground
                        })
                        .child(SharedString::from(value.to_string())),
                )
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().danger)
            .bg(cx.theme().danger.opacity(0.08))
            .child(
                Alert::error("changed-host-key", message)
                    .title("Host key changed — possible man-in-the-middle"),
            )
            .child(fingerprint_row("RECORDED (OLD)", &pending.old, false))
            .child(fingerprint_row("OFFERED (NEW)", &pending.new, true))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("replace-host-key")
                            .small()
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
                            .small()
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pending = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_trust_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(div().text_xs().text_color(muted).child("TRUST A HOST KEY"))
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
                    .gap_2()
                    .child(
                        Button::new("trust-host-key")
                            .small()
                            .primary()
                            .icon(IconName::Check)
                            .label("Check & trust")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.trust_key(window, cx);
                            })),
                    )
                    .child(div().text_xs().text_color(muted).child(
                        "Nothing is written until this click, and a changed key needs a second confirmation.",
                    )),
            )
            .into_any_element()
    }
}

impl Render for KeysPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.section {
            Section::Keys => self.render_keys(cx),
            Section::Hosts => self.render_known_hosts(cx),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(self.render_tabs(cx))
                    .child(
                        Button::new("keys-refresh")
                            .small()
                            .ghost()
                            .icon(IconName::RotateCw)
                            .tooltip("Reload keys and known hosts")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    ),
            )
            .child(body)
    }
}

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
}
