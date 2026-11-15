// swift-tools-version: 6.2
import PackageDescription
let package = Package(
    name: "SwitchbackBar",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "SwitchbackBar", targets: ["SwitchbackBar"])],
    targets: [
        .target(name: "UsagePresentation"),
        .executableTarget(name: "SwitchbackBar", dependencies: ["UsagePresentation"],
                          swiftSettings: [.defaultIsolation(MainActor.self)]),
        .testTarget(name: "UsagePresentationTests", dependencies: ["UsagePresentation"])
    ]
)
