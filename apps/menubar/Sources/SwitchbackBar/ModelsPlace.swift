import SwiftUI
import ControlPresentation

/// Every model by job, LM Studio-like: where it runs, a loaded dot, its speed and saved settings; the
/// selected model opens in the inspector.
struct ModelsPlace: View {
    let store: ControlStore
    @Bindable var state: ControlWindowState
    var interactive = true
    @FocusState private var searchFocused: Bool
    @FocusState private var listFocused: Bool

    var body: some View {
        let rows = store.rows
        let shown = filtered(rows)
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                search.padding(.horizontal, 14).padding(.bottom, 10)
                ScrollViewReader { scroller in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 1, pinnedViews: [.sectionHeaders]) {
                            ForEach(Job.allCases, id: \.self) { job in
                                let group = shown.filter { $0.job == job }
                                if !group.isEmpty {
                                    Section {
                                        ForEach(group) { row in
                                            ModelRowView(row: row, selected: store.selectedModel == row.name) { select(row) }
                                                .id(row.id)
                                        }
                                    } header: { JobHeader(job: job, count: group.count) }
                                }
                            }
                            if shown.isEmpty {
                                Text(rows.isEmpty ? (store.readError == nil ? "Reading the models…" : "No models: the gateway does not answer")
                                                  : "No model matches “\(state.search)”")
                                    .font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
                                    .frame(maxWidth: .infinity).padding(.vertical, 30)
                            }
                        }
                        .padding(.horizontal, 8).padding(.bottom, 12)
                    }
                    .focusable().focused($listFocused).focusEffectDisabled()
                    .onKeyPress(.downArrow) { move(1, in: shown, scroller: scroller); return .handled }
                    .onKeyPress(.upArrow) { move(-1, in: shown, scroller: scroller); return .handled }
                }
            }
            .frame(width: WindowMetrics.list)
            Rectangle().fill(WindowColor.hairline).frame(width: 1).accessibilityHidden(true)
            Group {
                if let row = rows.first(where: { $0.name == store.selectedModel }) {
                    InspectorView(store: store, state: state, row: row, interactive: interactive)
                        .id(row.id)
                } else {
                    VStack(spacing: 8) {
                        Image(systemName: "slider.horizontal.3").font(.system(size: 26)).foregroundStyle(.tertiary)
                        Text("Pick a model to see its settings").font(.system(size: 13)).foregroundStyle(WindowColor.secondaryText)
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background {
            Button("") { searchFocused = true }.keyboardShortcut("f", modifiers: .command).opacity(0).accessibilityHidden(true)
        }
        .onChange(of: state.focusSearch) { _, _ in searchFocused = true }
    }

    private var search: some View {
        HStack(spacing: 7) {
            Image(systemName: "magnifyingglass").font(.system(size: 12)).foregroundStyle(.tertiary).accessibilityHidden(true)
            TextField(text: $state.search, prompt: Text("Find a model").foregroundStyle(WindowColor.secondaryText)) { Text("Find a model") }
                .textFieldStyle(.plain).font(.system(size: 12)).focused($searchFocused)
                .accessibilityLabel("Find a model by name")
            if !state.search.isEmpty {
                Button { state.search = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary) }
                    .buttonStyle(PanelPressStyle()).focusable().help("Clear").accessibilityLabel("Clear search")
            }
        }
        .padding(.horizontal, 9).frame(height: 30)
        .background(Color.primary.opacity(0.045), in: RoundedRectangle(cornerRadius: 8))
    }

    private func filtered(_ rows: [ModelRow]) -> [ModelRow] {
        let query = state.search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !query.isEmpty else { return rows }
        return rows.filter { $0.name.lowercased().contains(query) || $0.targets.contains { $0.lowercased().contains(query) } }
    }
    private func select(_ row: ModelRow) {
        store.selectedModel = row.name
        state.reviewing = false
        listFocused = true
    }
    private func move(_ step: Int, in rows: [ModelRow], scroller: ScrollViewProxy) {
        guard !rows.isEmpty else { return }
        let index = rows.firstIndex { $0.name == store.selectedModel } ?? (step > 0 ? -1 : rows.count)
        let next = rows[max(0, min(rows.count - 1, index + step))]
        store.selectedModel = next.name
        state.reviewing = false
        scroller.scrollTo(next.id)
    }
}

struct JobHeader: View {
    let job: Job
    let count: Int
    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: job.symbol).font(.system(size: 10, weight: .semibold))
            Text(job.title.uppercased()).font(.system(size: 10, weight: .semibold)).tracking(0.4)
            Text("\(count)").font(.system(size: 10)).monospacedDigit().foregroundStyle(.tertiary)
            Spacer()
        }
        .foregroundStyle(WindowColor.secondaryText)
        .padding(.horizontal, 8).padding(.top, 10).padding(.bottom, 4)
        .background(WindowColor.canvas)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }
}

/// One model: loaded dot, name (a route marked apart), speed bar, where it runs, saved settings.
struct ModelRowView: View {
    let row: ModelRow
    let selected: Bool
    let action: () -> Void
    @State private var hovered = false
    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                LoadDot(state: row.load).frame(width: 8)
                if row.isRoute {
                    Image(systemName: "arrow.triangle.branch").font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(Color.purple).help("A route: picks one of \(row.targets.count) models per request")
                }
                Text(row.name).font(.system(size: 12, weight: selected ? .semibold : .regular))
                    .lineLimit(1).truncationMode(.middle).layoutPriority(1)
                Spacer(minLength: 4)
                if row.saved {
                    Image(systemName: "slider.horizontal.3").font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Color.accentColor).help("Has saved settings")
                }
                if row.speed != nil || row.job == .text {
                    SpeedBarView(speed: row.speed, width: 40, height: 4)
                }
                LocationLabel(location: row.location, showsWord: false).frame(width: 16)
            }
            .padding(.horizontal, 8).frame(height: 28)
            .background(selected ? Color.accentColor.opacity(0.16) : hovered ? Color.primary.opacity(0.045) : .clear,
                        in: RoundedRectangle(cornerRadius: 6))
            .contentShape(Rectangle())
        }
        .buttonStyle(PanelPressStyle()).focusable()
        .onHover { hovered = $0 }
        .help(row.name + (row.isRoute ? " · route over \(row.targets.joined(separator: ", "))" : row.standsFor.map { " · stands for \($0)" } ?? ""))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
    }
    private var spoken: String {
        var parts = [row.name]
        if row.isRoute { parts.append("route over \(row.targets.count) models") }
        if !row.load.word.isEmpty { parts.append(row.load.word) }
        parts.append("runs on " + row.location.word)
        if let speed = row.speed { parts.append(speed.word) }
        if row.saved { parts.append("has saved settings") }
        return parts.joined(separator: ", ")
    }
}
