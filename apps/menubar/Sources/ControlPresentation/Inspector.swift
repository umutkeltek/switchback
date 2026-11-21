import Foundation

/// Where a value comes from, as a small badge.
public enum SourceBadge: Sendable, Equatable {
    case engine
    case saved
    case profile(String)
    case locked
    public var word: String {
        switch self {
        case .engine: "engine default"
        case .saved: "saved for this model"
        case .profile(let name): "profile \(name)"
        case .locked: "locked"
        }
    }
    public var symbol: String {
        switch self {
        case .engine: "gearshape"
        case .saved: "checkmark.circle"
        case .profile: "person.crop.rectangle"
        case .locked: "lock.fill"
        }
    }
    /// The badge for a setting's source: a lock wins, then a profile, then saved, then the engine.
    public static func of(source: String, locked: Bool) -> SourceBadge {
        if locked { return .locked }
        if source.hasPrefix("profile:") { return .profile(String(source.dropFirst("profile:".count))) }
        if source.hasPrefix("route_profile:") { return .profile(String(source.dropFirst("route_profile:".count))) }
        if source == "defaults" { return .saved }
        return .engine
    }
}

/// One setting in the inspector: what it is now, where that comes from, and whether it may change here.
public struct SettingState: Sendable, Equatable, Identifiable {
    public var id: String { key }
    /// The key as the draft and `sb ai set` name it: "temperature", "reasoning.enabled",
    /// "reasoning_guard.min_tokens", "load.ctx_size".
    public let key: String
    public let title: String
    /// The value set for the model; nil when the engine decides.
    public let value: JSONValue?
    /// What the control shows when nothing is set: the engine's own value when the window can see it.
    public let shown: JSONValue?
    public let badge: SourceBadge
    public let gate: Gate
}

/// A change waiting for "Apply".
public enum DraftValue: Sendable, Equatable {
    case set(JSONValue)
    /// Remove what is saved: the engine's own value applies again.
    case reset
}
public struct SettingsDraft: Sendable, Equatable {
    public var values: [String: DraftValue] = [:]
    public var apply: ApplyMode = .nextLoad
    public init(values: [String: DraftValue] = [:], apply: ApplyMode = .nextLoad) { self.values = values; self.apply = apply }
    public var isEmpty: Bool { values.isEmpty }
}

public struct ChangeLine: Sendable, Equatable, Identifiable {
    public var id: String { key }
    public let key: String
    public let title: String
    public let from: String
    public let to: String
}
/// What "Apply" will do, shown before it runs.
public struct ChangePlan: Sendable, Equatable {
    public let lines: [ChangeLine]
    /// The commands, in order.
    public let actions: [ActionPlan]
    /// "Takes effect on the next answer", "Takes effect on next load", "Reloads the model now".
    public let notes: [String]
    public let reloads: Bool
    public var isEmpty: Bool { lines.isEmpty }
}

public struct FitHint: Sendable, Equatable {
    public enum Verdict: String, Sendable { case fits, tight, wontFit, unknown }
    public let verdict: Verdict
    public let neededBytes: Double?
    public let budgetBytes: Double?
    public let text: String
}

public struct VersionRow: Sendable, Equatable, Identifiable {
    public var id: Int { revision }
    public let revision: Int
    public let title: String
    public let when: String
    public let what: String
    public let isCurrent: Bool
    public let undo: ActionPlan?
}

public struct FindingRow: Sendable, Equatable, Identifiable {
    public var id: String { kind + text }
    public let kind: String
    /// What was measured, in plain words.
    public let text: String
    /// What to do, without the command.
    public let advice: String
    /// The `sb` command the speed report suggests, for the copy affordance.
    public let command: String?
    /// The suggestion as inspector changes, when it only sets keys the inspector shows.
    public let draft: SettingsDraft?
}

public enum Inspector {
    public static let requestKeys = ["temperature", "top_p", "max_tokens", "reasoning.enabled"]
    public static let guardKeys = ["reasoning_guard.enabled", "reasoning_guard.min_tokens"]
    public static let loadKeys = ["load.ctx_size", "load.cache_type_k", "load.parallel"]
    public static let cacheTypes = ["f16", "q8_0", "q4_0"]
    /// Slider and stepper ranges; a model's own bounds narrow them.
    public static func range(_ key: String, target: TargetSettings? = nil) -> ClosedRange<Double> {
        let base: ClosedRange<Double> = switch key {
        case "temperature": 0...2
        case "top_p": 0...1
        case "max_tokens": 16...131_072
        case "reasoning_guard.min_tokens": 0...32_768
        case "load.ctx_size": 1_024...262_144
        case "load.parallel": 1...16
        default: 0...1
        }
        let bare = key.hasPrefix("load.") ? String(key.dropFirst(5)) : key
        guard let bound = target?.bounds[bare] else { return base }
        return max(base.lowerBound, bound.lowerBound)...min(base.upperBound, max(bound.lowerBound, bound.upperBound))
    }

    static func gate(_ target: TargetSettings, badge: SourceBadge) -> Gate {
        if target.document == nil { return .disabled("This key can only read settings") }
        if case .profile(let name) = badge { return .disabled("Set by the profile \(name)") }
        return .allowed
    }

    /// Answer settings: temperature, top P, max tokens, thinking and the reasoning guard.
    public static func answerSettings(_ target: TargetSettings) -> [SettingState] {
        var rows: [SettingState] = []
        for key in ["temperature", "top_p", "max_tokens"] {
            let effective = target.effective[key] ?? EffectiveSetting(value: .null, source: "engine")
            let badge = SourceBadge.of(source: effective.source, locked: effective.locked)
            rows.append(SettingState(key: key, title: Words.setting(key), value: effective.value.isNull ? nil : effective.value,
                                     shown: nil, badge: badge, gate: gate(target, badge: badge)))
        }
        // Thinking is `reasoning.enabled`; unset, a model marked as one that thinks shows on.
        let reasoning = target.effective["reasoning"] ?? EffectiveSetting(value: .null, source: "engine")
        let enabled = reasoning.value["enabled"]
        let thinks = target.document?["reasoning"].bool ?? false
        let thinkingBadge = enabled.isNull ? SourceBadge.engine : SourceBadge.of(source: reasoning.source, locked: reasoning.locked)
        rows.append(SettingState(key: "reasoning.enabled", title: "Thinking", value: enabled.isNull ? nil : enabled,
                                 shown: .bool(thinks), badge: thinkingBadge, gate: gate(target, badge: thinkingBadge)))
        // The reasoning guard is target-wide: it is active when the model is marked as thinking and the guard is on.
        let guardDoc = target.document?["reasoning_guard"] ?? .null
        let active = thinks && (guardDoc["enabled"].bool ?? false)
        let guardBadge: SourceBadge = guardDoc.isNull ? .engine : .saved
        let guardGate: Gate = target.document == nil ? .disabled("This key can only read settings") : .allowed
        rows.append(SettingState(key: "reasoning_guard.enabled", title: "Reasoning guard",
                                 value: guardDoc.isNull ? nil : .bool(active), shown: .bool(false), badge: guardBadge, gate: guardGate))
        rows.append(SettingState(key: "reasoning_guard.min_tokens", title: "Guard minimum",
                                 value: guardDoc["min_tokens"].isNull ? nil : guardDoc["min_tokens"], shown: .number(1024),
                                 badge: guardDoc["min_tokens"].isNull ? .engine : .saved, gate: guardGate))
        return rows
    }

    /// Whether the window offers load settings, load and unload for a target, and why not.
    /// Never for the Mac; never for an engine that only lists its models.
    public static func loadControl(_ target: TargetSettings, engines: EnginesReport?, machines: [MachineCard]) -> Gate? {
        guard let laneName = target.engineLane ?? target.target.split(separator: "/").first.map(String.init),
              let machine = Machines.machine(forLane: laneName, in: machines) else { return nil }
        guard machine.kind == .gpuBox else { return nil }
        guard let lane = engines?.lane(laneName), lane.acceptsControl, target.engineLane != nil else { return nil }
        return target.document == nil ? .disabled("This key can only read settings") : .allowed
    }

    /// Load settings: context size, KV cache and parallel slots; nil when the window does not offer them.
    public static func loadSettings(_ target: TargetSettings, engines: EnginesReport?, machines: [MachineCard]) -> [SettingState]? {
        guard let control = loadControl(target, engines: engines, machines: machines) else { return nil }
        let model = engines?.model(target.target)?.model
        func row(_ key: String, engine: JSONValue?) -> SettingState {
            let bare = String(key.dropFirst(5))
            let saved = target.loadSettings[bare]?.value
            let locked = target.locked.contains(key)
            let badge: SourceBadge = saved == nil || saved?.isNull == true ? (locked ? .locked : .engine) : (locked ? .locked : .saved)
            return SettingState(key: key, title: Words.setting(key), value: saved.flatMap { $0.isNull ? nil : $0 },
                                shown: engine, badge: badge, gate: control)
        }
        let context = model?.argument(["--ctx-size", "-c"]).flatMap(Int.init).flatMap { $0 > 0 ? $0 : nil }
            ?? model?.memory?.inUse?.context
        return [
            row("load.ctx_size", engine: context.map { .number(Double($0)) }),
            row("load.cache_type_k", engine: .string(model?.argument(["--cache-type-k", "-ctk"]) ?? "f16")),
            row("load.parallel", engine: model?.argument(["--parallel", "-np"]).flatMap(Double.init).map(JSONValue.number)),
        ]
    }

    /// Bytes per cache element, as the gateway counts them.
    public static func cacheTypeBytes(_ type: String) -> Double {
        switch type {
        case "f32": 4
        case "f16", "bf16": 2
        case "f8_e4m3": 1
        case "q8_0": 34.0 / 32
        case "q5_1": 24.0 / 32
        case "q5_0": 22.0 / 32
        case "q4_1": 20.0 / 32
        case "q4_0", "iq4_nl": 18.0 / 32
        default: 2
        }
    }

    /// Whether a context size fits in memory: from the gateway's estimate (the `sb ai fit` numbers it keeps) when it
    /// has one, else from the engine's own measurement; unknown when neither exists.
    public static func fit(context: Int, cacheK: String, cacheV: String? = nil, target: TargetSettings,
                           engineModel: EngineModel?) -> FitHint {
        let cacheV = cacheV ?? cacheK
        if let estimate = target.estimate {
            let valueDim = estimate.valueDim ?? estimate.headDim
            let kv = estimate.kvLayers * Double(context) * estimate.kvHeads
                * (estimate.headDim * cacheTypeBytes(cacheK) + valueDim * cacheTypeBytes(cacheV))
            return verdict(needed: estimate.weightsBytes + kv + target.reserveBytes, budget: estimate.budgetBytes)
        }
        let memory = engineModel?.memory ?? target.memory
        if let low = memory?.minimum, let high = memory?.maximum, let a = low.context, let b = high.context,
           let bytesA = low.bytes, let bytesB = high.bytes, b > a, bytesB >= bytesA {
            let perToken = (bytesB - bytesA) / Double(b - a)
            let measuredType = engineModel?.argument(["--cache-type-k", "-ctk"]) ?? "f16"
            let scale = cacheTypeBytes(cacheK) / cacheTypeBytes(measuredType)
            let needed = bytesA - perToken * Double(a) + perToken * Double(context) * scale
            return FitHint(verdict: .unknown, neededBytes: needed, budgetBytes: nil,
                           text: "About \(Words.bytes(needed)) at \(Words.tokens(context))")
        }
        return FitHint(verdict: .unknown, neededBytes: nil, budgetBytes: nil, text: "Memory need unknown · no measurement yet")
    }
    static func verdict(needed: Double, budget: Double) -> FitHint {
        let ratio = budget > 0 ? needed / budget : 2
        let words = "\(Words.bytes(needed)) of \(Words.bytes(budget))"
        if ratio <= 0.9 { return FitHint(verdict: .fits, neededBytes: needed, budgetBytes: budget, text: "Fits · " + words) }
        if ratio <= 1 { return FitHint(verdict: .tight, neededBytes: needed, budgetBytes: budget, text: "Just fits · " + words) }
        return FitHint(verdict: .wontFit, neededBytes: needed, budgetBytes: budget, text: "Won’t fit · needs " + words)
    }

    /// What "Apply" will change, whether the model must reload, and the exact commands.
    public static func plan(target: TargetSettings, draft: SettingsDraft, engines: EnginesReport?,
                            machines: [MachineCard]) -> ChangePlan {
        let answer = Dictionary(uniqueKeysWithValues: answerSettings(target).map { ($0.key, $0) })
        let load = Dictionary(uniqueKeysWithValues: (loadSettings(target, engines: engines, machines: machines) ?? []).map { ($0.key, $0) })
        var lines: [ChangeLine] = []
        var sets: [(String, JSONValue)] = [], resets: [String] = []
        var loadSets: [(String, JSONValue)] = [], loadResets: [String] = []
        for key in draft.values.keys.sorted() {
            guard let change = draft.values[key] else { continue }
            let state = answer[key] ?? load[key]
            // A key the inspector does not show is left alone (the speed report's suggestions add "reasoning").
            let marked = target.document?["reasoning"]
            let current = state?.value ?? (key == "reasoning" && marked?.isNull == false ? marked : nil)
            let fromWord = current.map { Words.value($0) } ?? (state?.shown.map { "engine default (\(Words.value($0)))" } ?? "engine default")
            switch change {
            case .set(let value):
                if let current, current == value { continue }
                lines.append(ChangeLine(key: key, title: Words.setting(key), from: fromWord, to: Words.value(value)))
                if key.hasPrefix("load.") { loadSets.append((String(key.dropFirst(5)), value)) } else { sets.append((key, value)) }
            case .reset:
                guard current != nil else { continue }
                lines.append(ChangeLine(key: key, title: Words.setting(key), from: fromWord, to: "engine default"))
                if key.hasPrefix("load.") { loadResets.append(String(key.dropFirst(5))) } else { resets.append(key) }
            }
        }
        // Turning the guard on also marks the model as one that thinks, which the guard needs.
        if sets.contains(where: { $0.0 == "reasoning_guard.enabled" && $0.1 == .bool(true) }),
           target.document?["reasoning"].bool != true, !sets.contains(where: { $0.0 == "reasoning" }) {
            sets.append(("reasoning", .bool(true)))
            lines.append(ChangeLine(key: "reasoning", title: "Thinking model", from: "not marked", to: "marked, so the guard works"))
        }
        let model = engines?.model(target.target)?.model
        let loaded = model?.status == "loaded"
        let apply: ApplyMode = loaded ? draft.apply : .nextLoad
        var actions: [ActionPlan] = []
        func add(_ command: ControlCommand) {
            actions.append(ActionPlan(id: "apply:\(target.target):\(actions.count)", title: "Apply", command: command))
        }
        if !sets.isEmpty { add(Commands.set(target.target, sets)) }
        if !resets.isEmpty { add(Commands.unset(target.target, resets)) }
        if !loadSets.isEmpty { add(Commands.setLoad(target.target, loadSets, apply: apply)) }
        if !loadResets.isEmpty { add(Commands.unset(target.target, loadResets, load: true)) }
        var notes: [String] = []
        let loadChanged = !loadSets.isEmpty || !loadResets.isEmpty
        if !sets.isEmpty || !resets.isEmpty { notes.append("Answer settings take effect on the next answer") }
        if loadChanged {
            if loaded && apply == .now { notes.append("Reloads the model now · it pauses while it reloads") }
            else if loaded { notes.append("Takes effect on next load · the loaded model keeps its old settings") }
            else { notes.append("Takes effect when it loads") }
        }
        return ChangePlan(lines: lines, actions: actions, notes: notes, reloads: loadChanged && loaded && apply == .now)
    }

    /// Load and unload, for a GPU box model whose engine accepts them.
    public static func loadActions(_ target: TargetSettings, engines: EnginesReport?, machines: [MachineCard]) -> (load: ActionPlan?, unload: ActionPlan?) {
        guard let gate = loadControl(target, engines: engines, machines: machines),
              let lane = target.engineLane, let machine = Machines.machine(forLane: lane, in: machines) else { return (nil, nil) }
        let model = engines?.model(target.target)?.model
        let status = model?.status ?? target.engineStatus ?? "unknown"
        if status == "loaded" || status == "loading" || status == "sleeping" {
            let busy = model?.inFlight ?? 0
            let unloadGate: Gate = !gate.isAllowed ? gate : status == "loading" ? .disabled("Still loading")
                : busy > 0 ? .disabled("Answering now · try when it is done") : .allowed
            return (nil, ActionPlan(id: "unload:\(target.target)", title: "Unload", command: Commands.unload(target.target), gate: unloadGate))
        }
        let asleep = machine.power == .asleep || machine.power == .poweringOff
        let loadGate: Gate = !gate.isAllowed ? gate : machine.power == .waking ? .disabled("Waking up · try in a moment") : .allowed
        let title = WakeWarning.label("Load now", wakes: asleep, machine: machine.title)
        return (ActionPlan(id: "load:\(target.target)", title: title, command: Commands.load(target.target, wake: asleep),
                           wakes: asleep,
                           confirmation: asleep ? WakeWarning.confirmation("Load now", machine: machine.title,
                                                                           wakeSeconds: machine.wakeSeconds) : nil,
                           gate: loadGate), nil)
    }

    /// "Try it": one request to the name. A request to a model on a sleeping GPU box wakes it.
    public static func tryIt(_ name: String, targets: [String], prompt: String, machines: [MachineCard]) -> ActionPlan {
        let first = targets.first.flatMap { target in
            target.split(separator: "/").first.flatMap { Machines.machine(forLane: String($0), in: machines) }
        }
        let wakes = first.map { $0.kind == .gpuBox && ($0.power == .asleep || $0.power == .poweringOff) } ?? false
        let machine = first?.title ?? "GPU box"
        let trimmed = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        return ActionPlan(id: "chat:\(name)", title: WakeWarning.label("Send", wakes: wakes, machine: machine),
                          command: Commands.chat(name, prompt: trimmed), wakes: wakes,
                          confirmation: wakes ? WakeWarning.confirmation("Send", machine: machine, wakeSeconds: first?.wakeSeconds) : nil,
                          gate: trimmed.isEmpty ? .disabled("Write a prompt first") : .allowed)
    }

    /// The versions of a target's settings, newest first, each but the current with "Undo to here".
    public static func versions(_ history: SettingsHistory, now: Date) -> [VersionRow] {
        history.entries.map { entry in
            let current = entry.revision == history.revision
            let what: String
            if entry.action == "revert" { what = "Went back to an earlier version" }
            else if entry.changed.isEmpty { what = "Saved without a change" }
            else {
                var names: [String] = []
                for key in entry.changed { let word = Words.setting(key); if !names.contains(word) { names.append(word) } }
                what = names.joined(separator: ", ")
            }
            let when = entry.atMs.map { Words.ago(now.timeIntervalSince1970 - $0 / 1000) } ?? ""
            let undo = current ? nil : ActionPlan(id: "revert:\(history.target):\(entry.revision)", title: "Undo to here",
                                                  command: Commands.revert(history.target, to: entry.revision))
            return VersionRow(revision: entry.revision, title: "Version \(entry.revision)", when: when, what: what,
                              isCurrent: current, undo: undo)
        }
    }

    /// The speed report's findings for these targets, in plain words.
    public static func findings(_ doctor: DoctorReport?, targets: [String]) -> [FindingRow] {
        (doctor?.findings ?? []).filter { targets.contains($0.target) }.map { finding in
            var advice = finding.action, command: String?
            if let start = finding.action.firstIndex(of: "`"),
               let end = finding.action[finding.action.index(after: start)...].firstIndex(of: "`") {
                command = String(finding.action[finding.action.index(after: start)..<end])
                advice = String(finding.action[..<start]).trimmingCharacters(in: CharacterSet(charactersIn: " :"))
            }
            advice = advice.prefix(1).uppercased() + advice.dropFirst()
            return FindingRow(kind: finding.kind, text: Words.plain(finding.evidence.prefix(1).uppercased() + finding.evidence.dropFirst()),
                              advice: Words.plain(advice), command: command,
                              draft: command.flatMap { draft(fromCommand: $0, target: finding.target) })
        }
    }

    /// `sb ai set TARGET k=v ...` as inspector changes, when every key is one the inspector shows.
    static func draft(fromCommand command: String, target: String) -> SettingsDraft? {
        let words = command.split(separator: " ").map(String.init)
        guard words.count > 4, words[0] == "sb", words[1] == "ai", words[2] == "set", words[3] == target else { return nil }
        var draft = SettingsDraft()
        for pair in words.dropFirst(4) {
            let parts = pair.split(separator: "=", maxSplits: 1).map(String.init)
            guard parts.count == 2 else { return nil }
            let value = (try? JSONValue.parse(Data(parts[1].utf8))) ?? .string(parts[1])
            let known = requestKeys + guardKeys + ["reasoning"]
            guard known.contains(parts[0]) else { return nil }
            draft.values[parts[0]] = .set(value)
        }
        // `reasoning=true` with a minimum is how the report turns the guard on.
        if draft.values["reasoning_guard.min_tokens"] != nil && draft.values["reasoning_guard.enabled"] == nil {
            draft.values["reasoning_guard.enabled"] = .set(.bool(true))
        }
        return draft
    }
}
