import Foundation

/// Where a model runs.
public enum Location: Sendable, Equatable {
    case machine(MachineKind, title: String)
    case cloud
    case unknown
    public var word: String {
        switch self {
        case .machine(_, let title): title
        case .cloud: "Cloud"
        case .unknown: "—"
        }
    }
    public var symbol: String {
        switch self {
        case .machine(.gpuBox, _): "cpu"
        case .machine(.mac, _): "desktopcomputer"
        case .cloud: "cloud"
        case .unknown: "questionmark.circle"
        }
    }
}

/// Whether a model is in memory. Every place says it the same way: "In memory" (a filled green dot) or "Resting"
/// (a hollow dot: listed, not in memory; its engine loads it again when a request comes).
public enum LoadState: String, Sendable, Equatable {
    case loaded, resting, loading, unloaded, failed
    /// Nothing loads this model (a cloud model, an image workflow): no dot.
    case none
    public var word: String {
        switch self {
        case .loaded: "In memory"
        case .resting: "Resting"
        case .loading: "Loading"
        case .unloaded: "Not loaded"
        case .failed: "Failed to load"
        case .none: ""
        }
    }
}

/// A small bar: measured speed against the model's ceiling, or against the fastest model in its group when no
/// ceiling is saved.
public struct SpeedBar: Sendable, Equatable {
    public let tokensPerSecond: Double
    public let ceiling: Double?
    public let fraction: Double
    public var againstCeiling: Bool { ceiling != nil }
    public var word: String { Words.speed(tokensPerSecond) }
    public var detail: String {
        if let ceiling {
            return "\(Words.speed(tokensPerSecond)) · \(Int((tokensPerSecond / ceiling * 100).rounded()))% of its \(Words.speed(ceiling)) ceiling"
        }
        return "\(Words.speed(tokensPerSecond)) · no ceiling saved; the bar compares it with the fastest model here"
    }
}

public struct ModelRow: Sendable, Equatable, Identifiable {
    public var id: String { job.rawValue + ":" + name }
    public let name: String
    public let job: Job
    /// The models behind the name, as far as known.
    public let targets: [String]
    /// The name picks one of several models per request.
    public let isRoute: Bool
    /// A name that stands for one model with another id ("transcribe" → "whisper-large-v3").
    public let standsFor: String?
    public let location: Location
    /// A route's models on other machines, beyond the first.
    public let elsewhere: Int
    public let load: LoadState
    public let speed: SpeedBar?
    public let saved: Bool
    /// The window has read this name's settings, so its models and saved state are exact.
    public let known: Bool
}

public enum ModelList {
    /// Every name by job, with where it runs, whether it is loaded, its speed and whether it has saved settings.
    /// `settings` holds what `sb ai settings NAME` answered, by name; names not read yet are matched by id.
    public static func rows(catalog: ModelCatalog?, engines: EnginesReport?, doctor: DoctorReport?,
                            machines: [MachineCard], settings: [String: SettingsView]) -> [ModelRow] {
        guard let catalog else { return [] }
        var rows: [ModelRow] = []
        for job in Job.allCases {
            let names = catalog[job]
            var built = names.map { name in row(name, job: job, engines: engines, doctor: doctor, machines: machines, view: settings[name]) }
            // With no ceiling, a speed bar compares with the fastest measured model of the same job.
            let fastest = built.compactMap { $0.speed?.tokensPerSecond }.max() ?? 0
            built = built.map { row in
                guard let speed = row.speed, speed.ceiling == nil, fastest > 0 else { return row }
                return ModelRow(name: row.name, job: row.job, targets: row.targets, isRoute: row.isRoute, standsFor: row.standsFor,
                                location: row.location, elsewhere: row.elsewhere, load: row.load,
                                speed: SpeedBar(tokensPerSecond: speed.tokensPerSecond, ceiling: nil,
                                                fraction: min(1, speed.tokensPerSecond / fastest)),
                                saved: row.saved, known: row.known)
            }
            rows += built
        }
        return rows
    }

    static func row(_ name: String, job: Job, engines: EnginesReport?, doctor: DoctorReport?,
                    machines: [MachineCard], view: SettingsView?) -> ModelRow {
        let targets = view.map { $0.targets.map(\.target) } ?? guessTargets(name, engines: engines, doctor: doctor)
        let locations = targets.map { location(ofTarget: $0, machines: machines) }
        let first = locations.first ?? .unknown
        // A model on a machine that is not on is not in memory, whatever the engine listed before it powered off.
        let states = targets.compactMap { target -> String? in
            guard let status = engines?.model(target)?.model.status else { return nil }
            if let machine = Machines.machine(ofTarget: target, in: machines), machine.power != .on { return "unloaded" }
            return status
        }
        let failed = targets.contains { engines?.model($0)?.model.failed == true }
        let load: LoadState
        if states.contains("loaded") { load = .loaded }
        else if states.contains("loading") { load = .loading }
        else if failed { load = .failed }
        else if states.contains("sleeping") { load = .resting }
        else if !states.isEmpty { load = .unloaded }
        else { load = .none }
        // The speed of the model that answers: a loaded one first, else the first.
        let speedTarget = targets.first { engines?.model($0)?.model.status == "loaded" } ?? targets.first
        let speed = speedTarget.flatMap { doctor?.speed($0) }.flatMap { report -> SpeedBar? in
            guard let tps = report.decodeTPS, tps > 0 else { return nil }
            let ceiling = report.ceilingTPS.flatMap { $0 > 0 ? $0 : nil }
            return SpeedBar(tokensPerSecond: tps, ceiling: ceiling, fraction: ceiling.map { min(1, tps / $0) } ?? 0)
        }
        let single = targets.count == 1 ? targets[0] : nil
        let standsFor = single.flatMap { target -> String? in
            let id = target.split(separator: "/").dropFirst().joined(separator: "/")
            return id == name || target == name || id.hasSuffix("/" + name) ? nil : id
        }
        return ModelRow(name: name, job: job, targets: targets, isRoute: (view?.isRoute ?? false), standsFor: standsFor,
                        location: first, elsewhere: Set(locations.map(\.word)).subtracting([first.word]).count,
                        load: load, speed: speed, saved: view?.targets.contains(where: \.hasSaved) ?? false,
                        known: view != nil)
    }

    /// Models in memory now, each counted once: several names (a route, a name that stands for a model) can
    /// reach the same loaded model, and a resting model is listed but not in memory.
    public static func loadedCount(engines: EnginesReport?) -> Int {
        Set((engines?.lanes ?? []).flatMap(\.models).filter { $0.status == "loaded" }.map(\.target)).count
    }
    /// The sidebar count, in the words the machine cards use: "2 in memory"; nil when none. Counted from the
    /// machines, so a box that is asleep adds nothing, whatever its engine listed before it powered off.
    public static func inMemoryText(_ machines: [MachineCard]) -> String? {
        let count = machines.reduce(0) { $0 + $1.inMemory }
        return count == 0 ? nil : "\(count) in memory"
    }

    /// Before its settings are read, a name is matched to the engines' and the speed report's models by id.
    static func guessTargets(_ name: String, engines: EnginesReport?, doctor: DoctorReport?) -> [String] {
        var found: [String] = []
        for lane in engines?.lanes ?? [] {
            for model in lane.models where model.target == name || model.id == name {
                if !found.contains(model.target) { found.append(model.target) }
            }
        }
        for report in doctor?.targets ?? [] where report.target == name || report.target.hasSuffix("/" + name) {
            if !found.contains(report.target) { found.append(report.target) }
        }
        return found
    }

    /// A target's machine from its engine (the part before the first `/`); an engine no machine has is a cloud provider.
    public static func location(ofTarget target: String, machines: [MachineCard]) -> Location {
        guard let lane = target.split(separator: "/").first.map(String.init) else { return .unknown }
        if let machine = Machines.machine(forLane: lane, in: machines) { return .machine(machine.kind, title: machine.title) }
        return machines.isEmpty ? .unknown : .cloud
    }
}
