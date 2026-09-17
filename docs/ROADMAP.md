# Roadmap

Derived from the feature catalogue in `docs/re/FEATURES-UI.md` (169 features, 24
domains) and `docs/re/FEATURES-CORE.md`.

## The scope, honestly

Termius is not one product. It is four, and only the first can be "cloned" from
the client:

| Tier | Contains | Cloneable? | Scope |
| --- | --- | --- | --- |
| **A. Local terminal client** | hosts, vaults, SSH, terminal, keys, forwarding, SFTP, snippets, settings | Yes — open protocols | **In** |
| **B. Local security** | SSH Id passkeys, security keys / FIDO2, certificates, known hosts, agent | Yes — CTAP2 / SSH agent are open | **In** |
| **C. Ecosystem data** | shell autocomplete (~600 CLI specs) | Yes — the same upstream `withfig/autocomplete` specs are MIT | **In** |
| **D. Server-backed** | sync & backup, billing & plans, cloud integrations (AWS/DO/Azure/GCP), AI | **No.** Their backend is not in the bundle | Undecided |
| **X. Cut by decision** | **teams & enterprise**, **serial connections** | n/a | **Out** |

Of the 169 catalogued features: **~106 are Tier A/B**, **~5 are Tier C**, **~44
are Tier D** (still undecided), and **14 are cut by decision**.

Cutting teams also makes most of **billing & plans** moot — those plans exist to
sell team seats, so with no backend there is nothing to bill for. Billing is
treated as cut unless something changes.

Serial is dropped for a second reason beyond taste: it is the only subsystem that
needs device-level IOKit access and per-device driver quirks, which is a large
amount of platform-specific code for the smallest feature in the catalogue.

## Phases

### P0 — Foundation ✅ (done)
Workspace, CI/CD, ponytail rules, domain model (hosts, store, session state),
GPUI window shell, Termius-matched theme tokens.

### P1 — First real session (the gate)
The moment it stops being a mockup. Everything else is worthless until this works.

- SSH transport, key auth + agent auth, host key verification against `known_hosts`
- PTY allocation → terminal grid → keystrokes in, bytes out
- Session state wired to the UI (Connecting/Auth/Connected/Failed)
- One real integration test against a containerised `sshd`

### P2 — Usable daily driver
- Tabs + splits, per-terminal palettes, scrollback, copy/paste, search-in-buffer
- Vaults (multiple, encrypted), host groups/tags, credential storage in Keychain
- Password + keyboard-interactive auth, jump hosts / proxy
- Settings surface, keyboard shortcuts, command palette

### P3 — Files and plumbing
- SFTP browser, upload/download, drag-and-drop, transfer queue
- Port forwarding: local, remote, dynamic (SOCKS)
- Agent forwarding, key generation, import/export (OpenSSH, PEM, PuTTY), certificates

### P4 — Security parity
- FIDO2 / security-key-backed SSH keys (`sk-ssh-ed25519`)
- SSH Id: passkey-backed identity via platform authenticator (Tier B)
- Known-host management UI, host key change warnings

### P5 — Breadth
- Snippets, telnet, mosh (exec system `mosh-client`)
- Shell autocomplete from the MIT `withfig/autocomplete` spec set
- OS-branded host icons (25 colours already captured in `docs/UI-PARITY.md`)

### P6 — Distribution
- Code signing + notarization (needs an Apple Developer certificate)
- Auto-update, first-run onboarding, crash reporting (opt-in, or none)

### P7 — Tier D, or not at all
Sync, teams, billing, cloud integrations, AI. Each needs a backend we own.
Recommendation: omit, and instead support **import** from Termius JSON export so
users can migrate. Revisit only if there is a reason to run a service.

## Crate choices per layer

Decisions are provisional until the phase that needs them; each is recorded here
so it is not re-litigated.

| Layer | Pick | Why / alternative |
| --- | --- | --- |
| SSH transport | `russh` 0.63.3 (pin exactly) + `russh-sftp` 3.0.0 | Pure Rust, no C build in CI. Enable the `des` feature for 3des-cbc. Only known gap: umac. Alternative `ssh2` (libssh2) is blocking and simpler to bridge but drags a C + OpenSSL build into CI **and lacks the custom-signature hook**, which would make hardware-backed keys impossible. |
| Executor bridge | one `std::thread` per connection owning a `tokio` multi-thread runtime + bounded `async_channel` both ways | `russh` cannot run on GPUI's smol executor (hard tokio deps). `async_channel` is executor-agnostic and avoids shimming tokio↔smol. Send raw `Bytes`, never `ChannelMsg`. Backpressure is inherent: a full bounded channel stalls russh's read loop, which stops draining the SSH window, which pushes back on the server. Keep `window_size` / `channel_buffer_size` modest. |
| Terminal emulation | `alacritty_terminal` for the grid/parser | Mature, full escape-sequence coverage, scrollback. `vt100` is smaller but would need replacing. |
| PTY | `portable-pty` | Cross-platform, from the wezterm project. |
| Keys | `russh::keys::decode_secret_key` (uses `ssh-key` pinned by russh) | Confirmed present: OpenSSH, PKCS#1 RSA, PKCS#5 legacy PEM, PKCS#8 plain/encrypted, and **PuTTY `.ppk`** — no extra crate needed. |
| Agent | `russh::keys::agent::client::AgentClient` | In-tree, async, already `impl Signer`. Forwarding is native: `Session::agent_forward(...)` sends `auth-agent-req@openssh.com`, inbound arrives at `Handler::server_channel_open_agent_forward`. **Correction:** the `ssh-agent-client` crate named in the first draft does not exist on crates.io; the sync alternative is `ssh-agent-client-rs`, and `ssh-agent-lib` is for *writing* an agent. |
| FIDO2 | `ctap-hid-fido2` | Pure Rust over HID; avoids a `libfido2` C dependency. |
| OS keychain | `keyring` | Wraps Security.framework on macOS, Credential Manager on Windows. |
| Vault crypto | `argon2` + `chacha20poly1305` | Boring, audited primitives. `age` if we want file-level sharing later. |
| Storage | `serde_json` now, `redb` or SQLite when it outgrows | Ponytail: do not add a database before the flat file hurts. |
| Mosh | exec system `mosh-client` | No pure-Rust mosh exists; the protocol needs the mosh client. Detect and degrade gracefully. |
| Telnet | hand-rolled over `tokio`/`std::net` | Telnet is small; a crate would be more code than the parser. |
| Autocomplete | `withfig/autocomplete` specs (MIT) | The same upstream data set the original bundled, under a licence we can use. |

## Protocol compatibility target

From `docs/re/FEATURES-CORE.md`: the original bundles **libssh2 1.11.1 + Botan
3.2.0 + libsodium + libtelnet** inside one N-API addon (1,980 exported
functions). Its algorithm surface is therefore the compatibility bar — an SSH
client that cannot talk to the servers this one can is not at parity:

| Area | Must support |
| --- | --- |
| KEX | curve25519-sha256, ecdh-sha2-nistp{256,384,521}, diffie-hellman-group{14,16} |
| Host keys | ssh-ed25519, rsa-sha2-{256,512}, ecdsa-sha2-nistp*, and all `*-cert-v01` (OpenSSH certificates) |
| Ciphers | aes{128,192,256}-gcm, aes*-ctr, chacha20-poly1305, 3des-cbc (legacy servers) |
| MACs | hmac-sha2-{256,512}, umac-64/128, including `-etm` variants |
| Private keys | OpenSSH, PEM / PKCS#8 / PKCS#5 / DER, **PuTTY `.ppk`**, `sk-ssh-ed25519@openssh.com`, `sk-ecdsa-sha2-nistp256@openssh.com` |
| Auth | password, publickey, keyboard-interactive, agent, none — with a preference order |
| Certificates | parse, plus local CA signing and verification |

Two gaps were flagged for verification; both are **resolved** against `russh`
0.63.3 source (see `docs/re/RUSSH-GAP.md`):

1. **FIDO2 `sk-*` keys — achievable, no fork required.** `russh::Signer::auth_sign(&mut
   self, key, hash_alg, to_sign) -> Vec<u8>` is consumed by
   `Handle::authenticate_publickey_with(...)` and
   `authenticate_certificate_with(...)`. We produce the signature with
   `ctap-hid-fido2` and return `to_sign` with the SSH-encoded signature blob
   appended (the same contract `AgentClient` uses). `ssh-key`'s
   `Algorithm::SkEd25519` / `SkEcdsaSha2NistP256` parse the public key.
2. **Local CA certificate signing — lives in `ssh-key`, not `russh`.**
   `ssh_key::certificate::Builder::sign(&ca_key)`, with verification via
   `Certificate::validate` / `validate_at`. Pure Rust either way.

**The one genuine parity gap is `umac-64` / `umac-128`.** `russh`'s
`MacAlgorithm` and its `MACS` table are `pub(crate)` with no public registration
hook, so umac support means forking the crate. It only matters for servers that
offer umac *and* no hmac fallback, which is rare. Decision: accept the gap,
record it, revisit only if a real target host fails to connect.

`3des-cbc` is **not** a gap — it exists behind `russh`'s `des` Cargo feature, off
by default. Enable that feature.

Two version cautions: `russh-keys` on crates.io is stale (0.50.0-beta.7, Jan
2025) — use the re-export at `russh::keys`. And `russh` pins the pre-release
`ssh-key =0.7.0-rc.11`, so the certificate and `sk-*` APIs are
version-specific: pin `russh` exactly.

One correction to the UI catalogue's model: jump hosts were **not** confirmed as
`ProxyJump` in the native surface — the UI models them as *host chains*. Support
both chained dialling and `ProxyJump` semantics rather than assuming one.

## Secrets and vault

The original keeps secrets in the macOS Keychain (`keytar`) and Secure Enclave
(`sep::`), encrypts the local store with SJCL AES-GCM, and encrypts cloud sync
end-to-end with argon2id + RNCryptor v3 (AES-256-CBC + HMAC). **We do not target
bit-compatibility with their sync format** — our vault is our own. Only the
*plaintext* import path matters, which is why Termius JSON import is the
migration story instead of trying to read their encrypted store.

## Definition of parity

Parity means: for every Tier A/B/C feature in the catalogue, the same task is
achievable in `sshdeck` without leaving the app, at comparable fidelity. It does
not mean identical code, assets, or backend.

## Non-negotiables carried from day one

- No local builds (CI only), per `AGENTS.md`.
- No third-party code, icons, fonts, or branding copied into this repo.
- One runnable check per non-trivial logic path.
