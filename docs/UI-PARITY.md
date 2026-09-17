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
| `radius` / `radius.lg` | small / medium | 5 / 10 | 5 / 10 |
| `font.size` | UI base | 13 | 13 |
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
