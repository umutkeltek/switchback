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

/// The undo beside one setting.
public enum RowUndo: Sendable, Equatable {
    /// Take back an edit waiting for Apply: the saved value shows again.
    case undoChange
    /// Take back a reset waiting for Apply: the saved value stays.
    case undoReset
    /// Nothing waiting: remove the saved value, so the engine's own value applies (after Apply).
    case reset
    public var help: String {
        switch self {
        case .undoChange: "Undo this change"
        case .undoReset: "Undo the reset · keep the saved value"
        case .reset: "Back to the engine’s own value"
        }
    }
    public func spoken(_ title: String) -> String {
        switch self {
        case .undoChange: "Undo change to " + title
        case .undoReset: "Undo reset of " + title
        case .reset: "Reset " + title
        }
    }
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

/// What a setting's control shows.
public struct SettingFace: Sendable, Equatable {
    public enum Source: Sendable, Equatable { case draft, saved, engine, unknown }
    /// The value shown; nil when the engine decides and the window does not know its value.
    public let value: JSONValue?
    public let source: Source
    /// The words beside the control: "0.6", "Engine decides", "auto".
    public let text: String
    /// No value: a slider draws a neutral track with no handle; dragging on it starts a value.
    public var neutral: Bool { value == nil }
}

/// Thinking: the engine decides, or on, or off.
public enum ThinkingChoice: String, Sendable, CaseIterable, Identifiable {
    case engine, on, off
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .engine: "Engine"
        case .on: "On"
        case .off: "Off"
        }
    }
}

/// The context size control.
public struct ContextFace: Sendable, Equatable {
    /// The size shown; nil until one is chosen or the engine's own size is known.
    public let size: Int?
    /// The slider position among the steps; nil draws no handle.
    public let index: Int?
    /// Whether the size fits; nil without a size.
    public let fit: FitHint?
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

    /// The engine's own value for an answer setting, when the window can see it: from the gateway's effective
    /// settings when it reports one, else from the engine's start arguments (`--temperature 0.8`).
    static func engineValue(_ key: String, effective: EffectiveSetting?, engineModel: EngineModel?) -> JSONValue? {
        if let effective, effective.source == "engine", !effective.value.isNull { return effective.value }
        let names: [String] = switch key {
        case "temperature": ["--temperature", "--temp"]
        case "top_p": ["--top-p"]
        case "max_tokens": ["--n-predict", "--predict", "-n"]
        default: []
        }
        guard let text = engineModel?.argument(names), let number = Double(text) else { return nil }
        // -1 means "until the context is full": no number to show.
        return key == "max_tokens" && number <= 0 ? nil : .number(number)
    }

    /// Answer settings: temperature, top P, max tokens, thinking and the reasoning guard. Nothing saved means the
    /// engine decides: `value` stays nil and `shown` holds the engine's own value only when it is known.
    public static func answerSettings(_ target: TargetSettings, engineModel: EngineModel? = nil) -> [SettingState] {
        var rows: [SettingState] = []
        for key in ["temperature", "top_p", "max_tokens"] {
            let effective = target.effective[key] ?? EffectiveSetting(value: .null, source: "engine")
            let badge = SourceBadge.of(source: effective.source, locked: effective.locked)
            let set = effective.source != "engine" && !effective.value.isNull
            rows.append(SettingState(key: key, title: Words.setting(key), value: set ? effective.value : nil,
                                     shown: engineValue(key, effective: effective, engineModel: engineModel),
                                     badge: badge, gate: gate(target, badge: badge)))
        }
        // Thinking is `reasoning.enabled`: on, off, or (nothing saved) the engine decides. The window never guesses.
        let reasoning = target.effective["reasoning"] ?? EffectiveSetting(value: .null, source: "engine")
        let enabled = reasoning.value["enabled"]
        let thinks = target.document?["reasoning"].bool ?? false
        let thinkingBadge = enabled.isNull ? SourceBadge.engine : SourceBadge.of(source: reasoning.source, locked: reasoning.locked)
        rows.append(SettingState(key: "reasoning.enabled", title: "Thinking", value: enabled.isNull ? nil : enabled,
                                 shown: nil, badge: thinkingBadge, gate: gate(target, badge: thinkingBadge)))
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

    // MARK: What each control shows

    /// What a control shows: the edit waiting for Apply, else the saved value, else the engine's own value when it
    /// is known, else nothing (the engine decides; no handle, no made-up position).
    public static func face(_ setting: SettingState, draft: SettingsDraft, neutral: String = "Engine decides",
                            words: (JSONValue) -> String = { Words.value($0) }) -> SettingFace {
        switch draft.values[setting.key] {
        case .set(let value): return SettingFace(value: value, source: .draft, text: words(value))
        case .reset:
            return setting.shown.map { SettingFace(value: $0, source: .engine, text: words($0)) }
                ?? SettingFace(value: nil, source: .unknown, text: neutral)
        case nil:
            if let value = setting.value { return SettingFace(value: value, source: .saved, text: words(value)) }
            if let shown = setting.shown { return SettingFace(value: shown, source: .engine, text: words(shown)) }
            return SettingFace(value: nil, source: .unknown, text: neutral)
        }
    }

    /// The small undo beside a setting's title, if any: undo an edit or a reset waiting for Apply, or (with
    /// nothing waiting) go back to the engine's own value. Nil while the setting may not change here.
    public static func rowUndo(_ setting: SettingState, draft: SettingsDraft) -> RowUndo? {
        guard setting.gate.isAllowed else { return nil }
        switch draft.values[setting.key] {
        case .reset?: return .undoReset
        case .set?: return .undoChange
        case nil: return setting.value == nil ? nil : .reset
        }
    }
    /// What pressing that undo does to the draft. Undoing a waiting edit or reset leaves only that row's entry
    /// out, so the other changes stay waiting.
    public static func press(_ undo: RowUndo, for setting: SettingState, in draft: inout SettingsDraft) {
        switch undo {
        case .undoReset, .undoChange: draft.values[setting.key] = nil
        case .reset: draft.values[setting.key] = .reset
        }
    }

    /// Thinking as a three-way choice; "Engine" whenever nothing is saved or about to be.
    public static func thinking(_ setting: SettingState, draft: SettingsDraft) -> ThinkingChoice {
        switch draft.values[setting.key] {
        case .set(let value): value.bool.map { $0 ? .on : .off } ?? .engine
        case .reset: .engine
        case nil: setting.value?.bool.map { $0 ? .on : .off } ?? .engine
        }
    }
    /// The edit a thinking choice makes: none when it matches what is saved.
    public static func choose(_ choice: ThinkingChoice, for setting: SettingState, in draft: inout SettingsDraft) {
        let wanted: JSONValue? = switch choice {
        case .engine: nil
        case .on: .bool(true)
        case .off: .bool(false)
        }
        if wanted == setting.value { draft.values[setting.key] = nil }
        else if let wanted { draft.values[setting.key] = .set(wanted) }
        else { draft.values[setting.key] = .reset }
    }

    /// Where a stepper starts when it shows "auto" and the engine's value is unknown: shown only once pressed.
    public static func stepperStart(_ key: String) -> Double {
        switch key {
        case "max_tokens": 4096
        case "load.parallel": 1
        case "reasoning_guard.min_tokens": 1024
        default: 1
        }
    }
    /// The value a stepper press moves to: the next allowed step from what it shows, or its start when it shows
    /// "auto".
    public static func step(_ setting: SettingState, draft: SettingsDraft, steps: [Double], range: ClosedRange<Double>,
                            up: Bool) -> Double? {
        let allowed = steps.filter { range.contains($0) }
        guard let first = allowed.first, let last = allowed.last else { return nil }
        guard let current = face(setting, draft: draft).value?.double else {
            let start = stepperStart(setting.key)
            return allowed.min { abs($0 - start) < abs($1 - start) } ?? first
        }
        return up ? (allowed.first { $0 > current } ?? last) : (allowed.last { $0 < current } ?? first)
    }

    /// The context size control: a handle only once a size is chosen or the engine's own size is known, and a
    /// fit verdict only for a real size.
    public static func contextFace(_ context: SettingState, cache: SettingState?, draft: SettingsDraft, steps: [Double],
                                   target: TargetSettings, engineModel: EngineModel?) -> ContextFace {
        let shown = face(context, draft: draft).value?.double.map { Int($0) }
        guard let size = shown else {
            return ContextFace(size: nil, index: nil, fit: nil, text: "Engine decides")
        }
        let index = steps.firstIndex { $0 >= Double(size) } ?? max(0, steps.count - 1)
        let type = cache.flatMap { face($0, draft: draft).value?.string } ?? "f16"
        return ContextFace(size: size, index: index,
                           fit: fit(context: size, cacheK: type, target: target, engineModel: engineModel),
                           text: Words.tokens(size))
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
            // The cache type only when the engine's start arguments name one; otherwise the engine decides.
            row("load.cache_type_k", engine: model?.argument(["--cache-type-k", "-ctk"]).map(JSONValue.string)),
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
        let engineModel = engines?.model(target.target)?.model
        let answer = Dictionary(uniqueKeysWithValues: answerSettings(target, engineModel: engineModel).map { ($0.key, $0) })
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
                let locked = state?.badge == .locked || target.locked.contains(key)
                lines.append(ChangeLine(key: key, title: Words.setting(key), from: fromWord,
                                        to: locked ? "engine default · also removes the lock" : "engine default"))
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

    /// Load and unload, for a GPU box model whose engine accepts them. For a route, `target` is the model chosen
    /// in its picker. "Unload" only for a model in memory (or going into it) on a box that is on; a resting model
    /// is not in memory, so it gets "Load now" instead.
    public static func loadActions(_ target: TargetSettings, engines: EnginesReport?, machines: [MachineCard]) -> (load: ActionPlan?, unload: ActionPlan?) {
        guard let gate = loadControl(target, engines: engines, machines: machines),
              let lane = target.engineLane, let machine = Machines.machine(forLane: lane, in: machines) else { return (nil, nil) }
        let model = engines?.model(target.target)?.model
        let status = model?.status ?? target.engineStatus ?? "unknown"
        // A box that is not on has nothing in memory, whatever its last listing says.
        if machine.power == .on && (status == "loaded" || status == "loading") {
            let busy = model?.inFlight ?? 0
            let unloadGate: Gate = !gate.isAllowed ? gate : status == "loading" ? .disabled("Still loading")
                : busy > 0 ? .disabled("Answering now · try when it is done") : .allowed
            return (nil, ActionPlan(id: "unload:\(target.target)", title: "Unload", command: Commands.unload(target.target), gate: unloadGate))
        }
        let asleep = machine.power == .asleep || machine.power == .poweringOff
        let loadGate: Gate = !gate.isAllowed ? gate : machine.power == .waking ? .disabled("Waking up · try in a moment") : .allowed
        let title = WakeWarning.label("Load now", wakes: asleep, machine: machine.kind.title)
        return (ActionPlan(id: "load:\(target.target)", title: title, command: Commands.load(target.target, wake: asleep),
                           wakes: asleep,
                           confirmation: asleep ? WakeWarning.confirmation("Load now", machine: machine.title,
                                                                           wakeSeconds: machine.wakeSeconds) : nil,
                           gate: loadGate), nil)
    }

    /// "Try it": one request to the name, which the gateway always sends, waking a sleeping GPU box. A route
    /// picks any of its models, so every model behind the name counts: it asks first whenever any of them is on a
    /// GPU box the window does not know to be on, or on a machine whose kind the window cannot tell (the short
    /// status a non-operator key reads), and whenever it does not know the name's models yet.
    /// The button says only "Send · may wake" or "Send · wakes", so it never cuts its label; the full words are in
    /// `spoken` (VoiceOver and the tooltip) and in the question.
    public static func tryIt(_ name: String, targets: [String], targetsKnown: Bool = true, prompt: String,
                             machines: [MachineCard]) -> ActionPlan {
        let trimmed = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        let command = Commands.chat(name, prompt: trimmed)
        let gate: Gate = trimmed.isEmpty ? .disabled("Write a prompt first") : .allowed
        var boxes: [MachineCard] = []
        for target in targets {
            guard let card = Machines.machine(ofTarget: target, in: machines),
                  !boxes.contains(where: { $0.machine == card.machine }) else { continue }
            if card.kind == .unknown || (card.kind == .gpuBox && !card.knownOn) { boxes.append(card) }
        }
        let unknown = !targetsKnown || targets.isEmpty || machines.isEmpty
        guard unknown || !boxes.isEmpty else {
            return ActionPlan(id: "chat:\(name)", title: "Send", command: command, gate: gate)
        }
        let box = boxes.count == 1 ? boxes[0] : nil
        // "the GPU box" for one GPU box; "a machine" whenever a machine of unknown kind (or no known one) is behind it.
        let allBoxes = !unknown && boxes.allSatisfy { $0.kind == .gpuBox }
        let who = box.map { $0.kind == .gpuBox ? "the " + $0.title : "a machine" } ?? (allBoxes ? "the GPU box" : "a machine")
        let wakeSeconds = boxes.compactMap(\.wakeSeconds).max()
        func asleep(_ card: MachineCard) -> Bool {
            card.kind == .gpuBox && (card.power == .asleep || card.power == .poweringOff) && card.powerDoubt == nil
        }
        // Every model behind the name sits on a GPU box known to be asleep: the request surely wakes it.
        let surely = !unknown && targets.allSatisfy { target in
            Machines.machine(ofTarget: target, in: machines).map { card in boxes.contains { $0.machine == card.machine } && asleep(card) } ?? false
        }
        if surely {
            let question = WakeWarning.confirmation("Send", machine: who.hasPrefix("the ") ? String(who.dropFirst(4)) : who,
                                                    wakeSeconds: wakeSeconds)
            return ActionPlan(id: "chat:\(name)", title: "Send · wakes", command: command, wakes: true, confirmation: question,
                              gate: gate, spoken: "Send · wakes " + who)
        }
        let reason: String
        if unknown {
            reason = "The window does not know yet which models \(name) uses."
        } else if let first = boxes.first(where: { $0.kind == .unknown }) {
            reason = "The window can’t tell what the machine \(first.machine) can do: this key reads only the short status."
        } else if let first = boxes.first, asleep(first) {
            reason = "\(name) can answer from a model on the \(first.title), which is asleep."
        } else if let first = boxes.first {
            reason = "The window can’t be sure the \(first.title) is on: \(first.powerDoubt ?? first.powerWord.lowercased())."
        } else {
            reason = "The window can’t be sure where \(name) runs."
        }
        return ActionPlan(id: "chat:\(name)", title: "Send · may wake", command: command, wakes: true,
                          confirmation: WakeWarning.mayWakeConfirmation("Send", named: who, reason: reason,
                                                                        wakeSeconds: wakeSeconds),
                          gate: gate, spoken: "Send · may wake " + who)
    }

    /// What "Try it" does on the Mac: its engine loads a resting model by itself to answer.
    public static func tryItNote(targets: [String], machines: [MachineCard]) -> String? {
        let mac = targets.contains { Machines.machine(ofTarget: $0, in: machines)?.kind == .mac }
        return mac ? "Loads it on the Mac if it is resting" : nil
    }

    /// The versions of a target's settings, newest first, each but the current with "Undo to here". Never on the
    /// Mac (or a machine not read yet): a revert puts back the whole saved document, `load.*` keys included, and
    /// the window never changes what the Mac loads.
    public static func versions(_ history: SettingsHistory, now: Date, machines: [MachineCard]) -> [VersionRow] {
        let machine = Machines.machine(ofTarget: history.target, in: machines)
        let showOnly = machines.isEmpty || machine?.kind == .mac || machine?.kind == .unknown
        return history.entries.map { entry in
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
            let undo = current || showOnly ? nil
                : ActionPlan(id: "revert:\(history.target):\(entry.revision)", title: "Undo to here",
                             command: Commands.revert(history.target, to: entry.revision),
                             spoken: "Undo to version \(entry.revision)")
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
