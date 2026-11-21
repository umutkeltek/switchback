import SwiftUI
import ControlPresentation
import UsagePresentation

/// One glance at what runs: each machine's power and loaded models, the account to use, holds and problems.
struct NowPlace: View {
    let store: ControlStore
    let state: ControlWindowState
    let accounts: UsagePanelState

    var body: some View {
        let machines = store.machines
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if machines.isEmpty {
                    EmptyPlace(symbol: store.readError == nil ? "hourglass" : "wifi.exclamationmark",
                               title: store.readError == nil ? "Reading what runs…" : "The gateway does not answer",
                               detail: store.readError == nil ? "This takes a moment the first time."
                                                              : "The window tries again every 10 seconds while it is open.")
                } else {
                    // Two machines a row, so names and states keep their room.
                    LazyVGrid(columns: [GridItem(.flexible(), spacing: 16, alignment: .top),
                                        GridItem(.flexible(), spacing: 16, alignment: .top)],
                              alignment: .leading, spacing: 16) {
                        ForEach(machines) { machine in
                            MachineTile(machine: machine, budget: Machines.memoryBudget(machine, settings: store.settings))
                        }
                    }
                }
                HStack(alignment: .top, spacing: 16) {
                    bestAccount.frame(maxWidth: .infinity)
                    holds(machines).frame(maxWidth: .infinity)
                }
                problems
            }
            .padding(.horizontal, WindowMetrics.gutter).padding(.bottom, WindowMetrics.gutter)
        }
    }

    private var bestAccount: some View {
        let data = accounts.directionData
        return VStack(alignment: .leading, spacing: 10) {
            CardTitle(title: "Account to use", symbol: "person.crop.circle.badge.checkmark")
            if let best = data.best {
                BestNowCard(best: best, picks: data.modelBest, style: .gauge) { state.place = .accounts }
            } else if let noRoom = data.noRoom {
                NoRoomCard(notice: noRoom) { state.place = .accounts }
            } else if let noFresh = data.noFresh {
                NoFreshCard(notice: noFresh, busy: accounts.busy, action: accounts.onRefresh)
            } else {
                Text("No account reading yet").font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
                    .frame(maxWidth: .infinity, minHeight: GaugeLayout.hero)
            }
        }
        .windowCard()
    }

    private func holds(_ machines: [MachineCard]) -> some View {
        let holds = machines.flatMap(\.holds)
        return VStack(alignment: .leading, spacing: 10) {
            CardTitle(title: "Holds", symbol: "lock", trailing: holds.isEmpty ? nil : "\(holds.count)")
            if holds.isEmpty {
                HStack(spacing: 8) {
                    Image(systemName: "lock.open").foregroundStyle(.tertiary).accessibilityHidden(true)
                    // A machine of unknown kind shows no holds: "nothing is held" would be a guess.
                    Text(machines.contains { $0.kind == .unknown } ? "Holds show only on a GPU box; the window can’t tell what these machines can do"
                         : machines.contains { $0.kind == .gpuBox } ? "Nothing keeps the GPU box on; it powers off when idle"
                         : "Nothing is held")
                        .font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
                }
                .frame(maxWidth: .infinity, minHeight: GaugeLayout.hero, alignment: .leading)
            } else {
                ForEach(holds) { hold in HoldRow(hold: hold, store: store) }
            }
        }
        .windowCard()
    }

    private var problems: some View {
        let problems = store.problems
        return VStack(alignment: .leading, spacing: 8) {
            CardTitle(title: "Needs attention", symbol: "exclamationmark.triangle", trailing: problems.isEmpty ? nil : "\(problems.count)")
            if problems.isEmpty {
                Label("Nothing needs attention", systemImage: "checkmark.circle.fill")
                    .font(.system(size: 12)).foregroundStyle(.green).labelStyle(TightLabelStyle())
            } else {
                ForEach(problems) { problem in ProblemRow(problem: problem) }
            }
        }
        .windowCard()
    }
}

struct ProblemRow: View {
    let problem: Problem
    /// Inside the machine's own card: the line without its name.
    var short = false
    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 7) {
            Image(systemName: problem.serious ? "exclamationmark.octagon.fill" : "exclamationmark.triangle.fill")
                .font(.system(size: 11)).foregroundStyle(problem.serious ? Color.red : Color.orange).accessibilityHidden(true)
            Text(short ? problem.short : problem.text).font(.system(size: 12)).lineLimit(2).truncationMode(.tail).textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
        .help(problem.text)
        .accessibilityElement(children: .combine)
    }
}

/// A machine at a glance: power, how long until it powers off, who has the GPU, and what is loaded.
struct MachineTile: View {
    let machine: MachineCard
    let budget: Double?
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 12) {
                PowerGlyph(power: machine.power, kind: machine.kind, size: 40)
                    // A reading the window cannot vouch for: the glyph dims with the status line.
                    .opacity(machine.powerCheck == nil ? 1 : 0.45)
                VStack(alignment: .leading, spacing: 2) {
                    Text(machine.title).font(.system(size: 15, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                    StatusText(machine: machine).lineLimit(1)
                }
                .layoutPriority(1)
                Spacer(minLength: 6)
                MachineWords.timer(machine)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel(machine.title + ": " + Machines.statusLine(machine).text)
            HStack(spacing: 6) {
                if let gpu = machine.gpuUser { Pill(text: "GPU: " + gpu, symbol: "memorychip") }
                if machine.busy > 0 { Pill(text: "\(machine.busy) working", symbol: "bolt.fill", tint: .orange) }
                if machine.kind == .mac { Pill(text: "Always on", symbol: "infinity") }
                if !machine.problems.isEmpty {
                    Pill(text: "\(machine.problems.count) \(machine.problems.count == 1 ? "problem" : "problems")",
                         symbol: "exclamationmark.triangle.fill", tint: .red)
                }
            }
            Rectangle().fill(WindowColor.hairline).frame(height: 1).accessibilityHidden(true)
            LoadedList(models: machine.loaded, budget: budget, lastLoaded: machine.lastLoaded, limit: 4)
        }
        .windowCard()
    }
}

/// A machine's models, each "In memory" (filled dot, with its memory) or "Resting" (hollow dot). A machine that
/// is not on lists none; what it had before it powered off shows muted as "Last loaded".
struct LoadedList: View {
    let models: [LoadedModel]
    let budget: Double?
    var lastLoaded: [String] = []
    var limit = 99
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if models.isEmpty && !lastLoaded.isEmpty {
                Text("Last loaded: " + lastLoaded.joined(separator: ", ")).font(.system(size: 12)).foregroundStyle(.tertiary)
                    .lineLimit(2).truncationMode(.tail)
                    .help("What its engine listed before it powered off; nothing is in memory now")
            } else if models.isEmpty {
                Text("Nothing in memory").font(.system(size: 12)).foregroundStyle(.tertiary)
            }
            ForEach(models.prefix(limit)) { model in
                HStack(spacing: 8) {
                    LoadDot(state: model.loadState)
                    Text(model.name).font(.system(size: 12, weight: .medium)).lineLimit(1).truncationMode(.middle)
                        .help(model.name)
                    if model.answering > 0 {
                        Image(systemName: "waveform").font(.system(size: 10)).foregroundStyle(.orange)
                            .help("Answering \(model.answering) now").accessibilityLabel("answering")
                    }
                    Spacer(minLength: 6)
                    if model.state == .failed {
                        Text(model.word).font(.system(size: 11, weight: .medium)).foregroundStyle(.red).fixedSize()
                    } else {
                        // Memory shows only when the engine measured it.
                        if model.state == .loaded, model.memoryBytes != nil {
                            MemoryBar(used: model.memoryBytes, budget: budget, width: 56)
                        }
                        Text(model.word).font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).fixedSize()
                            .help(model.state == .resting ? "Listed, not in memory: it loads again when a request comes"
                                  : model.state == .loading ? "Going into memory now" : "In memory now")
                    }
                }
                .accessibilityElement(children: .combine)
                .accessibilityLabel("\(model.name), \(model.word)")
            }
            if models.count > limit {
                Text("+\(models.count - limit) more").font(.system(size: 11)).foregroundStyle(.tertiary)
            }
            if let budget, models.contains(where: { $0.memoryBytes != nil }) {
                let used = models.compactMap(\.memoryBytes).reduce(0, +)
                HStack(spacing: 8) {
                    Text("GPU memory").font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
                    Spacer(minLength: 4)
                    MemoryBar(used: used, budget: budget, width: 110, showsBudget: true)
                }
                .padding(.top, 2)
            }
        }
    }
}

/// The line under a machine's name; muted when the window cannot vouch for the power it last read.
struct StatusText: View {
    let machine: MachineCard
    var body: some View {
        let line = Machines.statusLine(machine)
        Text(line.text).font(.system(size: 12))
            .foregroundStyle(line.muted ? AnyShapeStyle(.tertiary) : AnyShapeStyle(WindowColor.secondaryText))
            .help(line.muted ? "The window can’t vouch for this right now: " + (machine.powerDoubt ?? "") : "")
    }
}

enum MachineWords {
    /// "1 in memory" beside a machine's models; nil when none is.
    static func inMemory(_ machine: MachineCard) -> String? {
        machine.inMemory == 0 ? nil : "\(machine.inMemory) in memory"
    }
    /// The idle countdown as a ring, "Kept on" while a hold runs, nothing otherwise.
    @ViewBuilder static func timer(_ machine: MachineCard) -> some View {
        if let seconds = machine.powerOffIn {
            HStack(spacing: 6) {
                TimeRing(fraction: seconds / 900, tint: seconds < 120 ? .orange : .accentColor)
                VStack(alignment: .trailing, spacing: 0) {
                    Text("Powers off in").font(.system(size: 10)).foregroundStyle(WindowColor.secondaryText)
                    Text(Words.duration(seconds)).font(.system(size: 13, weight: .semibold, design: .rounded)).monospacedDigit()
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Powers off in " + Words.duration(seconds))
        } else if !machine.holds.isEmpty {
            Pill(text: "Kept on", symbol: "lock.fill", tint: .accentColor)
                .help("A hold keeps it on")
        }
    }
}

struct HoldRow: View {
    let hold: HoldCard
    let store: ControlStore
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: hold.wakeFailed != nil ? "exclamationmark.triangle.fill" : hold.waking ? "sunrise.fill" : "lock.fill")
                .font(.system(size: 12)).foregroundStyle(hold.wakeFailed != nil ? Color.red : Color.accentColor)
                .frame(width: 18).padding(.top, 5).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(hold.reason).font(.system(size: 12, weight: .medium)).lineLimit(1).truncationMode(.tail).help(hold.reason)
                Text(hold.wakeFailed.map { "Did not wake · " + $0 } ?? hold.untilText)
                    .font(.system(size: 11)).monospacedDigit().foregroundStyle(hold.wakeFailed != nil ? Color.red : WindowColor.secondaryText)
                    .lineLimit(1).truncationMode(.tail)
            }
            .layoutPriority(1)
            Spacer(minLength: 6)
            ActionButton(plan: hold.release, store: store, symbol: "lock.open", compact: true)
                .fixedSize()
        }
        .accessibilityElement(children: .contain)
    }
}

struct EmptyPlace: View {
    let symbol: String
    let title: String
    let detail: String
    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: symbol).font(.system(size: 26)).foregroundStyle(.tertiary).accessibilityHidden(true)
            Text(title).font(.system(size: 14, weight: .semibold))
            Text(detail).font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText).multilineTextAlignment(.center)
                .lineLimit(3).frame(maxWidth: 420)
        }
        .frame(maxWidth: .infinity).padding(.vertical, 36)
        .windowCard()
    }
}
