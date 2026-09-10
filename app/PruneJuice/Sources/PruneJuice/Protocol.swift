import Foundation

/// The NDJSON contract, decoded.
///
/// These names are deliberate: they are what UniFFI's Swift codegen would
/// produce from the Rust `Event` enum — `lowerCamelCase` fields, an enum with
/// associated values, records as structs. If the transport is ever swapped from
/// a subprocess to an in-process FFI call, the view models keep compiling and
/// only `SubprocessService` is deleted. Rename anything here and that stops
/// being true.
enum ProtocolVersion {
    static let supported: UInt32 = 1
}

struct Totals: Decodable, Sendable {
    var containers: UInt32 = 0
    var images: UInt32 = 0
    var volumes: UInt32 = 0
    var networks: UInt32 = 0
    var buildCacheRecords: UInt32 = 0
    var volumeBytes: UInt64 = 0
    /// Sum over image stack sizes — what `docker images` adds up to. Larger
    /// than the disk the images occupy, because stacks overlap.
    var imageBytes: UInt64 = 0
    /// What the image layers actually occupy, each shared layer counted once,
    /// as the daemon computes it. `nil` when sizes were not measured — never
    /// filled in with `imageBytes`, which is a different number.
    var imageUniqueBytes: UInt64?
    var buildCacheBytes: UInt64 = 0

    /// The figure to show for images: the layer total where it is known.
    var imageDiskBytes: UInt64 { imageUniqueBytes ?? imageBytes }

    enum CodingKeys: String, CodingKey {
        case containers, images, volumes, networks
        case buildCacheRecords = "build_cache_records"
        case volumeBytes = "volume_bytes"
        case imageBytes = "image_bytes"
        case imageUniqueBytes = "image_unique_bytes"
        case buildCacheBytes = "build_cache_bytes"
    }
}

/// One classified resource — the payload the interface is built around.
struct Classified: Decodable, Sendable, Identifiable {
    let kind: String
    let name: String
    let tier: String
    let because: String
    let size: UInt64?
    /// What removing this one thing reclaims. Equal to `size` except for an
    /// image, where layers another image also holds are excluded. Sums are
    /// taken over this: a shared base layer counted once per image that sits
    /// on it inflated the reference machine's images from 59.3 GB to 82.5 GB.
    /// A `var`, so it defaults to `nil` and an older helper's output — or a
    /// test fixture — still constructs.
    var exclusiveSize: UInt64?
    let owner: String?
    let provenance: [String]

    var context: String = ""
    /// The figure to add up, falling back to the stack size where the daemon
    /// did not compute the overlap.
    var reclaimable: UInt64? { exclusiveSize ?? size }
    enum CodingKeys: String, CodingKey {
        case kind, name, tier, because, size, owner, provenance
        case exclusiveSize = "exclusive_size"
    }
    var id: String { "\(context):\(kind):\(name)" }
}

enum PJEvent: Sendable {
    case scanStarted(daemon: String, context: String, runtime: String, apiVersion: String)
    case phase(phase: String, done: UInt32, total: UInt32?)
    case resourceFound(kind: String)
    case warning(code: String, message: String)
    case scanFinished(totals: Totals, durationMs: UInt64, stale: Bool)
    case classified(Classified)
    case hostReclaim(dockerReported: UInt64, hostMeasured: UInt64?, confidence: String)
    /// A variant this build does not know about.
    ///
    /// The protocol's own rule is that adding an event is not a breaking
    /// change and consumers must ignore unknown ones. Modelling that as a case
    /// rather than a thrown error is what makes the rule true in practice.
    case progress(stage: String, name: String?, done: UInt32, total: UInt32)
    case unknown(String)
}

/// One line of NDJSON.
struct Envelope: Sendable {
    let v: UInt32
    let seq: UInt64
    let event: PJEvent
}

enum DecodeError: Error {
    case unsupportedProtocol(UInt32)
}

extension Envelope {
    /// Decode one line.
    ///
    /// The event tag is flattened alongside `v`/`seq`/`ts`, so this reads the
    /// tag first and then decodes the rest of the same object.
    static func decode(line: Data) throws -> Envelope? {
        struct Head: Decodable {
            let v: UInt32
            let seq: UInt64
            let event: String
        }
        let decoder = JSONDecoder()
        guard let head = try? decoder.decode(Head.self, from: line) else {
            // Not an envelope — progress noise, or a partial line. Skipping is
            // correct; stderr is where human output lives.
            return nil
        }
        guard head.v == ProtocolVersion.supported else {
            throw DecodeError.unsupportedProtocol(head.v)
        }

        let event: PJEvent
        switch head.event {
        case "scan_started":
            struct P: Decodable {
                let daemon: String
                let context: String
                let runtime: String
                let apiVersion: String
                enum CodingKeys: String, CodingKey {
                    case daemon, context, runtime
                    case apiVersion = "api_version"
                }
            }
            let p = try decoder.decode(P.self, from: line)
            event = .scanStarted(
                daemon: p.daemon, context: p.context, runtime: p.runtime,
                apiVersion: p.apiVersion)

        case "phase":
            struct P: Decodable {
                let phase: String
                let done: UInt32
                let total: UInt32?
            }
            let p = try decoder.decode(P.self, from: line)
            event = .phase(phase: p.phase, done: p.done, total: p.total)

        case "resource_found":
            struct P: Decodable {
                struct R: Decodable { let kind: String }
                let resource: R
            }
            let p = try decoder.decode(P.self, from: line)
            event = .resourceFound(kind: p.resource.kind)

        case "warning":
            struct P: Decodable {
                let code: String
                let message: String
            }
            let p = try decoder.decode(P.self, from: line)
            event = .warning(code: p.code, message: p.message)

        case "scan_finished":
            struct P: Decodable {
                let totals: Totals
                let durationMs: UInt64
                let stale: Bool
                enum CodingKeys: String, CodingKey {
                    case totals, stale
                    case durationMs = "duration_ms"
                }
            }
            let p = try decoder.decode(P.self, from: line)
            event = .scanFinished(totals: p.totals, durationMs: p.durationMs, stale: p.stale)

        case "classified":
            event = .classified(try decoder.decode(Classified.self, from: line))

        case "apply_progress":
            struct P: Decodable { let stage: String; let name: String?; let done: UInt32; let total: UInt32 }
            let p = try decoder.decode(P.self, from: line)
            event = .progress(stage: p.stage, name: p.name, done: p.done, total: p.total)

        case "host_reclaim":
            struct P: Decodable {
                let dockerReported: UInt64
                let hostMeasured: UInt64?
                let confidence: String
                enum CodingKeys: String, CodingKey {
                    case dockerReported = "docker_reported"
                    case hostMeasured = "host_measured"
                    case confidence
                }
            }
            let p = try decoder.decode(P.self, from: line)
            event = .hostReclaim(
                dockerReported: p.dockerReported, hostMeasured: p.hostMeasured,
                confidence: p.confidence)

        default:
            event = .unknown(head.event)
        }

        return Envelope(v: head.v, seq: head.seq, event: event)
    }
}

/// Decimal byte formatting, matching what the Rust side reports.
///
/// Docker reports decimal MB/GB. Using a binary formatter here would put the
/// app ~5% out of step with the CLI for no reason.
func humanBytes(_ n: UInt64) -> String {
    let units = ["B", "kB", "MB", "GB", "TB"]
    var v = Double(n)
    var u = 0
    while v >= 1000, u < units.count - 1 {
        v /= 1000
        u += 1
    }
    if u == 0 { return "\(n) B" }
    return v >= 100
        ? String(format: "%.0f %@", v, units[u])
        : String(format: "%.1f %@", v, units[u])
}

/// Normalize engine messages at the presentation boundary.
func displayText(_ text: String) -> String {
    text.replacingOccurrences(of: "—", with: "; ")
}
