// swift-tools-version: 6.0
import PackageDescription

// Optional, explicitly built component of CLI 2.0. The Rust workspace builds and
// tests without it; see the repository README.
let package = Package(
    name: "CLI2AppleFM",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "cli2-apple-fm", targets: ["CLI2AppleFM"])
    ],
    targets: [
        .executableTarget(
            name: "CLI2AppleFM",
            path: "Sources/CLI2AppleFM",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "CLI2AppleFMTests",
            dependencies: ["CLI2AppleFM"],
            path: "Tests/CLI2AppleFMTests",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
