// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "PruneJuice",
    platforms: [.macOS(.v14)],
    dependencies: [
        // Sparkle ships as a binary XCFramework through SPM. `scripts/bundle.sh`
        // copies the macOS slice into Contents/Frameworks and signs it there,
        // because a hand-assembled bundle gets none of Xcode's copy phases.
        .package(url: "https://github.com/sparkle-project/Sparkle", from: "2.9.0")
    ],
    targets: [
        .executableTarget(
            name: "PruneJuice",
            dependencies: [.product(name: "Sparkle", package: "Sparkle")],
            path: "Sources/PruneJuice",
            linkerSettings: [
                // Without this the app launches and immediately dies looking
                // for @rpath/Sparkle.framework. Xcode adds it for you; a
                // `swift build` does not.
                .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])
            ]
        ),
        .testTarget(name: "PruneJuiceTests", dependencies: ["PruneJuice"])
    ]
)
