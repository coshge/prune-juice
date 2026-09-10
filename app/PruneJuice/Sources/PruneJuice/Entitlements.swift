import Foundation

/// Things the app can do.
///
/// The seam for a possible one-time purchase later, present from the start so
/// adding one means swapping a single implementation rather than threading
/// checks through the interface.
///
/// Two rules hold this in place. Nothing here ever appears in the Rust crates —
/// those stay MIT and licence-unaware, so the CLI is genuinely free and there
/// is nothing in the open-source half for anyone to patch out. And today every
/// feature is enabled for everyone.
enum Feature: String, CaseIterable {
    case scan
    case reclaimSafeTier
    case reviewAndAct
    case vaultRestore
    case history
    case scheduledCleanup
}

protocol Entitlements: Sendable {
    func isEnabled(_ feature: Feature) -> Bool
}

/// The only implementation. Everything is on.
struct AlwaysEntitled: Entitlements {
    func isEnabled(_ feature: Feature) -> Bool { true }
}
