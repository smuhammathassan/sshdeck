//! OpenSSH certificate support for sshdeck.
//!
//! Host and user certificates: parse, validate against a CA, and sign with a
//! local CA key. Pure logic over `ssh-key` types, no UI and no transport.
