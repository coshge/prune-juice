import Foundation

/// What the app needs from the engine.
///
/// A protocol, not a concrete type, so the transport can change without the
/// interface noticing. `SubprocessService` is the shipping implementation; a
/// `NativeService` over UniFFI would satisfy the same protocol, and swapping
/// them is one line in the composition root.
///
/// Callback-shaped rather than `async` on purpose — it is the same shape a
/// UniFFI callback interface takes, so the seam stays honest.
protocol PruneJuiceService: Sendable {
    func scan(
        onEvent: @escaping @Sendable (PJEvent) -> Void,
        onFinish: @escaping @Sendable (Error?) -> Void
    )
}

/// A tiny mutable box, so a background queue can accumulate into it.
final class Box<T>: @unchecked Sendable {
    var value: T
    init(_ value: T) { self.value = value }
}

/// Failures under a GUI launch have nowhere to go — there is no terminal to
/// print to, and the unified log is awkward to reach for. A file the user can
/// be pointed at is worth more than either.
enum Diagnostics {
    static let path = FileManager.default
        .homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/prune-juice-app.log")

    static func log(_ message: String) {
        let line = "\(ISO8601DateFormatter().string(from: Date()))  \(message)\n"
        let dir = path.deletingLastPathComponent()
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        if let h = try? FileHandle(forWritingTo: path) {
            h.seekToEndOfFile()
            h.write(Data(line.utf8))
            try? h.close()
        } else {
            try? Data(line.utf8).write(to: path)
        }
    }
}

enum ServiceError: LocalizedError {
    case helperMissing
    case helperFailed(code: Int32, stderr: String)
    case unsupportedProtocol(UInt32)
    case lineTooLong

    var errorDescription: String? {
        switch self {
        case .helperMissing:
            // Never fall back to a $PATH copy. A missing bundled helper means a
            // damaged install, and a $PATH binary could be unsigned, a
            // different version, or hostile.
            return """
                The bundled prune-juice helper is missing from this app, which \
                means the install is damaged. Reinstall rather than running a \
                copy from somewhere else.
                """
        case .helperFailed(let code, let stderr):
            let detail = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
            return detail.isEmpty ? "The helper exited with code \(code)." : detail
        case .unsupportedProtocol(let v):
            return
                "The helper speaks protocol v\(v); this app understands v\(ProtocolVersion.supported)."
        case .lineTooLong:
            return "The helper produced an implausibly long line and was stopped."
        }
    }
}

/// Talks to the bundled CLI over NDJSON.
final class SubprocessService: PruneJuiceService {
    /// The helper lives in `Contents/MacOS`, which is where
    /// `forAuxiliaryExecutable` looks — and the only place a nested executable
    /// can sit without breaking bundle sealing and notarisation.
    private static func helperURL() -> URL? {
        if let u = Bundle.main.url(forAuxiliaryExecutable: "prune-juice") {
            return u
        }
        // `swift run` during development has no bundle. Deliberately a sibling
        // of the built binary, never $PATH.
        let sibling = Bundle.main.bundleURL.appendingPathComponent("prune-juice")
        return FileManager.default.isExecutableFile(atPath: sibling.path) ? sibling : nil
    }

    static var helperAvailable: Bool { helperURL() != nil }
    static var helperPath: String { helperURL()?.path ?? "(not found)" }

    func scan(
        onEvent: @escaping @Sendable (PJEvent) -> Void,
        onFinish: @escaping @Sendable (Error?) -> Void
    ) {
        guard let helper = Self.helperURL() else {
            Diagnostics.log("helper not found; searched the app bundle only, never $PATH")
            onFinish(ServiceError.helperMissing)
            return
        }
        Diagnostics.log("starting scan via \(helper.path)")

        // Read on a background thread with a plain `availableData` loop.
        //
        // The tempting shape — waitUntilExit() then read — deadlocks the moment
        // the pipe buffer fills, and yields no progress even when it does not.
        // A 682-resource scan overflows a 64 KB pipe long before it finishes.
        DispatchQueue.global(qos: .userInitiated).async {
            let process = Process()
            process.executableURL = helper
            // --no-tui because stdout is a pipe. The CLI would work that out on
            // its own; being explicit means the app does not rely on it.
            process.arguments = ["--json", "--no-tui"]

            let out = Pipe()
            let err = Pipe()
            process.standardOutput = out
            process.standardError = err

            do {
                try process.run()
            } catch {
                Diagnostics.log("could not launch helper: \(error)")
                onFinish(error)
                return
            }

            // Drain stderr concurrently.
            //
            // Not optional. A pipe nobody reads fills at 64 KB and the writer
            // blocks on it for ever — so leaving stderr until after the process
            // exits can deadlock the very process we are waiting for. The
            // symptom is a helper that hangs with no output, which is exactly
            // what a GUI launch looked like.
            let stderrBox = Box<Data>(Data())
            let stderrDone = DispatchSemaphore(value: 0)
            DispatchQueue.global(qos: .utility).async {
                let h = err.fileHandleForReading
                while true {
                    let chunk = h.availableData
                    if chunk.isEmpty { break }
                    stderrBox.value.append(chunk)
                }
                stderrDone.signal()
            }

            var buffer = Data()
            let maxLine = 4 * 1024 * 1024
            let handle = out.fileHandleForReading
            var failure: Error?

            while true {
                let chunk = handle.availableData
                if chunk.isEmpty { break }  // EOF: the process has exited
                buffer.append(chunk)

                while let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                    let line = Data(buffer[buffer.startIndex..<nl])
                    buffer.removeSubrange(buffer.startIndex...nl)
                    if line.isEmpty { continue }
                    do {
                        if let env = try Envelope.decode(line: line) {
                            onEvent(env.event)
                        }
                    } catch DecodeError.unsupportedProtocol(let v) {
                        failure = ServiceError.unsupportedProtocol(v)
                        break
                    } catch {
                        // A line we cannot parse is skipped, not fatal: an
                        // unknown event must not break an older app.
                    }
                }
                if failure != nil { break }
                if buffer.count > maxLine {
                    failure = ServiceError.lineTooLong
                    break
                }
            }

            if failure != nil { process.terminate() }
            process.waitUntilExit()
            _ = stderrDone.wait(timeout: .now() + 5)

            if let failure {
                Diagnostics.log("scan failed: \(failure)")
                onFinish(failure)
                return
            }

            // stderr carries human progress, never protocol. It is read only to
            // explain a failure. Exit 1 means "findings", which is not one.
            let code = process.terminationStatus
            let text = String(data: stderrBox.value, encoding: .utf8) ?? ""
            if code != 0 && code != 1 {
                Diagnostics.log("helper exited \(code): \(text)")
                onFinish(ServiceError.helperFailed(code: code, stderr: text))
            } else {
                if !text.isEmpty { Diagnostics.log("helper stderr: \(text)") }
                onFinish(nil)
            }
        }
    }
}
