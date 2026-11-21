import Foundation

/// What a machine is to the gateway. The window names a machine the gateway wakes and powers off "GPU box",
/// and an always-on machine "Mac"; the Mac is show-only.
public enum MachineKind: Sendable, Equatable {
    /// The gateway wakes it when work arrives and powers it off when it has been idle.
    case gpuBox
    /// Always on: the gateway never wakes it, powers it off or changes what its engine loads.
    case mac
    public var title: String { self == .gpuBox ? "GPU box" : "Mac" }
}

public enum Power: String, Sendable, Equatable {
    case on, asleep, waking, poweringOff
}

public struct HoldCard: Sendable, Equatable, Identifiable {
    public let id: String
    public let machine: String
    /// "nightly check", or "Kept on" when the hold gives no reason.
    public let reason: String
    public let owner: String?
    public let secondsLeft: Double?
    /// "until 20:33 · 1 h 12 min left".
    public let untilText: String
    public let waking: Bool
    public let wakeFailed: String?
    public let release: ActionPlan
}

public struct LoadedModel: Sendable, Equatable, Identifiable {
    public var id: String { target }
    public let target: String
    public let name: String
    public enum State: String, Sendable { case loaded, resting, loading, failed }
    public let state: State
    public let memoryBytes: Double?
    public let context: Int?
    public let answering: Int
}

/// Something wrong, in one plain line.
public struct Problem: Sendable, Equatable, Identifiable {
    public var id: String { text }
    /// The line with the machine it is about, for lists across machines.
    public let text: String
    /// The same line inside the machine's own card, without its name.
    public let short: String
    /// A worse problem draws red; a softer one orange.
    public let serious: Bool
    public init(text: String, short: String? = nil, serious: Bool) {
        self.text = text; self.short = short ?? text; self.serious = serious
    }
}

/// One machine as the window shows it.
public struct MachineCard: Sendable, Equatable, Identifiable {
    public var id: String { machine }
    /// The gateway's name for the machine; the `--host` of its commands.
    public let machine: String
    public let kind: MachineKind
    /// "GPU box", or "GPU box · name" when there are several.
    public let title: String
    public let power: Power
    /// "On", "Asleep", "Waking", "Powering off", "Not answering".
    public let powerWord: String
    /// Seconds until the idle power-off, when its countdown runs.
    public let powerOffIn: Double?
    /// Seconds the machine has been up, estimated from its last wake; nil when unknown.
    public let uptime: Double?
    /// "Text models": who has the GPU now.
    public let gpuUser: String?
    /// Requests running or waiting on the machine.
    public let busy: Int
    public let guardBusy: Bool
    public let holds: [HoldCard]
    public let loaded: [LoadedModel]
    public let problems: [Problem]
    /// How long a wake took, on average.
    public let wakeSeconds: Double?
    /// The machine's engines (lane names), for matching models to it.
    public let lanes: [String]
    /// Why the window cannot be sure of a GPU box's power right now ("last read 40 s ago"); nil when it can.
    /// Anything that could wake the box asks first while there is a doubt.
    public var powerDoubt: String? = nil
    /// The uptime is an estimate from the newest wake: the gateway did not say when the machine came up.
    public var uptimeApproximate: Bool = false

    /// On, and the window's reading of it is fresh: nothing sent to it can wake it.
    public var knownOn: Bool { power == .on && powerDoubt == nil }
    /// Models in memory now; a resting model is listed but not in memory.
    public var inMemory: Int { loaded.filter { $0.state == .loaded || $0.state == .loading }.count }
}

/// How far the window can trust the power it last read.
public struct PowerEvidence: Sendable, Equatable {
    /// When the status read that gave the power began; nil when there is none.
    public let readAt: Date?
    /// The last status read failed.
    public let failed: Bool
    /// The window came back from hidden and no status read has finished since.
    public let awaitingFresh: Bool
    public init(readAt: Date?, failed: Bool = false, awaitingFresh: Bool = false) {
        self.readAt = readAt; self.failed = failed; self.awaitingFresh = awaitingFresh
    }
}

public enum Machines {
    /// A machine may be powered off only after it has been up this long.
    public static let minimumUptime: Double = 240
    /// The durations the "Keep on" picker offers, the first being the default.
    public static let keepOnChoices = ["1h", "30m", "2h", "4h", "8h"]
    static let holdReason = "kept on from the Switchback window"

    /// Power older than this is unknown for anything that could wake a machine.
    public static let powerTrust: Double = 15
    /// An idle countdown this close to its end means the box may be asleep by the time a request arrives.
    public static let countdownTrust: Double = 30

    /// Every machine the status names. `evidence` says how old that status is; nil means it was just read.
    public static func cards(status: AIStatus?, engines: EnginesReport?, doctor: DoctorReport?, now: Date,
                             evidence: PowerEvidence? = nil) -> [MachineCard] {
        guard let status else { return [] }
        var order: [String] = status.power.map(\.machine)
        for lane in status.lanes where !order.contains(lane.machine) { order.append(lane.machine) }
        let kinds = Dictionary(uniqueKeysWithValues: order.map { machine in
            (machine, kind(of: status.lanes.filter { $0.machine == machine }))
        })
        // GPU boxes first, then the Mac; otherwise the gateway's order.
        order = order.filter { kinds[$0] == .gpuBox } + order.filter { kinds[$0] != .gpuBox }
        return order.map { machine in
            let lanes = status.lanes.filter { $0.machine == machine }
            let kind = kinds[machine] ?? .mac
            let sameKind = kinds.values.filter { $0 == kind }.count
            let title = sameKind > 1 ? kind.title + " · " + machine : kind.title
            let powerText = status.power.first { $0.machine == machine }?.power ?? "off"
            return card(machine: machine, title: title, kind: kind, powerText: powerText, lanes: lanes,
                        engines: engines, doctor: doctor, now: now, evidence: evidence)
        }
    }

    /// A machine the gateway wakes or powers off is a GPU box; any other is always on.
    static func kind(of lanes: [LaneReport]) -> MachineKind {
        lanes.contains { $0.wakeConfigured == true || $0.poweroffConfigured == true } ? .gpuBox : .mac
    }

    static func card(machine: String, title: String, kind: MachineKind, powerText: String, lanes: [LaneReport],
                     engines: EnginesReport?, doctor: DoctorReport?, now: Date, evidence: PowerEvidence? = nil) -> MachineCard {
        let power: Power = switch powerText {
        case "on": .on
        case "waking": .waking
        case "powering off": .poweringOff
        default: .asleep
        }
        let powerWord: String = switch (power, kind) {
        case (.on, _): "On"
        case (.waking, _): "Waking"
        case (.poweringOff, _): "Powering off"
        case (.asleep, .gpuBox): "Asleep"
        case (.asleep, .mac): "Not answering"
        }
        var holds: [HoldReport] = []
        for lane in lanes { for hold in lane.holds where !holds.contains(where: { $0.id == hold.id }) { holds.append(hold) } }
        // How old the reading is: a countdown keeps running after it was read.
        let age = evidence?.readAt.map { max(0, now.timeIntervalSince($0)) } ?? 0
        // A countdown only when nothing holds the machine and the gateway gives one.
        let countdown = holds.isEmpty ? lanes.compactMap(\.poweroffInSecs).filter { $0 >= 0 }.min().map { max(0, $0 - age) } : nil
        let nowMs = now.timeIntervalSince1970 * 1000
        let up = power == .on ? upSince(lanes) : nil
        let uptime = up.map { max(0, nowMs - $0.ms) / 1000 }
        let gpuLane = lanes.first { $0.holdsGPU == true }
        let machineLanes = lanes.map(\.name)
        let loaded: [LoadedModel] = (engines?.lanes ?? []).filter { machineLanes.contains($0.lane) }.flatMap { lane in
            lane.models.compactMap { model -> LoadedModel? in
                let state: LoadedModel.State
                switch model.status {
                case "loaded": state = .loaded
                case "loading": state = .loading
                case "sleeping": state = .resting
                default:
                    guard model.failed else { return nil }
                    state = .failed
                }
                return LoadedModel(target: model.target, name: model.id, state: state,
                                   memoryBytes: model.memory?.inUse?.bytes, context: model.memory?.inUse?.context,
                                   answering: model.inFlight)
            }
        }
        let holdCards = holds.map { hold -> HoldCard in
            let left = hold.expiresAtMs.map { max(0, $0 - nowMs) / 1000 }
            var until = ""
            if let expires = hold.expiresAtMs {
                until = "until " + Words.clock(Date(timeIntervalSince1970: expires / 1000), now: now)
                if let left { until += " · " + Words.duration(left) + " left" }
            }
            let release = ActionPlan(id: "release:\(hold.id)", title: "Release",
                                     command: Commands.release(hold.id, machine: machine),
                                     spoken: "Release the hold: " + (hold.reason ?? "kept on"))
            return HoldCard(id: hold.id, machine: machine, reason: hold.reason ?? "Kept on", owner: hold.owner,
                            secondsLeft: left, untilText: until, waking: hold.state == "waking",
                            wakeFailed: hold.state == "wake_failed" ? (hold.wakeDetail ?? "no detail") : nil,
                            release: release)
        }
        return MachineCard(
            machine: machine, kind: kind, title: title, power: power, powerWord: powerWord, powerOffIn: countdown,
            uptime: uptime, gpuUser: gpuLane.map { jobWords(lane: $0.name, engines: engines, doctor: doctor) },
            busy: lanes.reduce(0) { $0 + $1.inFlight + $1.queueDepth }, guardBusy: lanes.contains(where: \.poweroffGuardBusy),
            // A Mac shows no holds: there is nothing to release on a machine that is always on.
            holds: kind == .gpuBox ? holdCards : [], loaded: loaded,
            problems: problems(title: title, kind: kind, power: power, lanes: lanes, holds: holdCards,
                               engines: (engines?.lanes ?? []).filter { machineLanes.contains($0.lane) }),
            wakeSeconds: lanes.compactMap(\.wakeDurationAverageMs).max().map { $0 / 1000 },
            lanes: machineLanes,
            powerDoubt: kind == .gpuBox ? powerDoubt(power: power, countdown: countdown, evidence: evidence, now: now) : nil,
            uptimeApproximate: up?.approximate ?? false)
    }

    /// Why the window cannot be sure of a GPU box's power now, in a few words; nil when it can. The power is
    /// unknown when it was read more than 15 s ago, when the last read failed, when the window just came back
    /// from hidden and has not read it again, and when the idle power-off is under 30 s away.
    static func powerDoubt(power: Power, countdown: Double?, evidence: PowerEvidence?, now: Date) -> String? {
        if let evidence {
            if evidence.failed { return "the last reading failed" }
            if evidence.awaitingFresh { return "the window is reading it again" }
            guard let readAt = evidence.readAt else { return "it has not been read yet" }
            let age = now.timeIntervalSince(readAt)
            if age > powerTrust { return "it was last read \(Words.duration(age)) ago" }
        }
        if power == .on, let countdown, countdown < countdownTrust { return "it powers off in \(Words.duration(countdown))" }
        return nil
    }

    /// When the machine came up: the gateway's own time when it gives one. Otherwise the newest successful
    /// wake among its engines (plus how long it took), marked approximate: an engine can be started on a machine
    /// that was already up, so the newest wake may be later than the machine's start, never earlier. Power off
    /// then waits at least as long as it must.
    static func upSince(_ lanes: [LaneReport]) -> (ms: Double, approximate: Bool)? {
        if let reported = lanes.compactMap(\.upSinceMs).min() { return (reported, false) }
        let wakes = lanes.filter { ($0.lastWakeResult ?? "").hasPrefix("ok") }
            .compactMap { lane in lane.lastWakeMs.map { $0 + (lane.wakeDurationMs ?? 0) } }
        return wakes.max().map { ($0, true) }
    }

    /// "12 min", or "~12 min" when the uptime is an estimate from the newest wake.
    public static func uptimeText(_ machine: MachineCard) -> String? {
        machine.uptime.map { (machine.uptimeApproximate ? "~" : "") + Words.duration($0) }
    }

    /// One short line over every machine: "GPU box on · Mac on", "2 GPU boxes · 1 on · Mac not answering".
    public static func summary(_ cards: [MachineCard]) -> String {
        var parts: [String] = []
        let boxes = cards.filter { $0.kind == .gpuBox }
        if boxes.count == 1 { parts.append("GPU box " + boxes[0].powerWord.lowercased()) }
        if boxes.count > 1 {
            parts.append("\(boxes.count) GPU boxes")
            parts.append("\(boxes.filter { $0.power == .on }.count) on")
        }
        let macs = cards.filter { $0.kind == .mac }
        if macs.count == 1 { parts.append("Mac " + macs[0].powerWord.lowercased()) }
        if macs.count > 1 {
            let on = macs.filter { $0.power == .on }.count
            parts.append(on == macs.count ? "\(macs.count) Macs on" : "\(macs.count) Macs · \(macs.count - on) not answering")
        }
        return parts.joined(separator: " · ")
    }

    /// Who uses an engine, in everyday words: "Text models", "Images and video".
    static func jobWords(lane: String, engines: EnginesReport?, doctor: DoctorReport?) -> String {
        if engines?.lane(lane)?.kind.contains("llama") == true { return "Text models" }
        let ids = (doctor?.targets ?? []).filter { $0.lane == lane }.map(\.target)
        let media = ids.contains { $0.contains("/image/") || $0.contains("/video/") || $0.contains("/segment/") }
        if media { return "Images and video" }
        if ids.contains(where: { $0.contains("whisper") || $0.contains("audio") }) { return "Audio" }
        return lane.prefix(1).uppercased() + lane.dropFirst() + " models"
    }

    static func problems(title: String, kind: MachineKind, power: Power, lanes: [LaneReport], holds: [HoldCard],
                         engines: [EngineLane]) -> [Problem] {
        var found: [Problem] = []
        for lane in lanes where lane.state == "degraded" {
            let detail = lane.lastError.map { " · \(Words.plain($0))" } ?? ""
            found.append(Problem(text: "\(title): something needs attention" + detail, short: "Something needs attention" + detail,
                                 serious: true))
        }
        for hold in holds {
            if let failed = hold.wakeFailed {
                found.append(Problem(text: "\(title) did not wake · \(failed)", short: "Did not wake · \(failed)", serious: true))
            }
        }
        if power != .on, let failed = lanes.compactMap(\.lastWakeResult).first(where: { $0.hasPrefix("failed") || $0 == "timeout" }) {
            found.append(Problem(text: "\(title) did not wake last time · \(failed)", short: "Did not wake last time · \(failed)", serious: false))
        }
        if kind == .mac && power == .asleep {
            found.append(Problem(text: "The Mac’s engine is not answering", short: "Its engine is not answering", serious: true))
        }
        for engine in engines {
            if let error = engine.error {
                let words = "could not read its models · \(Words.plain(error))"
                found.append(Problem(text: "\(title): " + words, short: "Could not read its models · \(Words.plain(error))", serious: false))
            }
            if let error = engine.lastApplyError {
                let words = "saved settings did not reach the engine · \(Words.plain(error))"
                found.append(Problem(text: "\(title): " + words, short: "Saved settings did not reach the engine · \(Words.plain(error))",
                                     serious: true))
            }
            for model in engine.models where model.failed {
                found.append(Problem(text: "\(model.id) failed to load on the \(title)", short: "\(model.id) failed to load", serious: true))
            }
        }
        return found
    }

    /// "Power off": offered only for a GPU box, disabled with the reason while it must stay on.
    public static func powerOff(_ machine: MachineCard) -> ActionPlan? {
        guard machine.kind == .gpuBox else { return nil }
        let gate: Gate
        if machine.power == .asleep { gate = .disabled("Already asleep") }
        else if machine.power == .waking { gate = .disabled("Waking up") }
        else if machine.power == .poweringOff { gate = .disabled("Already powering off") }
        else if !machine.holds.isEmpty {
            gate = .disabled(machine.holds.count == 1 ? "A hold keeps it on · release it first"
                                                      : "\(machine.holds.count) holds keep it on · release them first")
        } else if machine.busy > 0 {
            gate = .disabled(machine.busy == 1 ? "Working on 1 request" : "Working on \(machine.busy) requests")
        } else if let uptime = machine.uptime, uptime < minimumUptime {
            gate = .disabled("Up \(uptimeText(machine) ?? Words.duration(uptime)) · it can power off after 4 min")
        } else if machine.guardBusy {
            gate = .disabled("Someone else is using it")
        } else { gate = .allowed }
        return ActionPlan(id: "power-off:\(machine.machine)", title: "Power off", command: Commands.off(machine.machine),
                          confirmation: Confirmation(title: "Power off the \(machine.title) now?",
                                                     message: "Loaded models are dropped. It wakes again when work arrives.",
                                                     confirm: "Power off", destructive: true),
                          gate: gate)
    }

    /// "Keep on" for a duration: a hold on a GPU box that is on, a hold with a wake on one that sleeps. When the
    /// window cannot be sure the box is on (old or failed reading, an idle power-off about to run), it asks first
    /// and keeps it on with a wake, which also covers a box that went to sleep meanwhile.
    public static func keepOn(_ machine: MachineCard, duration: String = keepOnChoices[0]) -> ActionPlan? {
        guard machine.kind == .gpuBox else { return nil }
        let words = durationWords(duration)
        if machine.power == .asleep || machine.power == .poweringOff {
            let title = WakeWarning.label("Keep on \(words)", wakes: true, machine: machine.kind.title)
            return ActionPlan(id: "keep-on:\(machine.machine)", title: title,
                              command: Commands.on(duration, reason: holdReason, machine: machine.machine), wakes: true,
                              confirmation: WakeWarning.confirmation("Keep on \(words)", machine: machine.title,
                                                                     wakeSeconds: machine.wakeSeconds))
        }
        if let doubt = machine.powerDoubt {
            let title = WakeWarning.mayWakeLabel("Keep on \(words)", machine: machine.kind.title)
            return ActionPlan(id: "keep-on:\(machine.machine)", title: title,
                              command: Commands.on(duration, reason: holdReason, machine: machine.machine), wakes: true,
                              confirmation: WakeWarning.mayWakeConfirmation(
                                "Keep on \(words)", machine: machine.title,
                                reason: "The window can’t be sure the \(machine.title) is on: \(doubt).",
                                wakeSeconds: machine.wakeSeconds))
        }
        return ActionPlan(id: "keep-on:\(machine.machine)", title: "Keep on \(words)",
                          command: Commands.hold(duration, reason: holdReason, machine: machine.machine))
    }

    /// "1h" → "1 h", "30m" → "30 min".
    public static func durationWords(_ duration: String) -> String {
        let hours = duration.hasSuffix("h") ? Int(duration.dropLast()) : nil
        let minutes = duration.hasSuffix("m") ? Int(duration.dropLast()) : nil
        if let hours { return "\(hours) h" }
        if let minutes { return "\(minutes) min" }
        return duration
    }

    /// The GPU memory a machine's models may use, when the gateway keeps an estimate for one of them.
    public static func memoryBudget(_ machine: MachineCard, settings: [String: SettingsView]) -> Double? {
        settings.values.flatMap(\.targets).filter { target in
            target.target.split(separator: "/").first.map { machine.lanes.contains(String($0)) } ?? false
        }.compactMap { $0.estimate?.budgetBytes }.max()
    }

    /// The machine a model's engine lives on.
    public static func machine(forLane lane: String, in cards: [MachineCard]) -> MachineCard? {
        cards.first { $0.lanes.contains(lane) }
    }
    /// The machine a target ("text/model-a") runs on: its engine is the part before the first `/`.
    public static func machine(ofTarget target: String, in cards: [MachineCard]) -> MachineCard? {
        target.split(separator: "/").first.flatMap { machine(forLane: String($0), in: cards) }
    }
}
