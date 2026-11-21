import SwiftUI
import ControlPresentation

/// Each machine in full: a GPU box with its holds, "Keep on" and "Power off"; the Mac show-only.
struct MachinesPlace: View {
    let store: ControlStore
    @Bindable var state: ControlWindowState

    var body: some View {
        let machines = store.machines
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                if machines.isEmpty {
                    EmptyPlace(symbol: "server.rack", title: store.readError == nil ? "Reading the machines…" : "The gateway does not answer",
                               detail: store.readError == nil ? "This takes a moment the first time."
                                                              : "The window tries again every 10 seconds while it is open.")
                }
                ForEach(machines) { machine in
                    if machine.kind == .gpuBox {
                        GPUBoxCard(machine: machine, store: store, budget: Machines.memoryBudget(machine, settings: store.settings),
                                   duration: Binding(get: { state.keepOn[machine.machine] ?? Machines.keepOnChoices[0] },
                                                     set: { state.keepOn[machine.machine] = $0 }))
                    } else {
                        MacCard(machine: machine)
                    }
                }
            }
            .padding(.horizontal, WindowMetrics.gutter).padding(.bottom, WindowMetrics.gutter)
        }
    }
}

struct GPUBoxCard: View {
    let machine: MachineCard
    let store: ControlStore
    let budget: Double?
    @Binding var duration: String

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 14) {
                PowerGlyph(power: machine.power, kind: machine.kind, size: 48)
                    // A reading the window cannot vouch for: the glyph dims with the status line.
                    .opacity(machine.powerCheck == nil ? 1 : 0.45)
                VStack(alignment: .leading, spacing: 3) {
                    Text(machine.title).font(.system(size: 17, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                    StatusText(machine: machine)
                }
                .accessibilityElement(children: .combine)
                Spacer(minLength: 12)
                controls
            }
            stats
            if !machine.problems.isEmpty {
                VStack(alignment: .leading, spacing: 6) { ForEach(machine.problems) { ProblemRow(problem: $0, short: true) } }
            }
            HStack(alignment: .top, spacing: 16) {
                VStack(alignment: .leading, spacing: 10) {
                    CardTitle(title: "Holds", symbol: "lock", trailing: machine.holds.isEmpty ? nil : "\(machine.holds.count)")
                    if machine.holds.isEmpty {
                        Text("Nothing keeps it on").font(.system(size: 12)).foregroundStyle(.tertiary)
                    }
                    ForEach(machine.holds) { HoldRow(hold: $0, store: store) }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                VStack(alignment: .leading, spacing: 10) {
                    CardTitle(title: "Models", symbol: "square.stack.3d.up", trailing: MachineWords.inMemory(machine))
                    LoadedList(models: machine.loaded, budget: budget, lastLoaded: machine.lastLoaded)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .windowCard(padding: 20)
    }

    /// "Keep on" with its duration, and "Power off" with the reason it may not run yet. Side by side when they
    /// fit, else one above the other, so no label is cut.
    private var controls: some View {
        VStack(alignment: .trailing, spacing: 8) {
            ViewThatFits(in: .horizontal) {
                HStack(alignment: .top, spacing: 8) { keepControl; offControl }
                VStack(alignment: .trailing, spacing: 8) { keepControl; offControl }
            }
            if let reason = Machines.powerOff(machine)?.gate.reason {
                Label("Power off: " + reason, systemImage: "info.circle")
                    .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).labelStyle(TightLabelStyle())
                    .lineLimit(2).multilineTextAlignment(.trailing)
            }
        }
        .frame(maxWidth: 460, alignment: .trailing)
    }

    @ViewBuilder private var keepControl: some View {
        if let keep = Machines.keepOn(machine, duration: duration) {
            HStack(alignment: .top, spacing: 2) {
                // With a hold already in place, keeping it on longer is not the main thing to do here.
                ActionButton(plan: keep, store: store, look: machine.holds.isEmpty ? .prominent : .plain, symbol: "lock.fill",
                             click: { await store.clickKeepOn(machine.machine, duration: duration) })
                // How long: a small menu beside the button, so the duration shows once, on the button.
                Menu {
                    Picker("Keep on for", selection: $duration) {
                        ForEach(Machines.keepOnChoices, id: \.self) { Text("Keep on " + Machines.durationWords($0)).tag($0) }
                    }
                    .pickerStyle(.inline)
                } label: {
                    Image(systemName: "chevron.down").font(.system(size: 10, weight: .semibold))
                        .frame(width: 22, height: 28)
                }
                .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                .help("How long to keep it on").accessibilityLabel("How long to keep it on")
            }
        }
    }

    @ViewBuilder private var offControl: some View {
        if let off = Machines.powerOff(machine) {
            ActionButton(plan: off, store: store, look: .destructive, symbol: "power", showsReason: false)
        }
    }

    /// Only the tiles that mean something now: an asleep box has no uptime or countdown to show.
    private var stats: some View {
        HStack(spacing: 10) {
            ForEach(Machines.stats(machine)) { stat in
                Stat(title: stat.title, value: stat.value, symbol: stat.symbol, help: stat.help, tint: stat.busy ? .orange : nil)
            }
        }
    }
}

struct Stat: View {
    let title: String
    let value: String
    let symbol: String
    var help = ""
    var tint: Color?
    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Label(title, systemImage: symbol).font(.system(size: 10, weight: .medium)).foregroundStyle(WindowColor.secondaryText)
                .labelStyle(TightLabelStyle())
            Text(value).font(.system(size: 14, weight: .semibold, design: .rounded)).monospacedDigit()
                .foregroundStyle(tint ?? .primary).lineLimit(1).minimumScaleFactor(0.75)
        }
        .padding(.horizontal, 12).padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 9))
        .help(help)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(title): \(value)")
    }
}

/// The Mac, or a machine whose kind the window cannot tell: its models, in memory or resting. Show-only: no
/// power, holds or load settings.
struct MacCard: View {
    let machine: MachineCard
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center, spacing: 14) {
                PowerGlyph(power: machine.power, kind: machine.kind, size: 48)
                VStack(alignment: .leading, spacing: 3) {
                    Text(machine.title).font(.system(size: 17, weight: .semibold))
                    Text(machine.kind == .mac ? (machine.power == .on ? "On · always on" : machine.powerWord)
                                              : Machines.statusLine(machine).text)
                        .font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
                }
                Spacer()
                Label("Show only", systemImage: "eye")
                    .font(.system(size: 11, weight: .medium)).foregroundStyle(WindowColor.secondaryText)
                    .padding(.horizontal, 8).frame(height: 22).background(Color.primary.opacity(0.06), in: Capsule())
                    .help(machine.capabilityNote
                          ?? "The Mac runs its models by itself; this window does not power it, hold it or change what it loads")
            }
            .accessibilityElement(children: .combine)
            if let note = machine.capabilityNote {
                Label(note, systemImage: "questionmark.circle")
                    .font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
                    .fixedSize(horizontal: false, vertical: true).labelStyle(TightLabelStyle())
            }
            if !machine.problems.isEmpty {
                VStack(alignment: .leading, spacing: 6) { ForEach(machine.problems) { ProblemRow(problem: $0, short: true) } }
            }
            CardTitle(title: "Models", symbol: "square.stack.3d.up", trailing: MachineWords.inMemory(machine))
            LoadedList(models: machine.loaded, budget: nil, lastLoaded: machine.lastLoaded)
        }
        .windowCard(padding: 20)
    }
}
