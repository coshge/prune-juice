import AppKit

// The entry point. A file named `main.swift` would be top-level code, which is
// why this lives here and calls into AppKit explicitly.
// The delegate and everything it owns are main-actor bound; top-level code in
// main.swift already runs there.
let delegate = MainActor.assumeIsolated { AppDelegate() }
let app = NSApplication.shared
app.delegate = delegate
Diagnostics.log("app starting; bundle=\(Bundle.main.bundlePath) helper=\(SubprocessService.helperPath)")
app.run()
