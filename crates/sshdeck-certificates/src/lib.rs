//! OpenSSH certificate support for sshdeck.
//!
//! Host and user certificates: parse, validate against a CA, and sign with a
//! local CA key. Pure logic over `ssh-key` types reached through
//! `russh::keys`, no UI and no transport.
//!
//! # Trust boundary — fail closed
//!
//! This module decides whether an SSH host (or user) is the one it claims to
//! be, so every check is mandatory and any malformed input is an error, never a
//! permissive default. [`Certificate::validate`] runs each check as its own
//! step and returns a distinct [`CertificateError`] for each, so the caller can
//! report *why* a certificate was refused:
//!
//! 1. **Type** — the certificate must be the kind the caller expects. A user
//!    certificate must never validate as a host certificate, and vice versa.
//! 2. **Trust** — the CA key embedded in the certificate must byte-for-byte
//!    equal one of the caller-supplied trusted CA keys. A valid signature from
//!    an untrusted CA fails here.
//! 3. **Window** — `valid_after <= now < valid_before`, with `now` injected so
//!    the decision is deterministic and testable.
//! 4. **Signature** — the CA signature must verify over the certificate body.
//! 5. **Principal** — the host/user name must be authorized by the certificate.
//!
//! A certificate that is unsigned, truncated, or otherwise malformed fails at
//! [`Certificate::from_openssh`] with [`CertificateError::Parse`].
//!
//! ## Principals (OpenSSH semantics)
//!
//! Current OpenSSH (the December 2025 hardening, CVE-2024-7594) **rejects a
//! certificate with an empty principal list**, so this module does too —
//! including host certificates. The older "empty list means any host"
//! behaviour is deliberately not reproduced: it was a fail-open footgun.
//!
//! Wildcards (`*` and `?`, glob only, no character classes) are honoured for
//! **host** certificates only, exactly as `sshkey_cert_check_host` does. User
//! certificates require an exact principal match. Certificate principals are
//! never hashed: a `|1|...` string is compared literally, never as a hashed
//! `known_hosts` entry.

use core::fmt;

use russh::keys::ssh_key;
use russh::keys::ssh_key::certificate::{Builder, CertType, OptionsMap};
use russh::keys::ssh_key::{Certificate as SshCertificate, Fingerprint, HashAlg};
use russh::keys::{PrivateKey, PublicKey};

/// Which kind of certificate a caller expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertUse {
    /// A host certificate (`CertType::Host`), presented by a server.
    Host,
    /// A user certificate (`CertType::User`), presented by a client.
    User,
}

impl fmt::Display for CertUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Host => "host",
            Self::User => "user",
        })
    }
}

/// A failure parsing, signing, or validating an OpenSSH certificate.
///
/// One variant per reason, so a caller can tell "wrong CA" from "expired" from
/// "wrong principal" — those are very different things to a user.
#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    /// The OpenSSH certificate text did not parse.
    #[error("certificate parse failed: {0}")]
    Parse(String),
    /// The certificate could not be re-encoded to OpenSSH text.
    #[error("certificate encoding failed: {0}")]
    Encode(String),
    /// Signing the certificate failed.
    #[error("certificate signing failed: {0}")]
    Sign(String),
    /// A certificate field was invalid or set twice while signing.
    #[error("invalid or duplicate certificate field: {0}")]
    InvalidField(String),
    /// A signing request listed no principals.
    #[error("a certificate must list at least one principal")]
    MissingPrincipals,
    /// The certificate is not the type the caller needs.
    #[error("certificate type does not match: expected a {expected} certificate")]
    WrongType {
        /// The kind the caller asked for.
        expected: CertUse,
    },
    /// The certificate's CA is not one of the trusted CAs.
    #[error("certificate was not signed by a trusted CA (CA fingerprint {ca})")]
    UntrustedCa {
        /// SHA-256 fingerprint of the CA embedded in the certificate.
        ca: String,
    },
    /// The CA signature over the certificate body does not verify.
    #[error("certificate CA signature is invalid (CA fingerprint {ca})")]
    BadSignature {
        /// SHA-256 fingerprint of the CA embedded in the certificate.
        ca: String,
    },
    /// The certificate's validity window has not opened yet.
    #[error("certificate is not valid until {valid_after} (now {now})")]
    NotYetValid {
        /// Unix seconds the certificate becomes valid.
        valid_after: u64,
        /// The injected current time.
        now: u64,
    },
    /// The certificate's validity window has closed.
    #[error("certificate expired at {valid_before} (now {now})")]
    Expired {
        /// Unix seconds the certificate stops being valid.
        valid_before: u64,
        /// The injected current time.
        now: u64,
    },
    /// The certificate carries an empty principal list, which OpenSSH rejects.
    #[error("certificate has an empty principal list, which OpenSSH rejects")]
    NoPrincipals,
    /// The certificate does not authorize the presented principal.
    #[error("certificate does not authorize principal {principal:?}")]
    WrongPrincipal {
        /// The host or user name that was presented.
        principal: String,
    },
}

/// A parsed, inspectable OpenSSH certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    inner: SshCertificate,
}

impl Certificate {
    /// Parses an OpenSSH certificate line
    /// (`ssh-ed25519-cert-v01@openssh.com AAAA... comment`).
    ///
    /// Fails closed: anything malformed or truncated is a
    /// [`CertificateError::Parse`], never a partially-trusted value.
    pub fn from_openssh(line: &str) -> Result<Self, CertificateError> {
        SshCertificate::from_openssh(line)
            .map(|inner| Self { inner })
            .map_err(|err| CertificateError::Parse(err.to_string()))
    }

    /// User or host certificate.
    pub fn cert_type(&self) -> CertType {
        self.inner.cert_type()
    }

    /// CA-chosen key identifier.
    pub fn key_id(&self) -> &str {
        self.inner.key_id()
    }

    /// CA-chosen serial number (`0` when unset).
    pub fn serial(&self) -> u64 {
        self.inner.serial()
    }

    /// Unix seconds the certificate becomes valid.
    pub fn valid_after(&self) -> u64 {
        self.inner.valid_after()
    }

    /// Unix seconds the certificate stops being valid.
    pub fn valid_before(&self) -> u64 {
        self.inner.valid_before()
    }

    /// Authorized principals (hostnames or usernames). May be empty, which
    /// validation rejects.
    pub fn principals(&self) -> &[String] {
        self.inner.valid_principals()
    }

    /// Critical options; unknown ones must cause rejection.
    pub fn critical_options(&self) -> &OptionsMap {
        self.inner.critical_options()
    }

    /// Non-critical extensions; unknown ones may be ignored.
    pub fn extensions(&self) -> &OptionsMap {
        self.inner.extensions()
    }

    /// Comment on the certificate.
    pub fn comment(&self) -> &str {
        self.inner.comment()
    }

    /// `SHA256:...` fingerprint of the signing CA's public key.
    pub fn ca_fingerprint(&self) -> Fingerprint {
        self.inner.signature_key().fingerprint(HashAlg::Sha256)
    }

    /// The certificate text, i.e. the pasteable `authorized_keys`-style line
    /// (`<cert-algorithm> <base64> <comment>`). The same certificate field is
    /// what OpenSSH writes into `known_hosts` for a host certificate.
    pub fn to_openssh(&self) -> Result<String, CertificateError> {
        self.inner
            .to_openssh()
            .map_err(|err| CertificateError::Encode(err.to_string()))
    }

    /// The underlying `ssh-key` certificate, e.g. to hand to russh's
    /// `authenticate_certificate_with`.
    pub fn as_ssh_key(&self) -> &SshCertificate {
        &self.inner
    }

    /// Validates the certificate for one use, principal, and point in time.
    ///
    /// `trusted_cas` is the trusted CA set as public keys; the certificate's
    /// embedded CA must match one of them by key data. `now` is Unix seconds,
    /// injected so the caller controls the clock.
    ///
    /// Every check is mandatory. `Ok(())` means all of type, trust, window,
    /// signature, and principal passed; otherwise a distinct
    /// [`CertificateError`] names the first failure.
    pub fn validate(
        &self,
        expected: CertUse,
        principal: &str,
        now: u64,
        trusted_cas: &[PublicKey],
    ) -> Result<(), CertificateError> {
        // 1. The certificate must be the type the caller is using it for.
        if self.inner.cert_type().is_host() != (expected == CertUse::Host) {
            return Err(CertificateError::WrongType { expected });
        }

        // 2. The embedded CA must be a trusted CA, compared by key data rather
        //    than a hash so trust never rests on a fingerprint collision.
        let Some(ca) = trusted_cas
            .iter()
            .find(|ca| ca.key_data() == self.inner.signature_key())
        else {
            return Err(CertificateError::UntrustedCa {
                ca: self.ca_fingerprint().to_string(),
            });
        };

        // 3. Validity window.
        if now < self.inner.valid_after() {
            return Err(CertificateError::NotYetValid {
                valid_after: self.inner.valid_after(),
                now,
            });
        }
        if now >= self.inner.valid_before() {
            return Err(CertificateError::Expired {
                valid_before: self.inner.valid_before(),
                now,
            });
        }

        // 4. The CA signature. Trust and the window already hold, so the only
        //    remaining reason `validate_at` can fail is the signature itself.
        let ca_fingerprint = ca.fingerprint(HashAlg::Sha256);
        self.inner
            .validate_at(now, core::iter::once(&ca_fingerprint))
            .map_err(|_| CertificateError::BadSignature {
                ca: self.ca_fingerprint().to_string(),
            })?;

        // 5. The principal must be authorized.
        self.validate_principal(expected, principal)
    }

    /// Checks the presented principal against the certificate's list, with
    /// OpenSSH's semantics (see the module docs).
    fn validate_principal(
        &self,
        expected: CertUse,
        principal: &str,
    ) -> Result<(), CertificateError> {
        let principals = self.inner.valid_principals();
        if principals.is_empty() {
            return Err(CertificateError::NoPrincipals);
        }

        let matched = match expected {
            // Host certificate principals may use glob wildcards.
            CertUse::Host => principals
                .iter()
                .any(|candidate| wildcard_match(principal, candidate)),
            // User certificate principals are exact.
            CertUse::User => principals
                .iter()
                .any(|candidate| candidate.as_str() == principal),
        };

        if matched {
            Ok(())
        } else {
            Err(CertificateError::WrongPrincipal {
                principal: principal.to_string(),
            })
        }
    }
}

impl serde::Serialize for Certificate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let line = self
            .to_openssh()
            .map_err(|err| <S::Error as serde::ser::Error>::custom(err.to_string()))?;
        serde::Serializer::serialize_str(serializer, &line)
    }
}

impl<'de> serde::Deserialize<'de> for Certificate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let line = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::from_openssh(&line)
            .map_err(|err| <D::Error as serde::de::Error>::custom(err.to_string()))
    }
}

/// Everything needed to sign one certificate with a local CA.
///
/// Build it with [`SignRequest::new`], add fields, then [`SignRequest::sign`].
#[derive(Debug, Clone)]
pub struct SignRequest {
    subject: PublicKey,
    cert_type: CertType,
    key_id: String,
    serial: u64,
    valid_after: u64,
    valid_before: u64,
    principals: Vec<String>,
    critical_options: Vec<(String, String)>,
    extensions: Vec<(String, String)>,
    comment: Option<String>,
}

impl SignRequest {
    /// A signing request for `subject`'s public key.
    ///
    /// At least one [`Self::principal`] is required; OpenSSH and
    /// [`Certificate::validate`] both reject principal-less certificates.
    pub fn new(
        subject: PublicKey,
        cert_type: CertType,
        key_id: impl Into<String>,
        valid_after: u64,
        valid_before: u64,
    ) -> Self {
        Self {
            subject,
            cert_type,
            key_id: key_id.into(),
            serial: 0,
            valid_after,
            valid_before,
            principals: Vec::new(),
            critical_options: Vec::new(),
            extensions: Vec::new(),
            comment: None,
        }
    }

    /// Sets the CA-chosen serial number (`0` when unset).
    pub fn serial(mut self, serial: u64) -> Self {
        self.serial = serial;
        self
    }

    /// Authorizes one principal (hostname or username). Repeatable.
    pub fn principal(mut self, principal: impl Into<String>) -> Self {
        self.principals.push(principal.into());
        self
    }

    /// Adds a critical option. Repeatable; duplicates fail at [`Self::sign`].
    pub fn critical_option(mut self, name: impl Into<String>, data: impl Into<String>) -> Self {
        self.critical_options.push((name.into(), data.into()));
        self
    }

    /// Adds a non-critical extension. Repeatable; duplicates fail at
    /// [`Self::sign`].
    pub fn extension(mut self, name: impl Into<String>, data: impl Into<String>) -> Self {
        self.extensions.push((name.into(), data.into()));
        self
    }

    /// Sets the certificate comment.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Signs the request with `ca` and returns the certificate.
    ///
    /// The pasteable line is [`Certificate::to_openssh`].
    pub fn sign(&self, ca: &PrivateKey) -> Result<Certificate, CertificateError> {
        if self.principals.is_empty() {
            return Err(CertificateError::MissingPrincipals);
        }

        let mut builder = Builder::new_with_random_nonce(
            &mut rand::rng(),
            &self.subject,
            self.valid_after,
            self.valid_before,
        )
        .map_err(field_error)?;
        builder.serial(self.serial).map_err(field_error)?;
        builder.cert_type(self.cert_type).map_err(field_error)?;
        builder.key_id(self.key_id.as_str()).map_err(field_error)?;
        for principal in &self.principals {
            builder
                .valid_principal(principal.as_str())
                .map_err(field_error)?;
        }
        for (name, data) in &self.critical_options {
            builder
                .critical_option(name.as_str(), data.as_str())
                .map_err(field_error)?;
        }
        for (name, data) in &self.extensions {
            builder
                .extension(name.as_str(), data.as_str())
                .map_err(field_error)?;
        }
        if let Some(comment) = &self.comment {
            builder.comment(comment.as_str()).map_err(field_error)?;
        }

        let inner = builder
            .sign(ca)
            .map_err(|err| CertificateError::Sign(err.to_string()))?;
        Ok(Certificate { inner })
    }
}

fn field_error(err: ssh_key::Error) -> CertificateError {
    CertificateError::InvalidField(err.to_string())
}

/// OpenSSH's `match_pattern` semantics: `*` matches any run (including empty)
/// and `?` matches exactly one byte. No character classes and no escaping.
fn wildcard_match(name: &str, pattern: &str) -> bool {
    let name = name.as_bytes();
    let pattern = pattern.as_bytes();
    let (mut n, mut p) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            n += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(star) = star {
            p = star + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::Algorithm;

    const NOW: u64 = 1_700_000_000;
    const AFTER: u64 = NOW - 3_600;
    const BEFORE: u64 = NOW + 3_600;

    fn key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("generate key")
    }

    fn host_request(subject: &PrivateKey, principals: &[&str]) -> SignRequest {
        let mut request = SignRequest::new(
            subject.public_key().clone(),
            CertType::Host,
            "host-id",
            AFTER,
            BEFORE,
        );
        for principal in principals {
            request = request.principal(*principal);
        }
        request
    }

    fn trusted(ca: &PrivateKey) -> Vec<PublicKey> {
        vec![ca.public_key().clone()]
    }

    #[test]
    fn signed_certificate_round_trips_and_exposes_its_fields() {
        let ca = key();
        let subject = key();
        let certificate = host_request(&subject, &["host.example", "other.example"])
            .serial(42)
            .sign(&ca)
            .expect("sign");

        assert_eq!(certificate.cert_type(), CertType::Host);
        assert_eq!(certificate.key_id(), "host-id");
        assert_eq!(certificate.serial(), 42);
        assert_eq!(certificate.valid_after(), AFTER);
        assert_eq!(certificate.valid_before(), BEFORE);
        assert_eq!(
            certificate.principals().join(","),
            "host.example,other.example"
        );
        assert!(certificate.critical_options().is_empty());
        assert!(certificate.extensions().is_empty());
        assert_eq!(
            certificate.ca_fingerprint(),
            ca.public_key().fingerprint(HashAlg::Sha256)
        );

        let line = certificate.to_openssh().expect("encode");
        assert!(
            line.starts_with("ssh-ed25519-cert-v01@openssh.com "),
            "got {line}"
        );

        let parsed = Certificate::from_openssh(&line).expect("parse");
        parsed
            .validate(CertUse::Host, "host.example", NOW, &trusted(&ca))
            .expect("valid certificate");
    }

    #[test]
    fn pasteable_line_reparses_into_an_equivalent_certificate() {
        let ca = key();
        let certificate = host_request(&key(), &["host.example"])
            .sign(&ca)
            .expect("sign");
        let line = certificate.to_openssh().expect("encode");

        let reparsed = Certificate::from_openssh(&line).expect("parse");
        assert_eq!(reparsed, certificate);
        assert_eq!(reparsed.to_openssh().expect("re-encode"), line);
    }

    #[test]
    fn a_signature_from_a_different_ca_is_rejected_as_untrusted() {
        let trusted_ca = key();
        let other_ca = key();
        let certificate = host_request(&key(), &["host.example"])
            .sign(&other_ca)
            .expect("sign with the other CA");

        let err = certificate
            .validate(CertUse::Host, "host.example", NOW, &trusted(&trusted_ca))
            .expect_err("wrong CA must fail");
        match err {
            CertificateError::UntrustedCa { ca } => {
                assert_eq!(
                    ca,
                    other_ca
                        .public_key()
                        .fingerprint(HashAlg::Sha256)
                        .to_string()
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_tampered_signature_is_rejected_as_a_bad_signature() {
        let ca = key();
        let certificate = host_request(&key(), &["host.example"])
            .sign(&ca)
            .expect("sign");
        let line = certificate.to_openssh().expect("encode");

        // Flip one base64 character 20 from the end: that lands inside the
        // trailing signature blob, so the text still parses but the signature
        // no longer verifies.
        let mut fields = line.splitn(3, ' ');
        let algorithm = fields.next().expect("algorithm");
        let body = fields.next().expect("base64");
        let rest: String = fields.next().map(|c| format!(" {c}")).unwrap_or_default();
        let mut chars: Vec<char> = body.chars().collect();
        let index = chars.len() - 20;
        chars[index] = if chars[index] == 'A' { 'B' } else { 'A' };
        let tampered_body: String = chars.into_iter().collect();
        let tampered = format!("{algorithm} {tampered_body}{rest}");

        let parsed = Certificate::from_openssh(&tampered).expect("still parses");
        let err = parsed
            .validate(CertUse::Host, "host.example", NOW, &trusted(&ca))
            .expect_err("tampered signature must fail");
        assert!(
            matches!(err, CertificateError::BadSignature { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn expired_and_not_yet_valid_certificates_are_distinguished() {
        let ca = key();
        let certificate = host_request(&key(), &["host.example"])
            .sign(&ca)
            .expect("sign");

        let err = certificate
            .validate(CertUse::Host, "host.example", BEFORE, &trusted(&ca))
            .expect_err("expired");
        assert!(matches!(err, CertificateError::Expired { .. }), "{err:?}");

        let err = certificate
            .validate(CertUse::Host, "host.example", AFTER - 1, &trusted(&ca))
            .expect_err("not yet valid");
        assert!(
            matches!(err, CertificateError::NotYetValid { .. }),
            "{err:?}"
        );

        // The window is inclusive at the start and exclusive at the end.
        certificate
            .validate(CertUse::Host, "host.example", AFTER, &trusted(&ca))
            .expect("valid at valid_after");
    }

    #[test]
    fn wrong_principal_is_rejected_and_host_wildcards_match() {
        let ca = key();
        let wildcard = host_request(&key(), &["*.example.com"])
            .sign(&ca)
            .expect("sign");

        wildcard
            .validate(CertUse::Host, "host.example.com", NOW, &trusted(&ca))
            .expect("wildcard principal matches");

        let err = wildcard
            .validate(CertUse::Host, "host.other.test", NOW, &trusted(&ca))
            .expect_err("not a listed principal");
        assert!(
            matches!(err, CertificateError::WrongPrincipal { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn user_principals_are_exact_and_never_wildcards() {
        let ca = key();
        let certificate = SignRequest::new(
            key().public_key().clone(),
            CertType::User,
            "user-id",
            AFTER,
            BEFORE,
        )
        .principal("*.example.com")
        .sign(&ca)
        .expect("sign");

        let err = certificate
            .validate(CertUse::User, "alice", NOW, &trusted(&ca))
            .expect_err("wildcards do not apply to user certificates");
        assert!(
            matches!(err, CertificateError::WrongPrincipal { .. }),
            "{err:?}"
        );

        certificate
            .validate(CertUse::User, "*.example.com", NOW, &trusted(&ca))
            .expect("literal match");
    }

    #[test]
    fn a_user_certificate_does_not_validate_as_a_host_certificate() {
        let ca = key();
        let certificate = SignRequest::new(
            key().public_key().clone(),
            CertType::User,
            "user-id",
            AFTER,
            BEFORE,
        )
        .principal("host.example")
        .sign(&ca)
        .expect("sign");

        let err = certificate
            .validate(CertUse::Host, "host.example", NOW, &trusted(&ca))
            .expect_err("user certificate as host");
        assert!(
            matches!(
                err,
                CertificateError::WrongType {
                    expected: CertUse::Host
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_host_certificate_does_not_validate_as_a_user_certificate() {
        let ca = key();
        let certificate = host_request(&key(), &["host.example"])
            .sign(&ca)
            .expect("sign");

        let err = certificate
            .validate(CertUse::User, "host.example", NOW, &trusted(&ca))
            .expect_err("host certificate as user");
        assert!(
            matches!(
                err,
                CertificateError::WrongType {
                    expected: CertUse::User
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn an_empty_principal_list_is_rejected() {
        let ca = key();
        let subject = key();
        // Built directly, because `SignRequest::sign` refuses empty principals.
        let mut builder =
            Builder::new_with_random_nonce(&mut rand::rng(), subject.public_key(), AFTER, BEFORE)
                .expect("builder");
        builder.cert_type(CertType::Host).expect("type");
        builder.key_id("no-principals").expect("key id");
        builder.all_principals_valid().expect("empty principals");
        let raw = builder.sign(&ca).expect("sign");
        let line = raw.to_openssh().expect("encode");

        let parsed = Certificate::from_openssh(&line).expect("parse");
        assert!(parsed.principals().is_empty());
        let err = parsed
            .validate(CertUse::Host, "host.example", NOW, &trusted(&ca))
            .expect_err("empty principal list must fail closed");
        assert!(matches!(err, CertificateError::NoPrincipals), "{err:?}");
    }

    #[test]
    fn signing_without_a_principal_is_refused() {
        let subject = key();
        let err = SignRequest::new(
            subject.public_key().clone(),
            CertType::Host,
            "host-id",
            AFTER,
            BEFORE,
        )
        .sign(&key())
        .expect_err("no principals");
        assert!(
            matches!(err, CertificateError::MissingPrincipals),
            "{err:?}"
        );
    }

    #[test]
    fn corrupt_and_truncated_certificates_return_typed_errors() {
        let ca = key();
        let good = host_request(&key(), &["host.example"])
            .sign(&ca)
            .expect("sign")
            .to_openssh()
            .expect("encode");
        let truncated = &good[..good.len() / 2];

        for bad in [
            "",
            "not a certificate",
            "ssh-ed25519-cert-v01@openssh.com AAAA",
            "ssh-ed25519-cert-v01@openssh.com !!!!",
            truncated,
        ] {
            let err = Certificate::from_openssh(bad).expect_err("must not parse");
            assert!(
                matches!(err, CertificateError::Parse(_)),
                "{bad:?} produced {err:?}"
            );
        }
    }

    #[test]
    fn a_certificate_round_trips_through_json() {
        let ca = key();
        let certificate = host_request(&key(), &["host.example"])
            .sign(&ca)
            .expect("sign");

        let json = serde_json::to_string(&certificate).expect("serialize");
        let back: Certificate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, certificate);
    }

    #[test]
    fn wildcard_matching_follows_openssh() {
        assert!(wildcard_match("host.example.com", "*.example.com"));
        assert!(wildcard_match("host.example.com", "host.*.com"));
        assert!(wildcard_match("anything", "*"));
        assert!(wildcard_match("ab", "a?"));
        assert!(wildcard_match("abc", "abc"));
        assert!(!wildcard_match("ab", "a?c"));
        assert!(!wildcard_match("host.other.test", "*.example.com"));
        // An empty pattern matches only an empty name, as OpenSSH does.
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("host", ""));
    }
}
