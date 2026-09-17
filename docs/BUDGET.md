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

| Date | Commit | Build | Idle RSS | Idle CPU | 1 session | Processes |
| --- | --- | --- | --- | --- | --- | --- |
| — | — | — | not yet measured | — | — | — |

Empty on purpose. The first row lands when P1 produces a real session; until
then there is nothing honest to put here.
