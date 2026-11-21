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

/// A stand-in `sb` whose chosen commands wait until the test lets them finish, or until their task is cancelled.
/// It records which commands started and which were stopped by a cancel.
final class GatedRunner: CommandRunning, @unchecked Sendable {
    let base: FakeRunner
    private let lock = NSLock()
    private var held: Set<[String]>
    private var released: Set<[String]> = []
    private var waiting: [UUID: ([String], CheckedContinuation<Void, Never>)] = [:]
    private var stoppedBeforeWaiting: Set<UUID> = []
    private var startedCommands: [[String]] = []
    private var cancelledCommands: [[String]] = []
    var started: [[String]] { lock.withLock { startedCommands } }
    var cancelled: [[String]] { lock.withLock { cancelledCommands } }

    init(holding: [ControlCommand], fixture: String = "window-synthetic.json") throws {
        base = try FakeRunner(fixture: fixture)
        held = Set(holding.map(\.arguments))
    }

    func run(_ command: ControlCommand) async throws -> CommandResult {
        let id = UUID()
        let mustWait = lock.withLock { () -> Bool in
            startedCommands.append(command.arguments)
            return held.contains(command.arguments) && !released.contains(command.arguments)
        }
        if mustWait {
            await withTaskCancellationHandler {
                await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                    let now = lock.withLock { () -> Bool in
                        if released.contains(command.arguments) || stoppedBeforeWaiting.contains(id) { return true }
                        waiting[id] = (command.arguments, continuation)
                        return false
                    }
                    if now { continuation.resume() }
                }
            } onCancel: {
                let continuation = lock.withLock { () -> CheckedContinuation<Void, Never>? in
                    stoppedBeforeWaiting.insert(id)
                    return waiting.removeValue(forKey: id)?.1
                }
                continuation?.resume()
            }
            if Task.isCancelled {
                lock.withLock { cancelledCommands.append(command.arguments) }
                throw CommandFailure.stopped
            }
        }
        return try await base.run(command)
    }

    /// Lets every waiting (and later) run of `command` finish.
    func release(_ command: ControlCommand) {
        let ready = lock.withLock { () -> [CheckedContinuation<Void, Never>] in
            released.insert(command.arguments)
            let matching = waiting.filter { $0.value.0 == command.arguments }
            for key in matching.keys { waiting[key] = nil }
            return matching.values.map(\.1)
        }
        for continuation in ready { continuation.resume() }
    }

    /// Waits (up to two seconds) until `command` has started.
    func waitUntilStarted(_ command: ControlCommand) async throws {
        for _ in 0..<400 where !started.contains(command.arguments) { try await Task.sleep(for: .milliseconds(5)) }
        guard started.contains(command.arguments) else { throw CommandFailure("\(command.display) never started") }
    }
    /// Waits (up to two seconds) until a run of `command` was stopped by a cancel.
    func waitUntilCancelled(_ command: ControlCommand) async throws {
        for _ in 0..<400 where !cancelled.contains(command.arguments) { try await Task.sleep(for: .milliseconds(5)) }
        guard cancelled.contains(command.arguments) else { throw CommandFailure("\(command.display) was never stopped") }
    }
}

/// The machines with every GPU box asleep.
func asleep(_ cards: [MachineCard]) -> [MachineCard] {
    cards.map { card in
        guard card.kind == .gpuBox else { return card }
        return MachineCard(machine: card.machine, kind: card.kind, title: card.title, power: .asleep, powerWord: "Asleep",
                           powerOffIn: nil, uptime: nil, gpuUser: nil, busy: 0, guardBusy: false, holds: [], loaded: [],
                           problems: [], wakeSeconds: 68, lanes: card.lanes)
    }
}

/// The synthetic status with every GPU box's power set to `power` ("on", "off"), as `sb ai status --json` prints it.
func syntheticStatus(power: String) throws -> String {
    var json = try Fixture.json("window-synthetic.json")["status"]
    if case .object(var root) = json, case .array(let machines) = root["machines"] {
        root["machines"] = .array(machines.map { row in
            guard case .object(var fields) = row, fields["machine"] == .string("gpu-box") else { return row }
            fields["power"] = .string(power)
            return .object(fields)
        })
        json = .object(root)
    }
    return String(decoding: try Fixture.data(json), as: UTF8.self)
}

/// Waits (up to two seconds) until `condition` holds.
@MainActor func eventually(_ condition: () -> Bool) async throws {
    for _ in 0..<400 where !condition() { try await Task.sleep(for: .milliseconds(5)) }
}
