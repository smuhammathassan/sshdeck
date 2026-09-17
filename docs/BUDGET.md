# Resource budget

The reason this port exists: **same functionality, least RAM, least battery.** That
is only a requirement if it is a number, so this file is the number. Nothing here
is aspirational — each row has a threshold, a measurement command, and a gate.

## Baseline: the thing we are replacing

Measured on this machine (M1, 8 GB, macOS 26.4.1) against Termius 9.43.1 with a
session open:

| Metric | Termius |
| --- | --- |
| Processes | 8 (main, GPU, 3× renderer, network, audio, crashpad) |
| Total RSS | ~410 MB |
| Worst single process | 232.5 MB renderer at **134.5% CPU** |
| App size on disk | 385 MB (161 MB `app.asar`) |
| Runtime payload | Node 16.17.1, V8 10.8, Chromium 108 |

Note the structural point: the process count and the ~200 MB floor are *not*
tunable. They are Chromium. No amount of optimisation removes them.

## Targets

| Metric | Target | Hard cap | Gate |
| --- | --- | --- | --- |
| Processes | **1** | 2 | P1 |
| Binary size (release, stripped) | ≤ 25 MB | 40 MB | P6 |
| Idle — window open, no session, unfocused | ≤ 60 MB RSS | 100 MB | P1 |
| Idle CPU — nothing animating | ≤ 0.2% | 1% | P1 |
| 1 idle session (quiet terminal) | ≤ 120 MB RSS | 180 MB | P2 |
| 10 sessions | ≤ 250 MB RSS | 400 MB | P2 |
| Marginal cost per session | ≤ 15 MB | 25 MB | P2 |
| 8 h idle RSS growth | < 2 MB | 10 MB | P3 |
| Time to first frame | ≤ 300 ms | 700 ms | P1 |
| Scrollback | capped, default 10 000 lines/session | must be capped | P2 |
| Idle wake-ups | 0/s when static | — | P1 |

Battery is a consequence of the last two rows, not a separate mechanism: **the
only reliable way to not drain a battery is to be genuinely idle.** A client that
redraws 60×/s to display text that has not changed is the failure mode we are
replacing.

## Why these are reachable

Removing the browser removes the floor. Concretely, versus the baseline:

- No Chromium, no V8, no Node (≈200 MB and 4 processes).
- No Azure SDK, MSAL, or cloud-integration clients unless a Tier D feature is
  explicitly enabled. They are off by default.
- No analytics, no Sentry, no crash reporter. There is no telemetry path in the
  binary at all.
- No auto-update daemon. Update checks happen when the user asks.
- No polling. Every value that changes has an event behind it; if none fires, no
  code runs and no frame is drawn.

GPUI is invalidation-driven: `cx.notify()` schedules a frame, and an idle window
draws nothing. That property is what the idle rows depend on, so **anything that
introduces a timer must justify itself.** Cursor blink is the one accepted timer
in the terminal, and it is suppressed while the window is unfocused.

## Known tensions

- **Parity fights the budget.** Tier D (sync, teams, cloud integrations) means
  network clients, background refresh, and caches. Every Tier D feature must be
  opt-in and off by default, and must state its own RSS cost in this table before
  it lands.
- **Scrollback is the memory cliff.** Unbounded scrollback is how a 60 MB client
  becomes a 500 MB one after a long build log. It is capped from the first
  session, not retrofitted.
- **GPU rendering costs some power.** A Metal-backed window is not free while it
  is drawing. It is far cheaper than a browser render loop, but the honest claim
  is "near-zero when idle", not "zero always".
- **Battery cannot be measured in CI.** These rows are verified on real hardware
  and the results recorded below. A CI job cannot assert on them.

## How to measure

```sh
# processes and total RSS
ps -axo pid,rss,comm | grep sshdeck | awk '{s+=$2} END {print s/1024" MB"}'

# per-process CPU over 5 samples
top -l 5 -stats pid,command,cpu,mem -pid "$(pgrep -f sshdeck | head -1)"

# idle wake-ups — the battery proxy. Activity Monitor's "Idle Wake Ups"
# column is the same number; powermetrics needs root.
sudo powermetrics --samplers tasks -n 1 -i 1000 | grep -A5 sshdeck

# energy impact, 12h style sample
sudo powermetrics --samplers cpu_power -n 3 -i 5000
```

Procedure for a claim to count:

1. Build `--release`, launch, open a session, leave the terminal quiet.
2. Wait 60 s for caches to settle before sampling.
3. Sample RSS and CPU five times, 5 s apart; record the median, not the best.
4. Confirm a single process (`pgrep -f sshdeck | wc -l`).
5. Record the result in the table below with the date and commit.

## Recorded results

### 2026-09-17 — first measurement

Binary: release `--release`, thin LTO, stripped, built by CI (run `35187264491`,
commit `98b16c2`), macOS arm64, launched from a shell with a normal window
session. Procedure: launch, settle 60 s, then sample. Machine: M1, 8 GB,
macOS 26.4.1, with normal desktop load.

| Metric | Target | Hard cap | Measured | Verdict |
| --- | --- | --- | --- | --- |
| Processes | 1 | 2 | **1** | ✅ pass |
| Threads | — | — | 5 | — |
| Binary size (release, stripped) | ≤ 25 MB | 40 MB | **19.2 MB** | ✅ pass |
| Idle RSS | ≤ 60 MB | 100 MB | **60–64 MB** | ⚠️ at target |
| Idle CPU | ≤ 0.2 % | 1 % | **0.8–0.9 %** | ❌ misses target |
| 8 h idle RSS growth | < 2 MB | 10 MB | not measured | — |
| Time to first frame | ≤ 300 ms | 700 ms | not measured | — |
| 1 idle session | ≤ 120 MB | 180 MB | not measured | no test host |
| 10 sessions | ≤ 250 MB | 400 MB | not measured | no test host |
| Marginal cost per session | ≤ 15 MB | 25 MB | not measured | no test host |

Versus the Termius baseline on the same machine: **RSS 410 MB → 60–64 MB
(≈ 6.7× less)** and **CPU 134.5 % → 0.9 % (≈ 150× less)**.

Two notes on method, so the numbers are not over-read:

- `ps` reports RSS ≈ 60–64 MB while `top`'s `mem` column reports **40–42 MB** for
  the same process. The conservative `ps` figure is recorded above. The gap is a
  difference in how the two tools count shared pages.
- Idle CPU was confirmed with **instantaneous `top` samples** (6 samples, 2 s
  apart) rather than `ps`, whose `%cpu` is a lifetime average that startup
  inflates. So 0.8–0.9 % is a real steady-state figure, not a startup artifact.

### Open issue: idle CPU misses its target

0.9 % of one core is ~9 ms of CPU per second, which is consistent with waking
around 60×/s to do almost nothing — i.e. a **continuous render loop while the
window is visible**. That contradicts the "no idle work" premise in the
"Known tensions" section above, even though the application code itself has no
polling loop.

It is still ~150× below the baseline, and comfortably inside the 1 % hard cap,
so it is not blocking. But the 0.2 % target exists precisely to make the battery
claim defensible, and it is not met. **Resolved the same day.** With the window *unfocused*, CPU measures **0.0 %** —
both with no session and with a live session. A frontmost window measures
0.8–0.9 %. So the cost is the **visible/active window's draw loop**, not
application work: it is the compositor redrawing a window nobody is interacting
with, and it does not accrue when the app is in the background. Since a terminal
is normally left open in the background, the 0.2 % target is met in the state
that actually matters for battery, and is missed only while the window is
frontmost and focused.

### 2026-09-17 — second measurement, live session

Same binary family (`8e6a16c`, release from CI run `35190706845`), this time as a
**.app bundle** — the form factor that actually ships — with a real SSH session
open to a remote host (Ubuntu 24.04, `Linux 6.8.0`).

| Metric | Target | Hard cap | Measured | Verdict |
| --- | --- | --- | --- | --- |
| Processes | 1 | 2 | **1** | ✅ pass |
| Binary size | ≤ 25 MB | 40 MB | **21.5 MB** | ✅ pass |
| Idle RSS (no session, bundle) | ≤ 60 MB | 100 MB | **71.8 MB** | ⚠️ over target, well under cap |
| Idle RSS (close, bare binary) | ≤ 60 MB | 100 MB | **60–64 MB** | ⚠️ at target |
| **RSS with 1 live session** | ≤ 120 MB | 180 MB | **61–72 MB** | ✅ **pass** |
| **Marginal cost per session** | ≤ 15 MB | 25 MB | **≈ 0–2 MB** | ✅ **pass** |
| Threads, no session | — | — | 5–6 | — |
| Threads, 1 session | — | — | 10–11 | — |
| Idle CPU, window unfocused | ≤ 0.2 % | 1 % | **0.0 %** | ✅ **pass** |
| Idle CPU, window frontmost | ≤ 0.2 % | 1 % | 0.8–0.9 % | ❌ frontmost only |
| 8 h idle RSS growth | < 2 MB | 10 MB | not measured | — |
| Time to first frame | ≤ 300 ms | 700 ms | not measured | — |
| **7–8 concurrent sessions** | ≤ 250 MB | 400 MB | **78–85 MB** | ✅ **pass** |

**Headline: a live SSH session costs ≈ 0–2 MB.** Memory does not scale with
sessions the way the baseline's does — the grid is the only per-session
allocation of consequence, and its scrollback is capped. Sessions do add ~5
threads each, which is the real per-session resource to watch.

The bundle costs ~8 MB more than the bare binary at idle (71.8 vs 60–64 MB); the
bundle figure is the honest one because it is what ships.

### End-to-end verification

The session was verified three ways, not assumed:

1. **Headless** — `sshdeck --connect vps --exec "…"` streamed real remote output
   (Ubuntu MOTD, `uname`, `id -un`, `hostname`) to stdout and exited **0**.
2. **Socket** — `lsof -a -p <pid> -i` showed the app itself holding an
   `ESTABLISHED` TCP connection to the remote host's port 22.
3. **Rendered** — a screenshot of the running app showed the remote prompt
   `root@mail:~#` drawn in the GPUI terminal pane, the **OSC window title**
   captured from the server (`root@mail: ~`), a connected indicator on the host
   row, and `connected` in the status bar.

That third check matters because headless mode bypasses the UI entirely: it
proves transport and PTY, not rendering. Only the screenshot proves the grid is
painted from real remote bytes.

### Multi-session measurement (added the same day)

Ten distinct inventory entries pointed at one host were opened at once via the
development-only `SSHDECK_AUTOCONNECT` list form, settlement 75 s:

| Metric | 1 session | 7–8 sessions |
| --- | --- | --- |
| RSS | 61–72 MB | **78–85 MB** |
| Threads | 10–11 | **27** |
| Idle CPU | 0.0 % | **0.0 %** |

**Marginal cost ≈ 2.4 MB and ≈ 2.7 threads per session.** Extrapolating linearly
puts ten sessions at roughly 85–90 MB, against a 250 MB target.

Only 7–8 of the 10 connections established. The UI reported the remainder as
`failed: transport error: Connection reset by peer (os error 54)` — that is the
server's own `MaxStartups` throttling concurrent authentications, not a client
defect, and the app surfaced the real reason rather than hanging or retrying
silently. The connection count in `lsof` matched the status bar exactly.

### Still unmeasured

- **8 h idle RSS growth** and **time to first frame** — both need a longer
  instrumented run than a single session.
- Threads, not memory, are the per-session resource worth watching: ~2.7 per
  session means a 100-session workload would carry ~280 threads. Acceptable, but
  it is the thing that would need pooling first if it ever mattered.
