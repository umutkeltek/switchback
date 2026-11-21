import Foundation
@testable import ControlPresentation

/// The synthetic fixtures: neutral names, the exact shapes `sb` prints.
enum Fixture {
    static let directory = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("Fixtures")
    static func json(_ name: String) throws -> JSONValue {
        try JSONValue.parse(Data(contentsOf: directory.appendingPathComponent(name)))
    }
    static func data(_ value: JSONValue) throws -> Data {
        try JSONSerialization.data(withJSONObject: JSONFixtureBridge.any(value), options: [.fragmentsAllowed])
    }
    static var synthetic: ControlFixture { get throws { ControlFixture(try json("window-synthetic.json")) } }
    static var worst: ControlFixture { get throws { ControlFixture(try json("window-worst.json")) } }
    static var now: Date { ISO8601DateFormatter().date(from: "2026-01-15T12:00:00Z")! }
}

/// A stand-in for `sb`: answers reads from the synthetic fixture and records every command it is asked to run.
final class FakeRunner: CommandRunning, @unchecked Sendable {
    private let lock = NSLock()
    private var recorded: [ControlCommand] = []
    private var answers: [[String]: CommandResult] = [:]
    var calls: [ControlCommand] { lock.withLock { recorded } }

    init(fixture: String = "window-synthetic.json") throws {
        let json = try Fixture.json(fixture)
        func answer(_ command: ControlCommand, _ value: JSONValue) throws {
            answers[command.arguments] = CommandResult(status: 0, stdout: try Fixture.data(value))
        }
        try answer(Commands.status, json["status"])
        try answer(Commands.models, json["models"])
        try answer(Commands.engines, json["engines"])
        try answer(Commands.doctor(), json["doctor"])
        for (name, view) in json["settings"].object ?? [:] { try answer(Commands.settings(name), view) }
        for (target, history) in json["history"].object ?? [:] { try answer(Commands.history(target), history) }
    }
    func answer(_ command: ControlCommand, status: Int32 = 0, stdout: String = "", stderr: String = "") {
        lock.withLock { answers[command.arguments] = CommandResult(status: status, stdout: Data(stdout.utf8), stderr: stderr) }
    }
    func run(_ command: ControlCommand) async throws -> CommandResult {
        lock.withLock {
            recorded.append(command)
            return answers[command.arguments] ?? CommandResult(status: 1, stdout: Data(), stderr: "sb ai: no fake answer\n")
        }
    }
}

extension ControlFixture {
    var machines: [MachineCard] { Machines.cards(status: status, engines: engines, doctor: doctor, now: now) }
    func target(_ name: String, _ index: Int = 0) -> TargetSettings? { settings[name]?.targets[index] }
}
