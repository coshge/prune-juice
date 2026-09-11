//! How this copy of `prune-juice` got here, and what follows from that.
//!
//! The rule: **whoever installed it owns updating it.** A binary under a
//! package manager's control must never be overwritten in place — the manager
//! would then be serving a version it did not install, its manifest and
//! receipts would be wrong, and the next `brew upgrade` would silently undo
//! the update. So the check still runs and still reports the new version; only
//! the *action* changes, to the command that manager understands.
//!
//! The same rule covers the Mac app: the helper inside `Prune Juice.app` is
//! sealed into a signed bundle, and replacing one file inside it breaks the
//! bundle's signature. Sparkle updates the whole bundle instead.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The helper inside `Prune Juice.app`. Sparkle updates the bundle whole,
    /// which is what keeps the app and its helper the same version.
    AppBundle,
    /// Homebrew, with the prefix it lives under.
    Homebrew,
    /// `cargo install`.
    Cargo,
    MacPorts,
    /// The Nix store, which is read-only by design.
    NixStore,
    /// A binary someone downloaded and put somewhere. The only case this tool
    /// will replace.
    Standalone,
}

impl Origin {
    /// Classify the running executable.
    ///
    /// The path is canonicalised first, which is the whole trick: Homebrew and
    /// MacPorts both install into a Cellar-like tree and symlink into `bin`, so
    /// the interesting evidence is only visible after the symlink is resolved.
    /// `/usr/local/bin/prune-juice` alone cannot tell you anything.
    pub fn detect(exe: &Path) -> Self {
        let path = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
        Self::classify(
            &path,
            std::env::var("CARGO_HOME").ok(),
            std::env::var("HOME").ok(),
        )
    }

    /// The decision, separated from the filesystem so it can be tested with
    /// paths that do not exist on the machine running the test.
    pub fn classify(path: &Path, cargo_home: Option<String>, home: Option<String>) -> Self {
        let text = path.to_string_lossy();

        // Checked first: a bundled helper can sit anywhere, including inside a
        // .app that was copied into /usr/local, so the bundle wins over any
        // prefix that happens to contain it.
        if path
            .ancestors()
            .any(|a| a.extension().is_some_and(|e| e == "app"))
        {
            return Origin::AppBundle;
        }
        if text.starts_with("/nix/store/") {
            return Origin::NixStore;
        }
        // `Cellar` is the marker, not the prefix: it is what every Homebrew
        // prefix has in common, including a custom `--prefix` and Linuxbrew.
        if path.components().any(|c| c.as_os_str() == "Cellar") {
            return Origin::Homebrew;
        }
        if text.starts_with("/opt/local/") {
            return Origin::MacPorts;
        }
        let cargo_bin = cargo_home
            .map(PathBuf::from)
            .or_else(|| home.as_deref().map(|h| Path::new(h).join(".cargo")))
            .map(|c| c.join("bin"));
        if cargo_bin.is_some_and(|b| path.starts_with(b)) {
            return Origin::Cargo;
        }
        Origin::Standalone
    }

    /// May this tool overwrite its own executable?
    pub fn self_replace_allowed(&self) -> bool {
        matches!(self, Origin::Standalone)
    }

    /// What the user should run instead, where a manager owns the binary.
    pub fn upgrade_command(&self) -> Option<&'static str> {
        match self {
            Origin::Standalone => None,
            Origin::AppBundle => None,
            Origin::Homebrew => Some("brew upgrade prune-juice"),
            Origin::Cargo => Some("cargo install prune-juice-cli --force"),
            Origin::MacPorts => Some("sudo port upgrade prune-juice"),
            Origin::NixStore => Some("nix profile upgrade prune-juice"),
        }
    }

    /// One sentence on why self-replacement is refused here. Present for every
    /// managed origin, because "no" without a reason is not an answer.
    pub fn why_not(&self) -> Option<&'static str> {
        match self {
            Origin::Standalone => None,
            Origin::AppBundle => Some(
                "this copy is the helper inside Prune Juice.app; the app updates \
                 itself and its helper together",
            ),
            Origin::Homebrew => Some("this copy is managed by Homebrew"),
            Origin::Cargo => Some("this copy was installed by cargo"),
            Origin::MacPorts => Some("this copy is managed by MacPorts"),
            Origin::NixStore => Some("the Nix store is read-only by design"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(p: &str) -> Origin {
        Origin::classify(Path::new(p), None, Some("/Users/x".into()))
    }

    #[test]
    fn homebrew_is_recognised_by_its_cellar_not_by_a_hardcoded_prefix() {
        assert_eq!(
            classify("/opt/homebrew/Cellar/prune-juice/0.1.0/bin/prune-juice"),
            Origin::Homebrew
        );
        assert_eq!(
            classify("/usr/local/Cellar/prune-juice/0.1.0/bin/prune-juice"),
            Origin::Homebrew
        );
        assert_eq!(
            classify("/home/linuxbrew/.linuxbrew/Cellar/prune-juice/0.1.0/bin/prune-juice"),
            Origin::Homebrew
        );
        // A custom prefix has no well-known path, and still has a Cellar.
        assert_eq!(
            classify("/data/brew/Cellar/prune-juice/0.1.0/bin/prune-juice"),
            Origin::Homebrew
        );
    }

    #[test]
    fn the_bundled_helper_is_never_self_replaced() {
        let o = classify("/Applications/Prune Juice.app/Contents/MacOS/prune-juice");
        assert_eq!(o, Origin::AppBundle);
        assert!(!o.self_replace_allowed());
        assert!(o.why_not().is_some());
        // Nothing for the user to run: the app handles it.
        assert!(o.upgrade_command().is_none());
    }

    #[test]
    fn a_bundle_inside_a_managed_prefix_is_still_a_bundle() {
        // The order of the checks is the point. A .app copied under a
        // Homebrew prefix must not be offered `brew upgrade`.
        assert_eq!(
            classify("/opt/homebrew/Cellar/x/1/Prune Juice.app/Contents/MacOS/prune-juice"),
            Origin::AppBundle
        );
    }

    #[test]
    fn cargo_bin_is_found_through_cargo_home_or_the_default() {
        assert_eq!(
            Origin::classify(
                Path::new("/Users/x/.cargo/bin/prune-juice"),
                None,
                Some("/Users/x".into())
            ),
            Origin::Cargo
        );
        assert_eq!(
            Origin::classify(
                Path::new("/opt/cargo/bin/prune-juice"),
                Some("/opt/cargo".into()),
                Some("/Users/x".into())
            ),
            Origin::Cargo
        );
    }

    #[test]
    fn a_downloaded_binary_is_the_only_thing_we_will_overwrite() {
        let o = classify("/Users/x/bin/prune-juice");
        assert_eq!(o, Origin::Standalone);
        assert!(o.self_replace_allowed());
        assert!(o.upgrade_command().is_none());
        assert!(o.why_not().is_none());
    }

    #[test]
    fn every_managed_origin_says_why_and_what_to_run_instead() {
        for o in [
            Origin::Homebrew,
            Origin::Cargo,
            Origin::MacPorts,
            Origin::NixStore,
        ] {
            assert!(!o.self_replace_allowed(), "{o:?}");
            assert!(o.why_not().is_some(), "{o:?}");
            assert!(o.upgrade_command().is_some(), "{o:?}");
        }
    }

    #[test]
    fn the_nix_store_is_never_written_to() {
        assert_eq!(
            classify("/nix/store/abc123-prune-juice-0.1.0/bin/prune-juice"),
            Origin::NixStore
        );
    }
}
