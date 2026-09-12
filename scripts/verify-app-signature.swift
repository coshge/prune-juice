import CryptoKit
import Foundation

// Verify against the public key actually shipped inside the app, without
// loading a private key or touching the user's keychain.
guard CommandLine.arguments.count == 3 else {
    fatalError("Usage: verify-app-signature.swift archive.zip signature.txt")
}
let archiveURL = URL(fileURLWithPath: CommandLine.arguments[1])
let signatureURL = URL(fileURLWithPath: CommandLine.arguments[2])
let signatureText = try String(contentsOf: signatureURL, encoding: .utf8)
    .trimmingCharacters(in: .whitespacesAndNewlines)
guard let signature = Data(base64Encoded: signatureText), signature.count == 64 else {
    fatalError("Missing or malformed Sparkle signature")
}
let unzip = Process()
unzip.executableURL = URL(fileURLWithPath: "/usr/bin/unzip")
unzip.arguments = ["-p", archiveURL.path, "Prune Juice.app/Contents/Info.plist"]
let output = Pipe()
unzip.standardOutput = output
try unzip.run()
let plistData = output.fileHandleForReading.readDataToEndOfFile()
unzip.waitUntilExit()
guard unzip.terminationStatus == 0,
      let plist = try PropertyListSerialization.propertyList(from: plistData, format: nil) as? [String: Any],
      let publicKeyText = plist["SUPublicEDKey"] as? String,
      let publicKeyData = Data(base64Encoded: publicKeyText) else {
    fatalError("Cannot read the app's Sparkle public key")
}
let key = try Curve25519.Signing.PublicKey(rawRepresentation: publicKeyData)
let archive = try Data(contentsOf: archiveURL)
guard key.isValidSignature(signature, for: archive) else {
    fatalError("Archive signature does not match the app's public key")
}
print("App archive signature verified against its bundled public key")

func run(_ executable: String, _ arguments: [String]) throws {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: executable)
    process.arguments = arguments
    try process.run()
    process.waitUntilExit()
    guard process.terminationStatus == 0 else {
        fatalError("Archive validation failed: \(executable)")
    }
}

let temporary = FileManager.default.temporaryDirectory
    .appendingPathComponent("prune-juice-archive-\(UUID().uuidString)")
try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: false)
defer { try? FileManager.default.removeItem(at: temporary) }
try run("/usr/bin/ditto", ["-x", "-k", archiveURL.path, temporary.path])
let app = temporary.appendingPathComponent("Prune Juice.app")
try run("/usr/bin/codesign", ["--verify", "--deep", "--strict", app.path])
for executable in ["PruneJuice", "prune-juice"] {
    try run("/usr/bin/lipo", [app.appendingPathComponent("Contents/MacOS/\(executable)").path,
                             "-verify_arch", "arm64", "x86_64"])
}
print("Downloaded app has valid nested signatures and both Mac architectures")
