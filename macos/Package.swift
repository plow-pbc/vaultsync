// swift-tools-version:5.9
import PackageDescription

// The engine is the Rust crate crates/ffi, built as a universal static
// library into lib/ (build.sh does that, or copies one in).
let package = Package(
    name: "VaultSyncBar",
    platforms: [.macOS(.v14)],
    targets: [
        .target(name: "CVaultSync"),
        .executableTarget(
            name: "VaultSyncBar",
            dependencies: ["CVaultSync"],
            linkerSettings: [
                .unsafeFlags(["-Llib", "-lvaultsync_ffi"]),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreFoundation"),
                .linkedLibrary("resolv"),
                .linkedLibrary("iconv"),
            ]
        ),
    ]
)
