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
    /// When the last status read that succeeded began: the age of the power the window shows.
    public private(set) var statusReadAt: Date?
    /// The last status read failed, so the power on screen may be old.
    public private(set) var statusFailed = false
    /// The window came back from hidden and no status read has finished since.
    public private(set) var awaitingFresh = false
    public private(set) var reading = false
    public private(set) var feedback: [String: ActionFeedback] = [:]
    public private(set) var chats: [String: ChatState] = [:]
    public private(set) var visible = false
    /// The model whose inspector is open. Picking another reads its settings and history once the pick has
    /// held for `debounce` seconds; a read still running for the earlier pick is stopped.
    public var selectedModel: String? {
        didSet { if selectedModel != oldValue, visible { scheduleSelectionRead() } }
    }

    /// Reads at most this often.
    public static let interval: TimeInterval = 10
    /// Power read longer ago than this counts as unknown for anything that could wake a machine.
    public static let powerTrust: TimeInterval = 15
    static let doctorEvery: TimeInterval = 30, modelsEvery: TimeInterval = 60, sweepEvery: TimeInterval = 300
    /// While every GPU box is known to be asleep, the engines are read only this often.
    static let enginesAsleepEvery: TimeInterval = 60

    @ObservationIgnored let runner: any CommandRunning
    @ObservationIgnored let clock: @Sendable () -> Date
    @ObservationIgnored let sleep: @Sendable (TimeInterval) async throws -> Void
    /// How long a model pick must hold before its settings are read.
    @ObservationIgnored let debounce: TimeInterval
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored private(set) var lastTick: Date?
    @ObservationIgnored private var lastDoctor: Date?
    @ObservationIgnored private var lastModels: Date?
    @ObservationIgnored private var lastEngines: Date?
    @ObservationIgnored private var lastSweep: Date?
    @ObservationIgnored private var visibleSince: Date?
    /// The one read of the open model's settings and history; a new pick cancels it.
    @ObservationIgnored private(set) var selectionTask: Task<Void, Never>?
    /// The five-minute read of every name's settings, run beside the status reads, never in their way.
    @ObservationIgnored private(set) var sweepTask: Task<Void, Never>?

    public init(runner: any CommandRunning, clock: @escaping @Sendable () -> Date = { Date() },
                sleep: @escaping @Sendable (TimeInterval) async throws -> Void = { try await Task.sleep(for: .seconds($0)) },
                debounce: TimeInterval = 0.3) {
        self.runner = runner; self.clock = clock; self.sleep = sleep; self.debounce = debounce
    }

    // MARK: Derived

    public var now: Date { clock() }
    /// How far the power on screen can be trusted: its age, a failed read, a read still due after the window was hidden.
    public var powerEvidence: PowerEvidence {
        PowerEvidence(readAt: statusReadAt, failed: statusFailed, awaitingFresh: awaitingFresh)
    }
    public var machines: [MachineCard] {
        Machines.cards(status: status, engines: engines, doctor: doctor, now: now, evidence: powerEvidence)
    }
    public var rows: [ModelRow] {
        ModelList.rows(catalog: catalog, engines: engines, doctor: doctor, machines: machines, settings: settings)
    }
    /// Anything wrong, one plain line each: the gateway first, then each machine.
    public var problems: [Problem] {
        (readError.map { [Problem(text: $0, serious: true)] } ?? []) + machines.flatMap(\.problems)
    }

    /// "Try it" for a model, from the power as it is now: a click asks first when the power has gone stale
    /// since the button was drawn.
    public func tryIt(_ row: ModelRow, prompt: String) -> ActionPlan {
        Inspector.tryIt(row.name, targets: row.targets, targetsKnown: row.known, prompt: prompt, machines: machines)
    }
    /// "Keep on" for a machine, from the power as it is now.
    public func keepOn(_ machine: String, duration: String = Machines.keepOnChoices[0]) -> ActionPlan? {
        machines.first { $0.machine == machine }.flatMap { Machines.keepOn($0, duration: duration) }
    }

    // MARK: Refresh

    /// The window shows (true) or hides, minimizes or closes (false). Reading pauses while it is not visible.
    public func setVisible(_ visible: Bool) {
        guard visible != self.visible else { return }
        self.visible = visible
        loop?.cancel()
        loop = nil
        guard visible else {
            // Nothing reads while hidden; a read cut short this way is not a gateway problem.
            selectionTask?.cancel()
            selectionTask = nil
            sweepTask?.cancel()
            return
        }
        // What the window shows was read before it hid: until a new status read finishes, the power may be old.
        visibleSince = now
        awaitingFresh = true
        if selectedModel != nil { scheduleSelectionRead() }
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

    /// ⌘R: reads at once when the last read began at least `interval` ago; never sooner.
    public func readNow() async {
        guard !reading, nextReadDelay() == 0 else { return }
        await tick()
    }

    /// One round of reads, all at once: status every time; the engines every time while a GPU box may be on (once
    /// a minute while every box is asleep); the speed report and the model list less often; the open model's
    /// settings when no read for it is running. The five-minute read of every name's settings starts beside it.
    public func tick() async {
        let started = now
        lastTick = started
        reading = true
        defer { reading = false }
        let wantDoctor = lastDoctor.map { started.timeIntervalSince($0) >= ControlStore.doctorEvery } ?? true
        let wantModels = lastModels.map { started.timeIntervalSince($0) >= ControlStore.modelsEvery } ?? true
        let wantEngines = enginesDue(at: started)
        let open = selectionTask == nil ? selectedModel : nil
        async let statusRead = read(Commands.status)
        async let enginesRead = wantEngines ? read(Commands.engines) : nil
        async let doctorRead = wantDoctor ? read(Commands.doctor()) : nil
        async let modelsRead = wantModels ? read(Commands.models) : nil
        async let openRead = open != nil ? read(Commands.settings(open ?? "")) : nil
        let (statusJSON, enginesJSON, doctorJSON, modelsJSON, openJSON) = await (statusRead, enginesRead, doctorRead, modelsRead, openRead)
        // Hidden mid-read: what was stopped says nothing about the gateway.
        if Task.isCancelled && statusJSON == nil { return }
        switch statusJSON {
        case .some(.success(let json)): applyStatus(json, readAt: started)
        case .some(.failure(let failure)):
            readError = "Can’t read the gateway · " + failure.message
            statusFailed = true
        case .none: break
        }
        if case .some(.success(let json)) = enginesJSON { engines = EnginesReport(json); lastEngines = started }
        if case .some(.success(let json)) = doctorJSON { doctor = DoctorReport(json); lastDoctor = started }
        if case .some(.success(let json)) = modelsJSON { catalog = ModelCatalog(json); lastModels = started }
        if let open, open == selectedModel, case .some(.success(let json)) = openJSON { settings[open] = SettingsView(json) }
        lastRead = started
        startSweepIfDue(at: started)
    }

    private func applyStatus(_ json: JSONValue, readAt started: Date) {
        status = AIStatus(json)
        readError = nil
        statusFailed = false
        statusReadAt = max(started, statusReadAt ?? started)
        if let since = visibleSince, started >= since { awaitingFresh = false }
    }

    /// The engines are read every round while any GPU box may be on; once a minute while every one is known to be
    /// asleep, because then nothing on them changes (the read may still check an engine's health).
    func enginesDue(at time: Date) -> Bool {
        guard let last = lastEngines else { return true }
        let boxes = machines.filter { $0.kind == .gpuBox }
        let allAsleep = !statusFailed && !boxes.isEmpty && boxes.allSatisfy { $0.power == .asleep }
        return !allAsleep || time.timeIntervalSince(last) >= ControlStore.enginesAsleepEvery
    }

    private func startSweepIfDue(at time: Date) {
        guard visible || loop == nil, sweepTask == nil,
              lastSweep.map({ time.timeIntervalSince($0) >= ControlStore.sweepEvery }) ?? true else { return }
        lastSweep = time
        sweepTask = Task { [weak self] in
            await self?.sweepSettings()
            guard let self else { return }
            // Cut short by hiding: the next round starts it again.
            if Task.isCancelled { self.lastSweep = nil }
            self.sweepTask = nil
        }
    }

    /// Reads the open model's settings and the history of each model behind it, at once (no debounce).
    public func readSelected() async {
        guard let name = selectedModel else { return }
        await readSelection(name)
    }

    private func scheduleSelectionRead() {
        selectionTask?.cancel()
        selectionTask = nil
        guard let name = selectedModel else { return }
        let wait = debounce
        selectionTask = Task { [weak self] in
            do { try await Task.sleep(for: .seconds(wait)) } catch { return }
            guard let self, !Task.isCancelled else { return }
            await self.readSelection(name)
            // A cancelled read was replaced by a newer pick; only the current one clears its slot.
            if !Task.isCancelled { self.selectionTask = nil }
        }
    }

    private func readSelection(_ name: String) async {
        guard case .success(let json) = await read(Commands.settings(name)), !Task.isCancelled, selectedModel == name else { return }
        settings[name] = SettingsView(json)
        for target in settings[name]?.targets.map(\.target) ?? [] {
            let answer = await read(Commands.history(target))
            guard !Task.isCancelled else { return }
            if case .success(let json) = answer { histories[target] = SettingsHistory(json) }
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
                guard !Task.isCancelled, let name = pending.next() else { return }
                group.addTask {
                    guard let result = try? await runner.run(Commands.settings(name)), result.succeeded else { return (name, nil) }
                    return (name, try? JSONValue.parse(result.stdout))
                }
            }
            for _ in 0..<3 { next() }
            for await (name, json) in group {
                if let json, !Task.isCancelled { settings[name] = SettingsView(json) }
                next()
            }
        }
    }

    enum ReadResult { case success(JSONValue), failure(CommandFailure) }
    /// One read. Nil when it was stopped (the window hid, or another model was picked): that is no failure.
    private func read(_ command: ControlCommand) async -> ReadResult? {
        do {
            let result = try await runner.run(command)
            guard result.succeeded else { return Task.isCancelled ? nil : .failure(CommandFailure(Outcome.errorLine(result))) }
            return .success(try JSONValue.parse(result.stdout))
        } catch let failure as CommandFailure {
            return failure.cancelled || Task.isCancelled ? nil : .failure(failure)
        } catch {
            return Task.isCancelled ? nil : .failure(CommandFailure("sb sent an answer the window cannot read"))
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

    /// "Try it": one request; the answer shows with its speed and the model that answered. A plan with a
    /// question (it may wake the GPU box) runs only once the owner said yes.
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
        let started = now
        if ["hold", "release", "on", "off"].contains(verb) {
            if case .success(let json) = await read(Commands.status) { applyStatus(json, readAt: started) }
        } else {
            if case .success(let json) = await read(Commands.engines) { engines = EnginesReport(json); lastEngines = started }
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
        statusReadAt = fixture.status == nil ? nil : fixture.now.addingTimeInterval(-fixture.statusAge)
        statusFailed = fixture.error != nil
        awaitingFresh = false
        selectedModel = fixture.selected
    }
}

/// Saved answers of every read, in one file: `{"now": ISO date, "status": …, "models": …, "engines": …,
/// "doctor": …, "settings": {NAME: …}, "history": {TARGET: …}, "limits": …, "selected": NAME, "error": TEXT,
/// "status_age": SECONDS}`.
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
    /// How old the status was when the answers were saved, in seconds ("status_age"); 0 when not given.
    public let statusAge: TimeInterval
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
        statusAge = json["status_age"].double ?? 0
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
