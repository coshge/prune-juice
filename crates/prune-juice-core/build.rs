//! Two compile-time facts the updater cannot work out at runtime.
//!
//! * `PRUNE_JUICE_TARGET` — the exact target triple this binary was built for.
//!   The updater has to pick one artifact out of a release, and guessing from
//!   `std::env::consts` cannot tell gnu from musl. Cargo already knows.
//! * `PRUNE_JUICE_UPDATE_PUBKEY` — the release-signing key, when one is being
//!   injected. Declared here only so that changing it triggers a rebuild;
//!   without the `rerun-if-env-changed` line a stale key would be baked in.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=PRUNE_JUICE_TARGET={target}");
    println!("cargo:rerun-if-env-changed=PRUNE_JUICE_UPDATE_PUBKEY");
    println!("cargo:rerun-if-changed=build.rs");
}
