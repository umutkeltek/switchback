import AppKit
import SwiftUI
import Observation
import ControlPresentation
import UsagePresentation

/// The window's four places.
enum WindowPlace: String, CaseIterable, Identifiable {
    case now, models, machines, accounts
    var id: String { rawValue }
    var title: String {
        switch self {
        case .now: "Now"
        case .models: "Models"
        case .machines: "Machines"
        case .accounts: "Accounts"
        }
    }
    var symbol: String {
        switch self {
        case .now: "gauge.with.dots.needle.50percent"
        case .models: "square.stack.3d.up"
        case .machines: "server.rack"
        case .accounts: "person.2"
        }
    }
    var shortcut: KeyEquivalent {
        switch self {
        case .now: "1"
        case .models: "2"
        case .machines: "3"
        case .accounts: "4"
        }
    }
}

/// What the owner is doing in the window: the place, the open model's edits, prompts and choices.
@Observable @MainActor final class ControlWindowState {
    var place: WindowPlace = .now
    var search = ""
    /// Edits waiting for "Apply", by model (target).
    var drafts: [String: SettingsDraft] = [:]
    /// For a route, the model whose settings the inspector shows.
    var routeTarget: [String: String] = [:]
    var prompts: [String: String] = [:]
    /// The "Keep on" duration picked per machine.
    var keepOn: [String: String] = [:]
    /// The change preview is open.
    var reviewing = false
    var focusSearch = 0
}

struct ControlWindowView: View {
    let store: ControlStore
    @Bindable var state: ControlWindowState
    let accounts: UsagePanelState
    var interactive = true
    var solid = false
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    var body: some View {
        HStack(spacing: 0) {
            sidebar.frame(width: WindowMetrics.sidebar)
            Rectangle().fill(WindowColor.hairline).frame(width: 1).accessibilityHidden(true)
            VStack(spacing: 0) {
                header
                Group {
                    switch state.place {
                    case .now: NowPlace(store: store, state: state, accounts: accounts)
                    case .models: ModelsPlace(store: store, state: state, interactive: interactive)
                    case .machines: MachinesPlace(store: store, state: state)
                    case .accounts: AccountsPlace(accounts: accounts, interactive: interactive, solid: solid)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }
            .background(WindowColor.canvas)
        }
        .frame(minWidth: WindowMetrics.minimumWidth, minHeight: WindowMetrics.minimumHeight)
        .background(WindowColor.canvas)
        .background {
            // The keyboard: Command-1 to 4 pick a place, Command-R reads again now.
            Group {
                ForEach(WindowPlace.allCases) { place in
                    Button("") { state.place = place }.keyboardShortcut(place.shortcut, modifiers: .command)
                }
                Button("") { Task { await store.readNow() } }.keyboardShortcut("r", modifiers: .command)
            }
            .opacity(0).accessibilityHidden(true)
        }
    }

    // MARK: Sidebar

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 2) {
            // Room for the window's buttons above the title.
            Color.clear.frame(height: 30)
            HStack(spacing: 8) {
                Image(systemName: "arrow.triangle.swap").font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Color.accentColor)
                    .frame(width: 26, height: 26)
                    .background(Color.accentColor.opacity(0.12), in: RoundedRectangle(cornerRadius: 7))
                    .accessibilityHidden(true)
                Text("Switchback").font(.system(size: 14, weight: .semibold))
            }
            .padding(.horizontal, 14).padding(.bottom, 14)
            ForEach(WindowPlace.allCases) { place in
                SidebarRow(place: place, selected: state.place == place, accessory: accessory(place)) { state.place = place }
            }
            Spacer(minLength: 8)
            freshness.padding(.horizontal, 14).padding(.bottom, 12)
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .background {
            if solid || reduceTransparency { Color.primary.opacity(0.035).background(WindowColor.canvas) } else { SidebarBackground() }
        }
    }

    private func accessory(_ place: WindowPlace) -> SidebarRow.Accessory? {
        switch place {
        case .now:
            let problems = store.problems
            return problems.isEmpty ? nil : .count(problems.count, problems.contains(where: \.serious) ? .red : .orange)
        case .models:
            let loaded = store.rows.filter { $0.load == .loaded && !$0.isRoute }.count
            return loaded == 0 ? nil : .text("\(loaded) loaded")
        case .machines:
            guard let box = store.machines.first(where: { $0.kind == .gpuBox }) else { return nil }
            let color: Color = switch box.power {
            case .on: .green
            case .waking: .orange
            case .asleep: .indigo
            case .poweringOff: .secondary
            }
            return .dot(color, box.powerWord)
        case .accounts:
            guard let best = accounts.directionData.best else { return nil }
            return .text(best.roomText + "%")
        }
    }

    private var freshness: some View {
        HStack(spacing: 6) {
            if store.reading {
                ProgressView().controlSize(.mini).frame(width: 10, height: 10)
            } else {
                Circle().fill(store.readError == nil ? Color.green.opacity(0.8) : Color.orange).frame(width: 6, height: 6)
            }
            Text(freshnessText).font(.system(size: 10)).foregroundStyle(WindowColor.secondaryText).lineLimit(2)
        }
        .help(store.readError ?? "Reads about every 10 seconds while this window is open")
        .accessibilityElement(children: .combine)
    }
    private var freshnessText: String {
        if store.readError != nil { return "Can’t reach the gateway" }
        guard let last = store.lastRead else { return store.reading ? "Reading…" : "Not read yet" }
        return "Updated " + Words.ago(store.now.timeIntervalSince(last))
    }

    // MARK: Header

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(state.place.title).font(.system(size: 20, weight: .semibold))
                Text(subtitle).font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText).lineLimit(1)
            }
            Spacer()
        }
        .padding(.horizontal, WindowMetrics.gutter).padding(.top, 18).padding(.bottom, 14)
    }
    private var subtitle: String {
        switch state.place {
        case .now:
            let machines = store.machines
            if machines.isEmpty { return store.readError == nil ? "Reading what runs…" : "Nothing to show until the gateway answers" }
            return machines.map { "\($0.title) \($0.powerWord.lowercased())" }.joined(separator: " · ")
        case .models:
            let rows = store.rows
            let routes = rows.filter(\.isRoute).count
            return rows.isEmpty ? "No models listed" : "\(rows.count) names · \(routes) \(routes == 1 ? "route" : "routes")"
        case .machines: return "Power, holds and what each machine has loaded"
        case .accounts: return "Which account to use now, and how much room each has"
        }
    }
}

struct SidebarRow: View {
    enum Accessory { case count(Int, Color), text(String), dot(Color, String) }
    let place: WindowPlace
    let selected: Bool
    let accessory: Accessory?
    let action: () -> Void
    @State private var hovered = false
    var body: some View {
        Button(action: action) {
            HStack(spacing: 9) {
                Image(systemName: place.symbol).font(.system(size: 13, weight: .medium))
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary).frame(width: 20)
                Text(place.title).font(.system(size: 13, weight: selected ? .semibold : .regular))
                Spacer(minLength: 4)
                switch accessory {
                case .count(let count, let color):
                    Text("\(count)").font(.system(size: 10, weight: .bold)).monospacedDigit().foregroundStyle(.white)
                        .padding(.horizontal, 6).frame(minWidth: 18, minHeight: 16).background(color, in: Capsule())
                case .text(let text):
                    Text(text).font(.system(size: 11)).monospacedDigit().foregroundStyle(.secondary)
                case .dot(let color, let word):
                    Circle().fill(color).frame(width: 7, height: 7).help(word)
                case nil: EmptyView()
                }
            }
            .padding(.horizontal, 10).frame(height: 30)
            .background(selected ? Color.accentColor.opacity(0.14) : hovered ? Color.primary.opacity(0.05) : .clear,
                        in: RoundedRectangle(cornerRadius: 7))
            .contentShape(Rectangle())
        }
        .buttonStyle(PanelPressStyle()).focusable()
        .onHover { hovered = $0 }
        .padding(.horizontal, 8)
        .help("\(place.title) (⌘\(String(place.shortcut.character)))")
        .accessibilityLabel(place.title + accessibilitySuffix)
        .accessibilityAddTraits(selected ? [.isSelected, .isButton] : .isButton)
    }
    private var accessibilitySuffix: String {
        switch accessory {
        case .count(let count, _): ", \(count) \(count == 1 ? "problem" : "problems")"
        case .text(let text): ", " + text
        case .dot(_, let word): ", GPU box " + word.lowercased()
        case nil: ""
        }
    }
}
