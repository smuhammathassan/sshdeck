# sshdeck — agent rules

Rust + GPUI desktop SSH client. Reimplementation of the Termius feature set on
open protocols. macOS-first (M1, 8 GB RAM), cross-platform later.

## Ponytail: lazy senior dev mode

You are a lazy senior developer. Lazy means efficient, not careless. The best
code is the code never written.

Before writing any code, stop at the first rung that holds:

1. Does this need to be built at all? (YAGNI)
2. Does it already exist in this codebase? Reuse it, don't rewrite it.
3. Does the standard library already do this? Use it.
4. Does a native platform feature cover it? Use it.
5. Does an already-installed dependency solve it? Use it.
6. Can this be one line? Make it one line.
7. Only then: write the minimum code that works.

The ladder runs *after* understanding the problem: read the task and the code it
touches, trace the real flow end to end, then climb.

Rules:

- No abstractions that weren't requested. No new dependency if it can be avoided.
- Deletion over addition. Boring over clever. Fewest files possible.
- Shortest working diff wins, but only once you understand the problem.
- Question complex requests: "Do you actually need X, or does Y cover it?"
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling
  and the upgrade path.
- Not lazy about: input validation at trust boundaries, error handling that
  prevents data loss, security, accessibility, anything explicitly requested.
- Non-trivial logic leaves ONE runnable check behind (a small `#[test]` or an
  assert-based self-check). No frameworks, no fixtures.

## Layout

```
crates/sshdeck-core/   domain model, vault, SSH/SFTP client. No UI. No gpui.
crates/sshdeck-app/    gpui-kit binary `sshdeck`. Owns presentation only.
docs/re/               reverse-engineering catalog of the Termius feature set.
.ai/gpui-kit/          pinned gpui-kit skill + references. Read before writing UI.
```

Dependency direction is one-way: `sshdeck-app -> sshdeck-core`. Core never
depends on the UI, and never imports `gpui`.

## GPUI / gpui-kit rules

- **Never invent an API.** The real signatures are in `.ai/gpui-kit/`. Read the
  relevant reference before writing a component; fetch
  `https://gpui-kit.com/component/{name}.md` if it is not on disk.
- Applications depend on `gpui-kit` alone. `use gpui_kit::*;` is GPUI.
- Boot order is fixed: `gpui_kit::init(cx)` first, then `Root::new(view, ...)`
  as the first child of every window, then render the dialog/sheet/notification
  layers from the app view.
- `Root::render_dialog_layer(window, cx)` — the layer helpers take `window`.
- Stateful components own an `Entity<State>`; keep `Subscription`s on the view
  that owns the state, never on a constructor-local.
- Theme colors come from `cx.theme()`. No hardcoded colors in application code.
- Repeated elements need domain-derived `ElementId`s, never list indexes.

### Known gpui-kit 0.6.1 errata

The published docs and the pinned skill references in `.ai/gpui-kit/` are wrong on
the points below. Verified by reading the resolved sources
(`gpui-component 0.6.1`, `gpui-base 0.6.1`, `gpui-pre 0.3.5`). Trust the source,
not the prose.

| Documented as | Reality in 0.6.1 |
| --- | --- |
| `cx.theme().surface` | No such field — `Theme` derefs to `ThemeColor`, which has no `surface`. Use `cx.theme().sidebar` for panel/sidebar backgrounds. |
| Accent colours listed as flat fields | Not all documented names exist; check `theme/theme_color.rs` before using one. |
| `cx.theme().destructive` (usage.md "Theming") | No such field. `ThemeColor` has `danger` / `danger_foreground` instead; verified in `theme/theme_color.rs` (gpui-component 0.6.1). |
| `gpui_kit::component::ButtonVariants` | Not re-exported at the component root. Import from `gpui_kit::component::button::{Button, ButtonVariants as _}`. |
| `Styled::overflow_y_scroll` | Removed. Use `ScrollableElement::overflow_y_scrollbar`. |
| `Theme::toggle_mode(cx)` | Does not exist. Use `Theme::change(mode, Some(window), cx)`. |
| `push_notification` on `Context` | Notifications are window-scoped: `window.push_notification(Notification::error(..), cx)`. |
| `cx.defer(..)` for a toast | `defer` yields only `&mut App`, no window. Use `cx.defer_in(window, ..)`. |
| `cx.spawn(..)` for a task that later shows a notification | `spawn` yields only `&mut AsyncApp`, which has no window. Use `cx.spawn_in(window, ..)` and `update_in`, which yield `&mut AsyncWindowContext` (verified in `context.rs`: `spawn` takes `AsyncApp`, `spawn_in` takes `AsyncWindowContext`). |
| Size methods need no import | `.small()`/`.large()` come from the `Sizable` trait; `.disabled()` from `Disableable`; `.when()` from `FluentBuilder` (in `prelude`); `.overflow_y_scrollbar()` from `ScrollableElement`; `.focus_handle(cx)` from `Focusable` (it takes `&App`). Missing these traits is the single most common cause of "no method named" errors here. |
| Colour guides treat `Rgba`/`Rgb`/`Hsla` as interchangeable | `Rgba` and `Hsla` are distinct structs and there is no `Rgb` type. Theme fields (`cx.theme().foreground`, `.background`, …) are `Hsla`; parsed terminal cell colours are `Rgba`. An `Option<Rgba>` cannot fall back to an `Hsla` in `unwrap_or` — convert with `.into()` (`From<Hsla> for Rgba` and `From<Rgba> for Hsla` both exist). Verified in `gpui-0.2.2/src/color.rs` (the version `gpui-base` resolves to): `Rgba` has public fields, `Copy`, a custom `Debug`, and a `blend` method but **no `opacity` method** — only `Hsla` has `opacity`, so set `Rgba { a: .., ..color }` instead. |
| `Tab::new("tab1")` (Tabs example) | `Tab::new()` takes no argument. Set the label with `Tab::label(..)` (or a child) and drive the selection with `TabBar::selected_index(..)` plus `TabBar::on_click(|index, ..| ..)`. |
| `Notification::new("Upload complete").info()` | `Notification::new()` takes no argument, and `info()` is not an instance method. The kind comes from the associated constructors `Notification::info("…")` / `::success(..)` / `::warning(..)` / `::error(..)`; `.message(..)` only sets the body. |
| `Sizable` exposes `.medium()` (usage.md and settings.md both list it) | There is no `medium()` method. `sizing.rs` implements only `with_size`, `xsmall`, `small`, `large`; `Size::Medium` is the default. Use `.with_size(Size::Medium)` (import `Size` from `gpui_kit::component`). |
| settings.md's "Complete Settings Example" imports `Settings` / `SettingPage` / `SettingGroup` / `SettingItem` / `SettingField` from `gpui_kit::component::{…}` | `gpui-component` declares `pub mod setting;` but never globs it at the crate root, so `gpui_kit::component::Settings` does not resolve. Import them from `gpui_kit::component::setting::{…}` (the same page's Import section is the correct one). |
| Interactive div methods all come from `StatefulInteractiveElement` | Only `on_click` / `hover` / `cursor_pointer` do. `on_double_click` is provided by `InteractiveElementExt` (gpui-base), so a `Stateful<Div>` needs **both** traits in scope (`StatefulInteractiveElement as _` from `prelude` plus `InteractiveElementExt as _` from `gpui_kit::component`). |
| The `IconName` catalog in `icons.md` lists the available icons | The documented list is aspirational and includes names with no variant. `IconName::Server` does not exist in 0.6.1; use a variant already proven in compiled code (`Globe`, `Folder`, `File`, `Check`, `Close`, …) or read the icon enum in the source. |
| Mouse/wheel handlers (`on_mouse_down`/`on_mouse_up`/`on_mouse_move`/`on_scroll_wheel`) need `Stateful`/`id()` | They are provided by gpui-pre's `InteractiveElement`, which `Div` implements directly. A `Div` gets a hitbox whenever `should_insert_hitbox` is true — any `track_focus`, listener, or cursor counts — so a plain `div().track_focus(..)` handles them with only `InteractiveElement as _` in scope. Do **not** add a fixed `.id(..)` just for this: several instances of the same view in one window then share one `ElementId`. Listeners take `impl Fn(&Event, &mut Window, &mut App)`; `Context::listener` supplies that shape with `(this, &event, window, cx)`. |
| theme.md's only theme-registration route is `ThemeRegistry::watch_dir(PathBuf, ..)` with an `on_load` callback | `ThemeRegistry::load_themes_from_str(&str)` is public and parses a `ThemeSet` from memory (`theme/registry.rs`), so a bundled app can `include_str!` its theme and never depend on the working directory. Applying that `Rc<ThemeConfig>` via `Theme::global_mut(cx).apply_config(..)` sets the fonts/colors but does **not** project to the Base layer (scrollbars, resize handles) or refresh windows; follow it with `Theme::change(mode, ..)` (or `Theme::sync_base`). |
| A rounded frameless window can be requested through `WindowOptions` | No corner-radius field exists. `WindowOptions { titlebar: Some(TitlebarOptions { appears_transparent: true, traffic_light_position: None, title: None }), ..WindowOptions::default() }` hides the system title bar and draws content under it while keeping the native traffic lights (the closable/minimizable/resizable style masks are only set when `titlebar` is `Some`), so the close button works. There is no titlebar-height option, so the lights keep their position in the native ~28px titlebar band and cannot be centred on a taller custom header; reserve a left inset instead. The native macOS corner radius stands. Verified in `gpui-0.2.2/src/platform.rs` (the version `gpui-base` resolves to). |

`gpui-kit = "0.6"` resolves to `gpui-pre 0.3.5` — read that version's source.

### Known core errata

`docs/ROADMAP.md` §"Protocol compatibility target" says to support both host
chains and `ProxyJump` but does not say how the two share a field. The source
(verified against `Host` in `lib.rs` and `sshdeck_core::jump`):

| Documented / assumed | Reality in the source |
| --- | --- |
| `Host::proxy_jump` is a jump-host label | It is **either** an inventory label/address (native chain) **or** a verbatim `ProxyJump` spec from `sshdeck-import`. Resolution prefers an inventory match (label, then address), then parses a spec; `none` means no jump. |
| `ProxyCommand` is honour-able | `Host` has **no** `ProxyCommand` field. `sshdeck-import` reports and drops it, and `sshdeck_core::jump` refuses a command-like `proxy_jump` value (whitespace or `%`) with `ChainError::ProxyCommandSkipped` rather than shell-interpolating it. |
| Jump depth is unbounded | Chains are capped at `sshdeck_core::jump::MAX_JUMP_DEPTH` (4) jumps; cycles are rejected by `HostId`. Dialling verifies each hop's key against `known_hosts` separately. |

## Rust rules

- Edition 2021. `cargo fmt` clean. `cargo clippy` clean (warnings are failures
  in review, not in CI yet).
- No `unwrap()`/`expect()` outside `main`, tests, and invariant-proving spots
  where the comment explains why it cannot fail.
- Public data types use private fields plus reader methods.
- Errors: `thiserror` in libraries, typed enums over `anyhow` at this stage.
- Every non-trivial logic path ships one small `#[test]`.

## Resource discipline (8 GB machine)

- `.cargo/config.toml` caps `jobs = 4`. Do not raise it.
- `[profile.dev] debug = 0` — keep the target dir small. Use `--release` for
  performance testing.
- Prefer `cargo check` while iterating; run `cargo build` when a binary is
  actually needed.

## Subagents

Delegate to `deepseek4.1` only. No other subagent, for any task.

## Legality

Clean-room reimplementation. Standard protocols (SSH, SFTP, mosh, FIDO2) and
public documentation are fair game. Never copy Termius source, assets, icons,
fonts, or branding into this repo. Reverse-engineered notes in `docs/re/`
describe *behaviour and interfaces*, which is what we reimplement.
