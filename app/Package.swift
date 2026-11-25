// swift-tools-version:5.10
// macOS app shell. Build with app/build.sh (builds the Rust core first, then bundles PhotoEditor.app).
import PackageDescription

let rustLib = Context.packageDirectory + "/../target/release"

let package = Package(
    name: "PhotoEditor",
    platforms: [.macOS(.v14)],
    targets: [
        .systemLibrary(name: "CPeFFI", path: "Sources/CPeFFI"),
        .executableTarget(
            name: "PhotoEditor",
            dependencies: ["CPeFFI"],
            path: "Sources/PhotoEditor",
            linkerSettings: [
                .unsafeFlags(["-L", rustLib]),
                .linkedLibrary("pe_ffi"),
                .linkedLibrary("iconv"),
                .linkedFramework("CoreFoundation"),
            ]
        ),
    ],
    swiftLanguageVersions: [.v5]
)
