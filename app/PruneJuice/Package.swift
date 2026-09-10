// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "PruneJuice",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "PruneJuice",
            path: "Sources/PruneJuice"
        ),
        .testTarget(name: "PruneJuiceTests", dependencies: ["PruneJuice"])
    ]
)
