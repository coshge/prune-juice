//! What makes an update trustworthy.
//!
//! Two links, and the whole chain hangs off the first one:
//!
//! 1. The **manifest** is signed with minisign (Ed25519) by the release key.
//!    The public half is compiled into this binary. Nothing in the manifest —
//!    not the version, not a URL, not a digest — is believed before that
//!    signature verifies.
//! 2. Every **artifact** is named in the signed manifest with its SHA-256. So
//!    an artifact is authenticated by digest, and the digest is authenticated
//!    by the signature.
//!
//! The consequence worth stating plainly: the release host is not trusted. It
//! can serve nothing, or stale bytes, or hostile bytes, and the worst outcome
//! is that no update is installed.

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The release-signing public key, in minisign's base64 form.
///
/// Empty in this checkout on purpose. Generating a keypair means holding a
/// secret half, and a public key whose secret half nobody can find is worse
/// than none — it looks like verification is configured when it is not. See
/// `RELEASING.md`: generate the pair once, put the secret in the release
/// secrets, and paste the public half here.
const RELEASE_KEY: &str = "";

/// The key this build will verify against.
///
/// `PRUNE_JUICE_UPDATE_PUBKEY` at build time takes precedence, which is how a
/// fork or a staging feed signs with its own key without editing the source.
///
/// `None` disables the update system outright — checking as well as
/// installing. An unverifiable version number is not a lesser form of news:
/// it is a stranger telling you to go and download something.
pub fn release_key() -> Option<&'static str> {
    let key = match option_env!("PRUNE_JUICE_UPDATE_PUBKEY") {
        Some(k) => k,
        None => RELEASE_KEY,
    };
    (!key.trim().is_empty()).then_some(key.trim())
}

/// Verify a detached minisign signature over `data`.
///
/// Legacy signatures are refused: the pre-hashed form is what minisign has
/// produced by default for years, and accepting both would mean accepting the
/// weaker one.
pub fn signature(key_b64: &str, data: &[u8], sig: &str) -> Result<()> {
    use minisign_verify::{PublicKey, Signature};

    let key = PublicKey::from_base64(key_b64)
        .map_err(|e| Error::Config(format!("the release key in this build is unusable: {e}")))?;
    let sig = Signature::decode(sig)
        .map_err(|e| Error::Config(format!("the update signature is malformed: {e}")))?;
    key.verify(data, &sig, false).map_err(|e| {
        Error::Config(format!(
            "the update manifest is not signed by the release key ({e}) — \
             refusing to act on it"
        ))
    })
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Confirm bytes match the digest the signed manifest promised.
pub fn digest(data: &[u8], expected: &str) -> Result<()> {
    let actual = sha256_hex(data);
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "the downloaded file does not match the digest in the signed manifest \
             (expected {expected}, got {actual}) — refusing to install it"
        )))
    }
}

/// A test key and two signatures over the same bytes, one in each of
/// minisign's two formats.
///
/// Fixtures rather than a generated pair on purpose: the thing being tested is
/// that this crate agrees with what `minisign` actually emits, and a signature
/// this code generated itself would only prove it agrees with itself. The
/// secret half is a throwaway seed of no consequence.
#[cfg(test)]
pub(crate) mod fixture {
    pub const PUBLIC_KEY: &str = "RWSqeW3ua2q8p3N00p6JjBzCSJhKv73L3NpZFEI3P1p8SN9IqwJY2yNI";

    pub const MANIFEST: &str = r#"{"schema":1,"version":"0.2.0","artifacts":[]}"#;

    /// The modern, prehashed form (`ED`), which is what minisign produces by
    /// default and the only form this crate accepts.
    pub const SIGNATURE: &str = "untrusted comment: signature from prune-juice test key\nRUSqeW3ua2q8p/KLSYzpfGoP4SZ8hQ/gmh4yqr+9hvYxnIFvOPWjcpQ83w4vhDpuiaeM20jpBUjbq92wBj9s9LrwZJJLCT+RwwE=\ntrusted comment: timestamp:0\tfile:update-manifest.json\thashed\nROUaEimt4ze1lgkiN/1rso+7MWMIX9FMAumI+Mq+tQ8jL+mN4yG5he95LerFKWyf6VANU60PP51jnhdycTdcDg==\n";

    /// The legacy form (`Ed`), which signs the raw message. Valid minisign,
    /// and refused here.
    pub const LEGACY_SIGNATURE: &str = "untrusted comment: signature from prune-juice test key\nRWSqeW3ua2q8p9qT2o3XWSzWessxohaSzHVjTkgT1OksfWr8Qjid8DpnCgX3Brwf4aMW6oHuljqqQYyiGEx5wJCRFaUwsygk8Ac=\ntrusted comment: timestamp:0\tfile:update-manifest.json\thashed\ncRL0F1OzzcRku0qDjICMuynBKnlZO+NbBB47zDhKWG0QMnGtPI2K6m+6ZgK1OGce7q5IrLVh95eo9Y8SpURCCw==\n";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkout_with_no_release_key_has_no_update_system() {
        // Guards the property the rest of the module relies on: an empty key
        // is not "verification off", it is "the feature is absent".
        if option_env!("PRUNE_JUICE_UPDATE_PUBKEY").is_none() {
            assert!(release_key().is_none());
        }
    }

    #[test]
    fn digests_are_compared_case_insensitively_and_exactly() {
        let d = sha256_hex(b"prune juice");
        assert!(digest(b"prune juice", &d).is_ok());
        assert!(digest(b"prune juice", &d.to_uppercase()).is_ok());
        assert!(digest(b"prune juic", &d).is_err());
    }

    #[test]
    fn sha256_matches_the_known_answer_for_the_empty_input() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_real_minisign_signature_verifies() {
        signature(
            fixture::PUBLIC_KEY,
            fixture::MANIFEST.as_bytes(),
            fixture::SIGNATURE,
        )
        .expect("the fixture was produced in minisign's own format");
    }

    #[test]
    fn one_changed_byte_in_the_manifest_invalidates_it() {
        let tampered = fixture::MANIFEST.replace("0.2.0", "9.9.9");
        assert_eq!(tampered.len(), fixture::MANIFEST.len());
        let e =
            signature(fixture::PUBLIC_KEY, tampered.as_bytes(), fixture::SIGNATURE).unwrap_err();
        assert!(
            e.to_string().contains("not signed by the release key"),
            "{e}"
        );
    }

    #[test]
    fn a_legacy_signature_is_refused_even_though_it_is_valid() {
        // The pre-hashed form has been minisign's default for years. Accepting
        // both would mean accepting the weaker one, and an attacker gets to
        // pick which one they present.
        let e = signature(
            fixture::PUBLIC_KEY,
            fixture::MANIFEST.as_bytes(),
            fixture::LEGACY_SIGNATURE,
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("not signed by the release key"),
            "{e}"
        );
    }

    #[test]
    fn a_signature_from_the_wrong_key_is_refused() {
        // A syntactically valid minisign key that did not sign anything here.
        let key = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        let sig = "untrusted comment: x\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\ntrusted comment: y\n";
        assert!(signature(key, b"anything", sig).is_err());
    }
}
