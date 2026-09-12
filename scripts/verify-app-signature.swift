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
