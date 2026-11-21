import Foundation
import Observation

/// A button's state after a click: its spinner, then one result line.
public enum ActionFeedback: Sendable, Equatable {
    case running
    case done(String)
    case failed(String)
    public var line: String? {
        switch self {
        case .running: nil
        case .done(let line), .failed(let line): line
        }
    }
}

public enum ChatState: Sendable, Equatable {
    case running
    case answered(ChatAnswer)
    case failed(String)
}

/// Everything the window reads through `sb`, and every action it runs. Reads refresh about every 10 seconds
/// while the window is visible and stop while it is hidden; no read ever wakes a machine.
@MainActor @Observable public final class ControlStore {
    public private(set) var status: AIStatus?
    public private(set) var catalog: ModelCatalog?
    public private(set) var engines: EnginesReport?
    public private(set) var doctor: DoctorReport?
    /// What `sb ai settings NAME` answered, by name.
    public private(set) var settings: [String: SettingsView] = [:]
    /// What `sb ai settings TARGET --history` answered, by target.
    public private(set) var histories: [String: SettingsHistory] = [:]
    /// Why the last status read failed, as one plain line; the last good data stays on screen.
    public private(set) var readError: String?
    public private(set) var lastRead: Date?
    public private(set) var reading = false
    public private(set) var feedback: [String: ActionFeedback] = [:]
    public private(set) var chats: [String: ChatState] = [:]
    public private(set) var visible = false
    /// The model whose inspector is open.
    public var selectedModel: String? {
        didSet { if selectedModel != oldValue, visible { Task { await readSelected() } } }
    }

    /// Reads at most this often.
    public static let interval: TimeInterval = 10
    static let doctorEvery: TimeInterval = 30, modelsEvery: TimeInterval = 60, sweepEvery: TimeInterval = 300

    @ObservationIgnored let runner: any CommandRunning
    @ObservationIgnored let clock: @Sendable () -> Date
    @ObservationIgnored let sleep: @Sendable (TimeInterval) async throws -> Void
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private(set) var lastTick: Date?
    @ObservationIgnored private var lastDoctor: Date?
    @ObservationIgnored private var lastModels: Date?
    @ObservationIgnored private var lastSweep: Date?

    public init(runner: any CommandRunning, clock: @escaping @Sendable () -> Date = { Date() },
                sleep: @escaping @Sendable (TimeInterval) async throws -> Void = { try await Task.sleep(for: .seconds($0)) }) {
        self.runner = runner; self.clock = clock; self.sleep = sleep
    }

    // MARK: Derived

    public var now: Date { clock() }
    public var machines: [MachineCard] { Machines.cards(status: status, engines: engines, doctor: doctor, now: now) }
    public var rows: [ModelRow] {
        ModelList.rows(catalog: catalog, engines: engines, doctor: doctor, machines: machines, settings: settings)
    }
    /// Anything wrong, one plain line each: the gateway first, then each machine.
    public var problems: [Problem] {
        (readError.map { [Problem(text: $0, serious: true)] } ?? []) + machines.flatMap(\.problems)
    }

    // MARK: Refresh

    /// The window shows (true) or hides, minimizes or closes (false). Reading pauses while it is not visible.
    public func setVisible(_ visible: Bool) {
        guard visible != self.visible else { return }
        self.visible = visible
        loop?.cancel()
        loop = nil
        guard visible else { return }
        loop = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                let wait = self.nextReadDelay()
                if wait > 0 {
                    do { try await self.sleep(wait) } catch { return }
                }
                guard !Task.isCancelled else { return }
                await self.tick()
            }
        }
    }

    /// Seconds before the next read may start: never sooner than `interval` after the last one began.
    public func nextReadDelay() -> TimeInterval {
        guard let lastTick else { return 0 }
        return max(0, ControlStore.interval - now.timeIntervalSince(lastTick))
    }

    /// One round of reads: status and engines every time; the speed report, the model list and the settings of
    /// every model less often; the open model's settings and history every time.
    public func tick() async {
        let started = now
        lastTick = started
        reading = true
        defer { reading = false }
        let wantDoctor = lastDoctor.map { started.timeIntervalSince($0) >= ControlStore.doctorEvery } ?? true
        let wantModels = lastModels.map { started.timeIntervalSince($0) >= ControlStore.modelsEvery } ?? true
        async let statusRead = read(Commands.status)
        async let enginesRead = read(Commands.engines)
        async let doctorRead = wantDoctor ? read(Commands.doctor()) : nil
        async let modelsRead = wantModels ? read(Commands.models) : nil
        let (statusJSON, enginesJSON, doctorJSON, modelsJSON) = await (statusRead, enginesRead, doctorRead, modelsRead)
        switch statusJSON {
        case .some(.success(let json)): status = AIStatus(json); readError = nil
        case .some(.failure(let failure)): readError = "Can’t read the gateway · " + failure.message
        case .none: break
        }
        if case .some(.success(let json)) = enginesJSON { engines = EnginesReport(json) }
        if case .some(.success(let json)) = doctorJSON { doctor = DoctorReport(json); lastDoctor = started }
        if case .some(.success(let json)) = modelsJSON { catalog = ModelCatalog(json); lastModels = started }
        lastRead = started
        await readSelected()
        if lastSweep.map({ started.timeIntervalSince($0) >= ControlStore.sweepEvery }) ?? true {
            lastSweep = started
            await sweepSettings()
        }
    }

    /// The open model's settings, and the history of each model behind it.
    public func readSelected() async {
        guard let name = selectedModel else { return }
        if case .success(let json) = await read(Commands.settings(name)) {
            settings[name] = SettingsView(json)
        }
        for target in settings[name]?.targets.map(\.target) ?? [] {
            if case .success(let json) = await read(Commands.history(target)) { histories[target] = SettingsHistory(json) }
        }
    }

    /// Reads the settings of every text and audio name, three at a time: which names are routes, where they run,
    /// and which have saved settings.
    func sweepSettings() async {
        let names = (catalog?[.text] ?? []) + (catalog?[.audio] ?? [])
        let runner = self.runner
        await withTaskGroup(of: (String, JSONValue?).self) { group in
            var pending = names.makeIterator()
            func next() {
                guard let name = pending.next() else { return }
                group.addTask {
                    guard let result = try? await runner.run(Commands.settings(name)), result.succeeded else { return (name, nil) }
                    return (name, try? JSONValue.parse(result.stdout))
                }
            }
            for _ in 0..<3 { next() }
            for await (name, json) in group {
                if let json { settings[name] = SettingsView(json) }
                next()
            }
        }
    }

    enum ReadResult { case success(JSONValue), failure(CommandFailure) }
    private func read(_ command: ControlCommand) async -> ReadResult? {
        do {
            let result = try await runner.run(command)
            guard result.succeeded else { return .failure(CommandFailure(Outcome.errorLine(result))) }
            return .success(try JSONValue.parse(result.stdout))
        } catch let failure as CommandFailure {
            return .failure(failure)
        } catch {
            return .failure(CommandFailure("sb sent an answer the window cannot read"))
        }
    }

    // MARK: Actions

    /// Runs one action. An action with a question runs only once the owner said yes (`confirmed`); a disabled
    /// action, or one already running, does nothing. Returns what the button then shows.
    @discardableResult
    public func perform(_ plan: ActionPlan, confirmed: Bool = false) async -> ActionFeedback? {
        guard plan.gate.isAllowed, plan.command.writes, plan.confirmation == nil || confirmed,
              feedback[plan.id] != .running else { return nil }
        feedback[plan.id] = .running
        let outcome = await run(plan.command)
        feedback[plan.id] = outcome
        await reread(after: plan.command)
        return outcome
    }

    /// Runs a change plan's commands in order under one button; stops at the first that fails.
    @discardableResult
    public func apply(_ plan: ChangePlan, id: String) async -> ActionFeedback? {
        guard !plan.isEmpty, feedback[id] != .running else { return nil }
        feedback[id] = .running
        var lines: [String] = []
        var outcome: ActionFeedback = .done("Nothing to change")
        for action in plan.actions {
            outcome = await run(action.command)
            if case .done(let line) = outcome { lines.append(line) } else { break }
        }
        if case .done = outcome, let last = lines.last { outcome = .done(last) }
        feedback[id] = outcome
        if let first = plan.actions.first { await reread(after: first.command) }
        return outcome
    }

    /// "Try it": one request; the answer shows with its speed and the model that answered.
    public func send(_ plan: ActionPlan, model: String, confirmed: Bool = false) async {
        guard plan.gate.isAllowed, plan.confirmation == nil || confirmed, chats[model] != .running else { return }
        chats[model] = .running
        do {
            let result = try await runner.run(plan.command)
            if result.succeeded, let json = try? JSONValue.parse(result.stdout) {
                chats[model] = .answered(ChatAnswer(json))
            } else {
                chats[model] = .failed(result.succeeded ? "sb sent an answer the window cannot read" : Outcome.errorLine(result))
            }
        } catch let failure as CommandFailure {
            chats[model] = .failed(failure.message)
        } catch {
            chats[model] = .failed("Could not run sb")
        }
    }

    public func clearFeedback(_ id: String) { feedback[id] = nil }

    private func run(_ command: ControlCommand) async -> ActionFeedback {
        do {
            let result = try await runner.run(command)
            return result.succeeded ? .done(Outcome.successLine(command, stdout: result.stdout)) : .failed(Outcome.errorLine(result))
        } catch let failure as CommandFailure {
            return .failed(failure.message)
        } catch {
            return .failed("Could not run sb")
        }
    }

    /// After a write, what it changed is read again at once.
    private func reread(after command: ControlCommand) async {
        let verb = command.arguments.count > 1 ? command.arguments[1] : ""
        if ["hold", "release", "on", "off"].contains(verb) {
            if case .success(let json) = await read(Commands.status) { status = AIStatus(json) }
        } else {
            if case .success(let json) = await read(Commands.engines) { engines = EnginesReport(json) }
            let name = selectedModel ?? (command.arguments.count > 2 ? command.arguments[2] : "")
            if case .success(let json) = await read(Commands.settings(name)) { settings[name] = SettingsView(json) }
            if command.arguments.count > 2, case .success(let json) = await read(Commands.history(command.arguments[2])) {
                histories[command.arguments[2]] = SettingsHistory(json)
            }
        }
    }

    // MARK: Renders

    /// Fills the store from a saved set of answers, without running anything (offscreen renders and tests).
    public func load(_ fixture: ControlFixture) {
        status = fixture.status; catalog = fixture.catalog; engines = fixture.engines; doctor = fixture.doctor
        settings = fixture.settings; histories = fixture.histories; readError = fixture.error
        lastRead = fixture.now; feedback = fixture.feedback; chats = fixture.chats
        selectedModel = fixture.selected
    }
}

/// Saved answers of every read, in one file: `{"now": ISO date, "status": …, "models": …, "engines": …,
/// "doctor": …, "settings": {NAME: …}, "history": {TARGET: …}, "limits": …, "selected": NAME, "error": TEXT}`.
public struct ControlFixture: Sendable {
    public let now: Date
    public let status: AIStatus?
    public let catalog: ModelCatalog?
    public let engines: EnginesReport?
    public let doctor: DoctorReport?
    public let settings: [String: SettingsView]
    public let histories: [String: SettingsHistory]
    public let selected: String?
    public let error: String?
    public let feedback: [String: ActionFeedback]
    public let chats: [String: ChatState]
    /// The raw `sb limits --json` answer, for the account gauges.
    public let limits: Data?

    public init(_ json: JSONValue) {
        let formatter = ISO8601DateFormatter()
        now = json["now"].string.flatMap(formatter.date(from:)) ?? Date()
        status = json["status"].isNull ? nil : AIStatus(json["status"])
        catalog = json["models"].isNull ? nil : ModelCatalog(json["models"])
        engines = json["engines"].isNull ? nil : EnginesReport(json["engines"])
        doctor = json["doctor"].isNull ? nil : DoctorReport(json["doctor"])
        settings = (json["settings"].object ?? [:]).mapValues(SettingsView.init)
        histories = (json["history"].object ?? [:]).mapValues(SettingsHistory.init)
        selected = json["selected"].string
        error = json["error"].string
        var feedback: [String: ActionFeedback] = [:]
        for (id, row) in json["feedback"].object ?? [:] {
            if let line = row["done"].string { feedback[id] = .done(line) }
            else if let line = row["failed"].string { feedback[id] = .failed(line) }
            else if row["running"].bool == true { feedback[id] = .running }
        }
        self.feedback = feedback
        var chats: [String: ChatState] = [:]
        for (model, row) in json["chat"].object ?? [:] {
            if let line = row["failed"].string { chats[model] = .failed(line) } else { chats[model] = .answered(ChatAnswer(row)) }
        }
        self.chats = chats
        limits = json["limits"].isNull ? nil
            : (try? JSONSerialization.data(withJSONObject: JSONFixtureBridge.any(json["limits"]), options: []))
    }
}

enum JSONFixtureBridge {
    static func any(_ value: JSONValue) -> Any {
        switch value {
        case .null: NSNull()
        case .bool(let flag): flag
        case .number(let number): number
        case .string(let text): text
        case .array(let list): list.map(any)
        case .object(let map): map.mapValues(any)
        }
    }
}
