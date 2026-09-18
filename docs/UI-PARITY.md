# UI parity spec

Visual language matched to Termius 9.43.1, implemented with GPUI + gpui-kit.

The colour values below were recovered from the application's own stylesheet
(`ui-process/assets/main-*.css`, `reconnectSaga-*.css`), so they are exact
rather than eyeballed. Everything here is *tokens and layout*; no third-party
artwork, icons, fonts, or source code is copied. See "Substitutions" below.

## Source tokens

Termius ships a two-dimensional token system: a neutral ramp (`dark-grey-N` /
`light-grey-N`) plus semantic aliases (`--c-title`, `--border-basic`,
`--button-accent`, …) that resolve differently per mode. We take the ramp
verbatim and map it onto gpui-kit semantic tokens.

### Neutral ramp

| Token | Dark | Light |
| --- | --- | --- |
| grey-1 | `#141729` | `#798c94` |
| grey-2 | `#1d2033` | `#8fa1a8` |
| grey-3 | `#282b3d` | `#a4b3ba` |
| grey-4 | `#32364a` | `#d5dde0` |
| grey-5 | `#3e4257` | `#e6ebed` |
| grey-6 | `#5a5e73` | `#edf1f2` |
| grey-7 | `#8d91a5` | `#f7f9fa` |

Dark reads the ramp in reverse: grey-1 is the darkest surface, grey-7 the
lightest text. Light mode reuses the same names with the opposite meaning.

### Accents

| Name | Value | | Name | Value |
| --- | --- | --- | --- | --- |
| blue | `#2091f6` | | red | `#f25e61` |
| blue-dark | `#186cb5` | | red-dark | `#f24e50` |
| blue-light | `#58adf8` | | red-light | `#ff7375` |
| blueberry | `#6666d2` | | green | `#21b568` |
| purple | `#af5fff` | | green-medium | `#00cc74` |
| vivid-purple | `#a042ff` | | green-light | `#00d67a` |
| teal | `#54d2d2` | | lime-green | `#81d254` |
| raspberry | `#d2549a` | | yellow | `#f2c94c` |
| creme | `#e9c899` | | bright-yellow | `#ffcb00` |
| orange | `#f8aa4b` | | dark-blue | `#182542` |

### Geometry and type

| Token | Value |
| --- | --- |
| corner-radius-extra-small | 4px |
| corner-radius-small | 5px |
| corner-radius-small-increased | 6px |
| corner-radius-small-medium | 8px |
| corner-radius-medium | 10px |
| corner-radius-large | 15px |
| corner-radius-large-increased | 20px |
| corner-radius-extra-large | 25px |
| corner-radius-full | 1000px |
| app-window-border-radius | 10px (0 on Linux/Windows/full-screen) |
| active-terminal-width | 240px |

Built-in themes shipped by Termius: `light`, `dark`, `midnight`. Only the first
two have their values recovered; `midnight` is pending and must not be invented.

### Host OS brand colours

Termius ships a brand colour per detected OS, used for host icons in the list.
25 values are present and worth carrying over as-is (they are third-party brand
colours, used nominatively to indicate the platform):

`ubuntu #e95420` · `debian #ce0056` · `arch #1793d1` · `fedora #3c6eb4` ·
`centos #efa720` · `redhat #ee0000` · `rockylinux #34d399` · `suse #30ba78` ·
`mageia #2397d4` · `gentoo #54487a` · `alpine` — · `freebsd #f60006` ·
`openbsd #f2ca30` · `netbsd #f26711` · `routeros #164aaa` · `linux #ffcc33` ·
`macos #49a3f2` · `windows #00a1f1` · `android #3ddc84` · `apple #171719` ·
`aws #ff9900` · `digitalocean #0080ff` · `cisco #00bceb` · `pi`/`raspbian #be3956` ·
`gloria #16b8f0`

## Mapping onto gpui-kit

Shipped as data in `crates/sshdeck-app/themes/sshdeck.json` (a gpui-kit
`ThemeSet`), with two entries: `sshdeck Dark` and `sshdeck Light`.

| gpui-kit token | Termius source | Dark | Light |
| --- | --- | --- | --- |
| `background` | grey-1 / grey-7 | `#141729` | `#f7f9fa` |
| `foreground` | inverse of background | `#f7f9fa` | `#141729` |
| `muted.foreground` | grey-7 / grey-1 | `#8d91a5` | `#798c94` |
| `muted.background` | grey-3 / grey-5 | `#282b3d` | `#e6ebed` |
| `border` | grey-4 | `#32364a` | `#d5dde0` |
| `primary.background` | blue | `#2091f6` | `#2091f6` |
| `sidebar.background` | grey-2 / grey-6 | `#1d2033` | `#edf1f2` |
| `tab_bar.background` | grey-1 / grey-6 | `#141729` | `#edf1f2` |
| `tab.active.background` | grey-3 / grey-7 | `#282b3d` | `#f7f9fa` |
| `tab.foreground` | grey-7 / grey-1 | `#8d91a5` | `#798c94` |
| `list.active.border` | blue | `#2091f6` | `#2091f6` |
| `radius` / `radius.lg` | small-increased / medium | 6 / 10 | 6 / 10 |
| `font.size` | UI base | 14 | 14 |
| `mono_font.size` | terminal base | 13 | 13 |

`shadow: false` — Termius surfaces are flat; borders carry the separation.

### Wiring (not yet done)

The theme file exists but is not registered. Two candidate mechanisms, neither
verified yet — do not guess:

1. `ThemeRegistry::watch_dir(path, cx, cb)` then
   `Theme::global_mut(cx).apply_config(&theme)` on the selected entry, per
   `https://gpui-kit.com/component/theme.md`.
2. Parsing the embedded `include_str!` JSON into a `ThemeConfig` and applying it
   directly, which avoids shipping a theme directory next to the binary.

Option 2 is preferable for a bundled app; confirm the public API on docs.rs
before writing it.

## Layout

Dimensions marked **(t)** are token-backed and exact; **(u)** are unconfirmed
estimates taken from the layout's known structure and must be checked against a
real window before we claim parity.

```
┌──────────────────────────────────────────────────────────────┐
│ tab bar: tab icons + host tabs + "+"        update, bell    │  h 38 (u)
├───────────────┬──────────────────────────────────────────────┤
│ sidebar       │ terminal surface                             │
│  search       │                                              │
│  group/tag    │  (mono grid, per-terminal palette)           │
│  host rows    │                                              │
│  w 240 (t)    │                                              │
│               ├──────────────────────────────────────────────┤
│               │ status bar: endpoint · encryption · state     │  h 24 (u)
└───────────────┴──────────────────────────────────────────────┘
```

- Window: `app-window-border-radius: 10px`, so the window is frameless with
  rounded corners on macOS. Needs a custom titlebar and a transparent window;
  the tab bar is the drag region.
- Terminal background is **not** the app background: Termius themes the terminal
  separately (per-terminal palette), with `#141729`-family default in dark.
- Host rows carry an OS brand colour, the label, and a muted `user@host:port`.
- Row actions (edit, duplicate, delete) appear on hover.

## Chrome metrics (exact, from the app's own stylesheet)

Recovered from `ui-process/assets/main-*.css` `:root` — these are the real
values, not estimates:

| Variable | Value | Meaning |
| --- | --- | --- |
| `--header-height` | **56px** | top bar |
| `--horizontal-tabs-height` | **51px** | tab row |
| `--default-font-size` | **14px** | body text |
| `--app-window-border-radius` | **10px** (0 on Linux/Windows/full-screen) | window corner |
| `--scrollbar-width` | **8px** | scrollbar |
| `--max-terminal-width` | 180px | collapsed side panel |
| `--active-terminal-width` | 240px | expanded side panel |
| `--default-z-index-overlap` | 1301 | overlay stacking |

Body text is `CircularXX, sans-serif` at 14px with `-webkit-font-smoothing:
antialiased`; context menus are `border-radius: 6px`, `0 6px 10px #0003`,
`font-size: 12px`.

## Semantic tokens (exact mapping)

The stylesheet defines a semantic layer over the greys, switched by a
`.termius-dark-theme` class. This is the mapping to match, dark mode:

| Semantic | Dark value | Hex |
| --- | --- | --- |
| `--text-primary` | `white` | `#ffffff` |
| `--text-secondary` | `dark-grey-7` | `#8d91a5` |
| `--text-disabled` | `dark-grey-6` | `#5a5e73` |
| `--text-accent` | `blue` | `#2091f6` |
| `--surface-lowest` | `dark-grey-1` | `#141729` |
| **`--main-bg`** | **`dark-grey-2`** | **`#1d2033`** |
| `--main-side-bg` | `dark-grey-2` | `#1d2033` |
| `--surface-high` | `dark-grey-3` | `#282b3d` |
| `--surface-highest` | `dark-grey-4` | `#32364a` |
| `--main-form-bg` | `dark-grey-4` | `#32364a` |
| `--background-entity` | `dark-grey-5` | `#3e4257` |
| `--border-strong` | `dark-grey-5` | `#3e4257` |
| `--border-basic` | `dark-grey-7-a25` | `#8d91a540` |
| `--border-light` | `dark-grey-7-a10` | `#8d91a51a` |
| `--border-extra-light` | `dark-grey-7-a05` | `#8d91a50d` |
| `--surface-accent` | `blue-a25` | `#2091f640` |
| `--list-hover` | `dark-grey-5` | `#3e4257` |
| `--list-select` | `dark-grey-4` | `#32364a` |
| `--entity-item-background` | `dark-grey-3` | `#282b3d` |
| `--cf-main-background` | `dark-grey-1` | `#141729` |
| `--border-accent` / `--button-accent` | `blue` | `#2091f6` |

**Correction to the first draft of this document:** the app background is
`--main-bg` = **`#1d2033`**, not `#141729`. The darkest grey is
`--surface-lowest` and `--cf-main-background`, i.e. the command-line/pane
background behind the chrome, not the window itself. The earlier mapping used
`#141729` for `background`, which renders the whole window one step too dark.

## Known chrome differences still to close

Measured against a screenshot of the running app. These are the gaps between our
current shell and the original:

1. **We have three horizontal bands; the original has one.** We draw a custom
   title bar *and* a tab strip *and* a bottom status bar. The original draws a
   single 56px header containing the sidebar toggle, tabs, `+`, and the
   right-aligned actions (update, notifications, account), and **no bottom
   status bar at all**.
2. **Sidebar is not collapsible.** The original collapses to a rail
   (`--max-terminal-width: 180px` collapsed vs `--active-terminal-width: 240px`
   expanded) and puts host *creation* behind a control rather than an
   always-visible form pinned to the bottom.
3. **Window corner radius.** The original is a 10px rounded frameless window.
4. **Host rows.** The original uses `--entity-item-background` (`#282b3d`) cards
   with `--list-hover` / `--list-select` states, not bare rows.
5. **Body text is 14px**, not 13px.

## Header: measured deltas vs the real thing

Comparing our running app against a capture of Termius side by side, the chrome
is now the right *shape* but four things still read as wrong. These are the
remaining gaps, most-visible first.

1. **Our header's right side is visually heavy; theirs is almost empty.**
   Termius has three subtle items: a text button (`Update`), a bell icon, and an
   account control. We render a two-line status block
   (`1 connected` / `root@… · password · connected`), a **solid blue filled
   `Reconnect` button**, and four more icons. A filled primary button in the
   chrome is the single loudest difference. Connection state belongs in the tab
   (a dot) and a tooltip, not as text in the header, and `Reconnect` belongs on
   the session tab's context, not as a primary action.
2. **Termius has a second tab row.** Directly under the 56px header there is a
   ~28px band, spanning only the pane (not the sidebar), holding the focused
   pane's own tabs plus a `+`. We draw the terminal directly under the header.
3. **Tab rendering.** Theirs: a coloured terminal-type glyph, then the label,
   then a close `✕`; the active tab is an elevated rounded rect and inactive
   tabs are fully transparent. Ours uses a connection **dot** where the glyph
   should be, which reads as a different design.
4. **Icons.** Theirs are filled/duotone glyphs at a consistent optical weight;
   ours are thin single-weight line icons, which changes the whole texture of
   the header. Matching the exact glyphs is not required, but the visual weight
   is: prefer solid/filled icons in the chrome and keep one size (16px).

Not fixable, for the record: the 10px window corner radius. GPUI has no
corner-radius field and clips to a rectangular mask, so the native macOS radius
stands. Do not spend time on it again.

## Substitutions

Fidelity where it is free, substitution where the original is licensed:

| Original | Replacement | Why |
| --- | --- | --- |
| Circular XX Web (UI font) | `.SystemUIFont` (SF Pro on macOS) | Circular is a commercial licence we do not hold. Metrics differ slightly; this is the closest system face. |
| Termius icon set | Lucide, via `gpui-kit::assets` | Their icons are their artwork. Lucide is the set gpui-kit already bundles. |
| `<OS> Nerd Font Mono` (terminal) | same fonts, vendored | These are OFL-licensed and redistributable, so the terminal can match exactly. |
| Termius logo / wordmark | none | Branding is not reimplemented. The product name is `sshdeck`. |

Net effect: layout, density, colour, and terminal rendering match; the UI font
and iconography are legally equivalent stand-ins rather than copies.

## Open questions

- `midnight` theme values (not recovered, not guessed).
- Exact tab bar, status bar, and sidebar row heights — need one real screenshot
  to confirm. Termius's Electron content exposes no accessibility tree, so this
  requires capturing the window while it is frontmost.
- Whether `#RRGGBBAA` is interpreted as RGBA or ARGB by the theme loader; the
  light `selection.background` uses an 8-digit value and may need revisiting.
