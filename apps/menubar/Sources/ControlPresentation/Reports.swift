import Foundation

// What each `sb` read answers, read leniently from its JSON. Field names follow the CLI's output.

/// A hold: something keeps a machine on until a time.
public struct HoldReport: Sendable, Equatable, Identifiable {
    public let id: String
    public let reason: String?
    public let owner: String?
    /// "active", "waking" or "wake_failed".
    public let state: String?
    public let expiresAtMs: Double?
    public let wakeDetail: String?
    init(_ json: JSONValue) {
        id = json["id"].string ?? "?"
        reason = json["reason"].string.flatMap { $0.isEmpty ? nil : $0 }
        owner = json["owner"].string
        state = json["state"].string
        expiresAtMs = json["expires_at_ms"].double
        wakeDetail = json["wake_detail"].string
    }
    public init(id: String, reason: String?, owner: String? = nil, state: String? = "active", expiresAtMs: Double?, wakeDetail: String? = nil) {
        self.id = id; self.reason = reason; self.owner = owner; self.state = state
        self.expiresAtMs = expiresAtMs; self.wakeDetail = wakeDetail
    }
}

/// One engine the gateway runs work on (`sb ai status` calls it a lane), and the machine it lives on.
public struct LaneReport: Sendable, Equatable {
    public let name: String
    /// The machine; a lane with a machine of its own has none and is named after itself.
    public let machine: String
    /// offline, waking, healthy, draining, idle_countdown, powering_off, degraded.
    public let state: String
    public let queueDepth: Int
    public let inFlight: Int
    public let holdsGPU: Bool?
    public let gpuGroup: String?
    public let poweroffInSecs: Double?
    public let poweroffGuardBusy: Bool
    public let lastError: String?
    public let holds: [HoldReport]
    /// Nil when the key may only read the short client view.
    public let wakeConfigured: Bool?
    public let poweroffConfigured: Bool?
    public let lastWakeMs: Double?
    public let lastWakeResult: String?
    public let wakeDurationMs: Double?
    public let wakeDurationAverageMs: Double?
    /// When the machine came up, when the gateway says so (newer gateways); else nil.
    public let upSinceMs: Double?
    init(_ json: JSONValue) {
        name = json["name"].string ?? "?"
        machine = json["host"].string ?? name
        state = json["state"].string ?? "unknown"
        queueDepth = json["queue_depth"].int ?? 0
        inFlight = json["in_flight"].int ?? 0
        holdsGPU = json["holds_gpu"].bool
        gpuGroup = json["gpu_group"].string
        poweroffInSecs = json["poweroff_in_secs"].double
        poweroffGuardBusy = json["poweroff_guard_busy"].bool ?? false
        lastError = json["last_error"].string
        holds = json["holds"].array.map(HoldReport.init)
        wakeConfigured = json["wake_configured"].bool
        poweroffConfigured = json["poweroff_configured"].bool
        lastWakeMs = json["last_wake_ms"].double
        lastWakeResult = json["last_wake_result"].string
        wakeDurationMs = json["wake_duration_ms"].double
        wakeDurationAverageMs = json["wake_duration_ewma_ms"].double
        upSinceMs = json["up_since_ms"].double
    }
}

/// `sb ai status --json`: each machine's power and its lanes.
public struct AIStatus: Sendable, Equatable {
    /// machine → "on", "waking", "powering off" or "off", in the CLI's order.
    public let power: [(machine: String, power: String)]
    public let lanes: [LaneReport]
    public init(_ json: JSONValue) {
        power = json["machines"].array.compactMap { row in
            row["machine"].string.map { ($0, row["power"].string ?? "off") }
        }
        lanes = json["lanes"].array.map(LaneReport.init)
    }
    public static func == (lhs: AIStatus, rhs: AIStatus) -> Bool {
        lhs.lanes == rhs.lanes && lhs.power.map(\.machine) == rhs.power.map(\.machine)
            && lhs.power.map(\.power) == rhs.power.map(\.power)
    }
}

/// `sb ai models --json`: the names people use, by job.
public struct ModelCatalog: Sendable, Equatable {
    public let byJob: [Job: [String]]
    public init(_ json: JSONValue) {
        var groups: [Job: [String]] = [:]
        for job in Job.allCases { groups[job] = json[job.rawValue].array.compactMap(\.string) }
        byJob = groups
    }
    public init(byJob: [Job: [String]]) { self.byJob = byJob }
    public subscript(job: Job) -> [String] { byJob[job] ?? [] }
}

/// The kinds of work, in the order the window lists them.
public enum Job: String, CaseIterable, Sendable {
    case text, image, video, audio, segment
    public var title: String {
        switch self {
        case .text: "Text"
        case .image: "Image"
        case .video: "Video"
        case .audio: "Audio"
        case .segment: "Segment"
        }
    }
    public var symbol: String {
        switch self {
        case .text: "text.bubble"
        case .image: "photo"
        case .video: "film"
        case .audio: "waveform"
        case .segment: "square.dashed"
        }
    }
}

/// The memory an engine measured for a model (bytes on the GPU and in host memory, at a context size).
public struct MemoryPart: Sendable, Equatable {
    public let context: Int?
    public let bytes: Double?
    public let hostBytes: Double?
    init?(_ json: JSONValue) {
        guard json.object != nil else { return nil }
        context = json["n_ctx"].int
        bytes = json["bytes"].double
        hostBytes = json["host_bytes"].double
    }
    public init(context: Int?, bytes: Double?, hostBytes: Double? = nil) {
        self.context = context; self.bytes = bytes; self.hostBytes = hostBytes
    }
}
public struct MemorySummary: Sendable, Equatable {
    public let minimum: MemoryPart?
    public let maximum: MemoryPart?
    public let effective: MemoryPart?
    init?(_ json: JSONValue) {
        guard json.object != nil else { return nil }
        minimum = MemoryPart(json["min"]); maximum = MemoryPart(json["max"]); effective = MemoryPart(json["effective"])
    }
    public init(minimum: MemoryPart?, maximum: MemoryPart?, effective: MemoryPart?) {
        self.minimum = minimum; self.maximum = maximum; self.effective = effective
    }
    /// What the loaded model uses now: the effective measurement, else the smallest.
    public var inUse: MemoryPart? { effective ?? minimum }
}

/// A model an engine has, from `sb ai engines --json`.
public struct EngineModel: Sendable, Equatable {
    public let id: String
    public let target: String
    /// loaded, loading, unloaded, sleeping, ...
    public let status: String
    public let failed: Bool
    /// The engine's own start arguments, read only for context size, KV cache and parallel slots.
    public let arguments: [String]
    public let memory: MemorySummary?
    public let pending: Bool
    public let inFlight: Int
    init(_ json: JSONValue, lane: String) {
        id = json["id"].string ?? "?"
        target = json["target"].string ?? "\(lane)/\(id)"
        status = json["status"].string ?? "unknown"
        // `failed` is a flag or a detail object; anything but false/null counts.
        switch json["failed"] {
        case .null, .bool(false): failed = false
        default: failed = true
        }
        arguments = json["args"].array.compactMap(\.string)
        memory = MemorySummary(json["memory"])
        pending = json["pending"].bool ?? false
        inFlight = json["in_flight"].int ?? 0
    }
    public init(id: String, target: String, status: String, failed: Bool = false, arguments: [String] = [],
                memory: MemorySummary? = nil, pending: Bool = false, inFlight: Int = 0) {
        self.id = id; self.target = target; self.status = status; self.failed = failed
        self.arguments = arguments; self.memory = memory; self.pending = pending; self.inFlight = inFlight
    }
    /// The value of `--name VALUE`, `-n VALUE` or `--name=VALUE` in the engine's arguments.
    public func argument(_ names: [String]) -> String? {
        var iterator = arguments.makeIterator()
        while let argument = iterator.next() {
            if names.contains(argument) { return iterator.next() }
            for name in names where name.hasPrefix("--") && argument.hasPrefix(name + "=") {
                return String(argument.dropFirst(name.count + 1))
            }
        }
        return nil
    }
}

/// One engine and what it has loaded.
public struct EngineLane: Sendable, Equatable {
    public let lane: String
    public let kind: String
    /// "up", "asleep", "waking", ...
    public let state: String
    public let error: String?
    public let models: [EngineModel]
    /// True when the engine takes load settings and load/unload; a listing-only engine has none.
    public let acceptsControl: Bool
    public let lastApplyError: String?
    init(_ json: JSONValue) {
        lane = json["lane"].string ?? "?"
        kind = json["kind"].string ?? ""
        state = json["state"].string ?? "unknown"
        error = json["error"].string
        let lane = self.lane
        models = json["models"].array.map { EngineModel($0, lane: lane) }
        acceptsControl = json["settings_command"].bool ?? false
        let apply = json["last_apply"]
        lastApplyError = apply["ok"].bool == false ? (apply["error"].string ?? "no detail") : nil
    }
    public init(lane: String, kind: String = "llama_cpp_router", state: String = "up", error: String? = nil,
                models: [EngineModel], acceptsControl: Bool, lastApplyError: String? = nil) {
        self.lane = lane; self.kind = kind; self.state = state; self.error = error
        self.models = models; self.acceptsControl = acceptsControl; self.lastApplyError = lastApplyError
    }
}
/// `sb ai engines --json`.
public struct EnginesReport: Sendable, Equatable {
    public let lanes: [EngineLane]
    public init(_ json: JSONValue) { lanes = json["data"].array.map(EngineLane.init) }
    public init(lanes: [EngineLane]) { self.lanes = lanes }
    public func model(_ target: String) -> (lane: EngineLane, model: EngineModel)? {
        for lane in lanes { if let model = lane.models.first(where: { $0.target == target }) { return (lane, model) } }
        return nil
    }
    public func lane(_ name: String) -> EngineLane? { lanes.first { $0.lane == name } }
}

/// A setting's effective value and where it comes from.
public struct EffectiveSetting: Sendable, Equatable {
    public let value: JSONValue
    /// "engine", "defaults", "profile:NAME" or "route_profile:NAME".
    public let source: String
    public let locked: Bool
    public init(value: JSONValue, source: String, locked: Bool = false) {
        self.value = value; self.source = source; self.locked = locked
    }
}
public struct LoadSetting: Sendable, Equatable {
    public let value: JSONValue
    public let pending: Bool
}
/// The numbers the gateway checks a context size against when the engine has no measurement.
public struct FitEstimate: Sendable, Equatable {
    public let weightsBytes: Double
    public let budgetBytes: Double
    public let kvLayers: Double
    public let kvHeads: Double
    public let headDim: Double
    public let valueDim: Double?
    init?(_ json: JSONValue) {
        guard let weights = json["weights_bytes"].double, let budget = json["budget_bytes"].double,
              let layers = json["kv_layers"].double, let heads = json["kv_heads"].double,
              let dim = json["head_dim"].double else { return nil }
        weightsBytes = weights; budgetBytes = budget; kvLayers = layers; kvHeads = heads; headDim = dim
        valueDim = json["value_dim"].double
    }
    public init(weightsBytes: Double, budgetBytes: Double, kvLayers: Double, kvHeads: Double, headDim: Double, valueDim: Double? = nil) {
        self.weightsBytes = weightsBytes; self.budgetBytes = budgetBytes; self.kvLayers = kvLayers
        self.kvHeads = kvHeads; self.headDim = headDim; self.valueDim = valueDim
    }
}
/// One target's settings: `sb ai settings MODEL --json` lists one per target behind the name.
public struct TargetSettings: Sendable, Equatable {
    public let target: String
    public let revision: Int
    public let effective: [String: EffectiveSetting]
    /// The saved document; nil when this key may only read settings.
    public let document: JSONValue?
    public let locked: [String]
    public let routeProfile: String?
    /// The engine side: nil for a target that no engine control loads.
    public let engineLane: String?
    public let engineStatus: String?
    public let loadSettings: [String: LoadSetting]
    public let loadPending: Bool
    public let memory: MemorySummary?
    public let bounds: [String: ClosedRange<Double>]
    public let estimate: FitEstimate?
    public let reserveBytes: Double
    init(_ json: JSONValue) {
        target = json["target"].string ?? "?"
        revision = json["revision"].int ?? 0
        var effective: [String: EffectiveSetting] = [:]
        for (key, row) in json["effective"].object ?? [:] {
            effective[key] = EffectiveSetting(value: row["value"], source: row["source"].string ?? "engine",
                                              locked: row["locked"].bool ?? false)
        }
        self.effective = effective
        document = json.object?["document"]
        locked = json["locked"].array.compactMap(\.string)
        routeProfile = json["route_profile"].string
        let load = json["load"]
        engineLane = load["engine"]["lane"].string
        engineStatus = load["engine"]["status"].string
        var settings: [String: LoadSetting] = [:]
        for (key, row) in load["settings"].object ?? [:] {
            settings[key] = LoadSetting(value: row["value"], pending: row["pending"].bool ?? false)
        }
        loadSettings = settings
        loadPending = load["pending"].bool ?? false
        memory = MemorySummary(load["memory"])
        let policy = json["request_load"]
        var bounds: [String: ClosedRange<Double>] = [:]
        for (key, pair) in policy["bounds"].object ?? [:] {
            if let low = pair.array.first?.double, let high = pair.array.last?.double, low <= high { bounds[key] = low...high }
        }
        self.bounds = bounds
        estimate = FitEstimate(policy["estimate"])
        reserveBytes = policy["reserve_bytes"].double ?? 0
    }
    /// True when something is saved for this target (its document is not empty).
    public var hasSaved: Bool { !(document?.isEmpty ?? true) }
}
/// `sb ai settings MODEL --json`.
public struct SettingsView: Sendable, Equatable {
    public let model: String
    public let targets: [TargetSettings]
    public init(_ json: JSONValue) {
        model = json["model"].string ?? ""
        targets = json["targets"].array.map(TargetSettings.init)
    }
    /// A name that reaches several targets picks one of them per request: a route.
    public var isRoute: Bool { targets.count > 1 }
}

public struct HistoryEntry: Sendable, Equatable, Identifiable {
    public var id: Int { revision }
    public let revision: Int
    public let atMs: Double?
    public let action: String
    public let changed: [String]
    public let role: String?
}
/// `sb ai settings TARGET --history --json`.
public struct SettingsHistory: Sendable, Equatable {
    public let target: String
    public let revision: Int
    public let entries: [HistoryEntry]
    public init(_ json: JSONValue) {
        target = json["target"].string ?? "?"
        revision = json["revision"].int ?? 0
        entries = json["history"].array.compactMap { row in
            guard let revision = row["revision"].int else { return nil }
            return HistoryEntry(revision: revision, atMs: row["at_ms"].double, action: row["action"].string ?? "put",
                                changed: row["changed"].array.compactMap(\.string), role: row["role"].string)
        }.sorted { $0.revision > $1.revision }
    }
}

/// One target in the speed report.
public struct SpeedReport: Sendable, Equatable {
    public let target: String
    public let lane: String?
    public let machine: String?
    public let decodeTPS: Double?
    public let promptTPS: Double?
    public let firstTokenMs: Double?
    public let ceilingTPS: Double?
    public let ceilingSource: String?
    public let percentOfCeiling: Double?
    public let requests: Int
    init(_ json: JSONValue) {
        target = json["target"].string ?? "?"
        lane = json["lane"].string
        machine = json["host"].string
        decodeTPS = json["decode_tps_p50"].double
        promptTPS = json["prompt_tps_p50"].double
        firstTokenMs = json["ttft_ms_p50"].double
        ceilingTPS = json["ceiling"]["decode_tps"].double
        ceilingSource = json["ceiling"]["source"].string
        percentOfCeiling = json["pct_of_ceiling"].double
        requests = json["requests"].int ?? 0
    }
}
public struct Finding: Sendable, Equatable, Identifiable {
    public var id: String { target + "|" + kind }
    public let target: String
    public let kind: String
    public let evidence: String
    public let action: String
}
/// `sb ai doctor --json`: measured speed against the ceiling, and findings.
public struct DoctorReport: Sendable, Equatable {
    public let targets: [SpeedReport]
    public let findings: [Finding]
    public init(_ json: JSONValue) {
        targets = json["targets"].array.map(SpeedReport.init)
        findings = json["findings"].array.map { row in
            Finding(target: row["target"].string ?? "?", kind: row["kind"].string ?? "",
                    evidence: row["evidence"].string ?? "", action: row["action"].string ?? "")
        }
    }
    public func speed(_ target: String) -> SpeedReport? { targets.first { $0.target == target } }
}

/// `sb ai chat MODEL "prompt" --json`.
public struct ChatAnswer: Sendable, Equatable {
    public let answer: String
    public let thinking: String?
    public let promptTokens: Int
    public let completionTokens: Int
    public let firstTokenMs: Double?
    public let tokensPerSecond: Double?
    public let totalSeconds: Double?
    /// The model that answered, as the gateway's route header names it.
    public let route: String?
    public init(_ json: JSONValue) {
        answer = json["answer"].string ?? ""
        thinking = json["reasoning"].string
        promptTokens = json["prompt_tokens"].int ?? 0
        completionTokens = json["completion_tokens"].int ?? 0
        firstTokenMs = json["ttft_ms"].double
        tokensPerSecond = json["decode_tok_s"].double
        totalSeconds = json["total_s"].double
        route = json["route"].string
    }
}
