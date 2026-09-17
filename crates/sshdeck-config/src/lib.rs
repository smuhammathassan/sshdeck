//! Persisted application settings for sshdeck.
//!
//! Pure logic and file I/O: no UI. The UI reads and writes this model; the
//! values are clamped on load so a hand-edited file cannot produce a state the
//! app cannot render.
