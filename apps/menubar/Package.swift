// swift-tools-version: 6.2
import PackageDescription
let package = Package(
    name: "SwitchbackBar",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "SwitchbackBar", targets: ["SwitchbackBar"])],
    targets: [
        .target(name: "UsagePresentation"),
        // The Switchback window's rules: what `sb` said, what may be done and why not, what wakes the GPU box.
        .target(name: "ControlPresentation"),
        .executableTarget(name: "SwitchbackBar", dependencies: ["UsagePresentation", "ControlPresentation"],
                          swiftSettings: [.defaultIsolation(MainActor.self)]),
        .testTarget(name: "UsagePresentationTests", dependencies: ["UsagePresentation"]),
        .testTarget(name: "ControlPresentationTests", dependencies: ["ControlPresentation"], exclude: ["Fixtures"])
    ]
)
