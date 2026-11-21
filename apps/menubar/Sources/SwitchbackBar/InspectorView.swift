import AppKit
import SwiftUI
import ControlPresentation

/// The selected model: its settings with where each comes from, load settings for engines that take them,
/// a preview before "Apply", "Try it", its speed findings and its history.
struct InspectorView: View {
    let store: ControlStore
    @Bindable var state: ControlWindowState
    let row: ModelRow
    var interactive = true

    private var view: SettingsView? { store.settings[row.name] }
    private var target: TargetSettings? {
        guard let targets = view?.targets, !targets.isEmpty else { return nil }
        let chosen = state.routeTarget[row.name]
        return targets.first { $0.target == chosen } ?? targets.first { $0.target == row.name } ?? targets[0]
    }
    private var machines: [MachineCard] { store.machines }

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    header
                    if let target {
                        if state.drafts[target.target] == nil { ResultLine(feedback: store.feedback["apply:" + target.target]) }
                        if view?.isRoute == true { routePicker }
                        AnswerCard(target: target, draft: draftBinding(target.target),
                                   engineModel: store.engines?.model(target.target)?.model)
                        if let load = Inspector.loadSettings(target, engines: store.engines, machines: machines) {
                            LoadCard(target: target, rows: load, draft: draftBinding(target.target),
                                     engineModel: store.engines?.model(target.target)?.model)
                        } else if target.engineLane != nil {
                            Label(loadNote(target), systemImage: "info.circle")
                                .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).labelStyle(TightLabelStyle())
                        }
                    } else if row.job == .text || row.job == .audio {
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text("Reading its settings…").font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
                        }
                        .windowCard()
                    } else {
                        Label("\(row.job.title) models have no settings here", systemImage: "info.circle")
                            .font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText).windowCard()
                    }
                    if row.job == .text { TryItCard(store: store, state: state, row: row, machines: machines) }
                    SpeedCard(row: row, store: store, state: state, target: target)
                    if let target, let history = store.histories[target.target], !history.entries.isEmpty {
                        HistoryCard(store: store, history: history)
                    }
                }
                .padding(.horizontal, 20).padding(.bottom, 20)
            }
            if let target, let draft = state.drafts[target.target], !draft.isEmpty {
                ApplyBar(store: store, state: state, target: target, draft: draft, machines: machines)
            }
        }
    }

    private func draftBinding(_ target: String) -> Binding<SettingsDraft> {
        Binding(get: { state.drafts[target] ?? SettingsDraft() },
                set: { state.drafts[target] = $0.isEmpty ? nil : $0 })
    }

    private func loadNote(_ target: TargetSettings) -> String {
        let lane = target.engineLane ?? ""
        if let machine = Machines.machine(forLane: lane, in: machines), machine.kind == .mac {
            return "The Mac’s engine chooses how it loads models; this window only shows them"
        }
        return "This engine only lists its models; load settings are not offered"
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top, spacing: 10) {
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 8) {
                        LoadDot(state: row.load, size: 9)
                        Text(row.name).font(.system(size: 18, weight: .semibold)).lineLimit(2).truncationMode(.middle)
                            .textSelection(.enabled)
                    }
                    if let standsFor = row.standsFor {
                        Text("Stands for " + standsFor).font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).lineLimit(1)
                    }
                }
                .layoutPriority(1)
                Spacer(minLength: 0)
            }
            HStack(alignment: .top, spacing: 6) {
                Pill(text: row.location.word + (row.elsewhere > 0 ? " +\(row.elsewhere)" : ""), symbol: row.location.symbol)
                if !row.load.word.isEmpty {
                    Pill(text: row.load.word, symbol: row.load == .failed ? "exclamationmark.triangle.fill" : "circle.fill",
                         tint: row.load == .loaded ? .green : row.load == .failed ? .red : .secondary)
                }
                if row.isRoute { Pill(text: "Route · picks 1 of \(row.targets.count)", symbol: "arrow.triangle.branch", tint: .purple) }
                if row.saved { Pill(text: "Saved settings", symbol: "slider.horizontal.3", tint: .accentColor) }
                Spacer(minLength: 8)
                if let target {
                    let actions = Inspector.loadActions(target, engines: store.engines, machines: machines)
                    if let load = actions.load { ActionButton(plan: load, store: store, look: .prominent, symbol: "arrow.down.circle").fixedSize() }
                    if let unload = actions.unload { ActionButton(plan: unload, store: store, symbol: "eject", showsReason: false).fixedSize() }
                }
            }
            if let target, let reason = Inspector.loadActions(target, engines: store.engines, machines: machines).unload?.gate.reason {
                HStack { Spacer(); Label("Unload: " + reason, systemImage: "info.circle").labelStyle(TightLabelStyle()) }
                    .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
            }
        }
        .padding(.top, 4)
    }

    private var routePicker: some View {
        HStack(spacing: 8) {
            Text("Settings for").font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText)
            Picker("Settings for", selection: Binding(get: { target?.target ?? "" },
                                                      set: { state.routeTarget[row.name] = $0; state.reviewing = false })) {
                ForEach(view?.targets.map(\.target) ?? [], id: \.self) { name in
                    Text(name + " · " + ModelList.location(ofTarget: name, machines: machines).word).tag(name)
                }
            }
            .labelsHidden().pickerStyle(.menu).fixedSize()
            Spacer()
        }
        .help("A route picks one of its models per request; settings belong to each model")
    }
}

// MARK: Setting rows

/// The value a control shows: the draft when edited, else what is set, else the engine's own value.
private func shownValue(_ setting: SettingState, draft: SettingsDraft) -> JSONValue? {
    switch draft.values[setting.key] {
    case .set(let value): value
    case .reset: setting.shown
    case nil: setting.value ?? setting.shown
    }
}

/// Title, badge, an "edited" dot and "back to engine default".
private struct SettingLabel: View {
    let setting: SettingState
    @Binding var draft: SettingsDraft
    var body: some View {
        HStack(spacing: 6) {
            if draft.values[setting.key] != nil {
                Circle().fill(Color.accentColor).frame(width: 6, height: 6).help("Changed, not applied yet")
                    .accessibilityLabel("changed")
            }
            Text(setting.title).font(.system(size: 12, weight: .medium))
            SourceBadgeView(badge: draft.values[setting.key] == nil ? setting.badge
                            : draft.values[setting.key] == .reset ? .engine : .saved)
            Spacer(minLength: 4)
            if setting.gate.isAllowed && (setting.value != nil || draft.values[setting.key] != nil) && draft.values[setting.key] != .reset {
                Button {
                    if setting.value == nil { draft.values[setting.key] = nil } else { draft.values[setting.key] = .reset }
                } label: {
                    Image(systemName: "arrow.uturn.backward").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
                        .frame(width: 20, height: 20).contentShape(Rectangle())
                }
                .buttonStyle(PanelPressStyle()).focusable()
                .help(setting.value == nil ? "Undo this change" : "Back to the engine’s own value")
                .accessibilityLabel((setting.value == nil ? "Undo change to " : "Reset ") + setting.title)
            }
        }
    }
}

private struct SliderSetting: View {
    let setting: SettingState
    @Binding var draft: SettingsDraft
    let range: ClosedRange<Double>
    var step = 0.05
    var body: some View {
        let face = Inspector.face(setting, draft: draft) { $0.double.map(JSONValue.numberText) ?? Words.value($0) }
        VStack(alignment: .leading, spacing: 4) {
            SettingLabel(setting: setting, draft: $draft)
            HStack(spacing: 10) {
                if let value = face.value?.double {
                    Slider(value: Binding(get: { min(range.upperBound, max(range.lowerBound, value)) },
                                          set: { draft.values[setting.key] = .set(.number(($0 / step).rounded() * step)) }),
                           in: range)
                        .controlSize(.small)
                        .disabled(!setting.gate.isAllowed)
                        .opacity(setting.gate.isAllowed ? 1 : 0.5)
                        .accessibilityLabel(setting.title)
                        .accessibilityValue(face.text + ", " + (face.source == .engine ? "the engine's own value" : setting.badge.word))
                    Text(face.text).font(.system(size: 12, weight: .semibold, design: .rounded))
                        .monospacedDigit().foregroundStyle(face.source == .engine ? .secondary : .primary)
                        .frame(width: 42, alignment: .trailing)
                } else {
                    // Nothing saved and the engine's value unknown: no handle and no made-up position.
                    NeutralTrack(range: range, step: step, enabled: setting.gate.isAllowed) {
                        draft.values[setting.key] = .set(.number($0))
                    }
                    .accessibilityLabel(setting.title)
                    Color.clear.frame(width: 42, height: 1)
                }
            }
            if let reason = setting.gate.reason { ReasonLine(reason) }
        }
    }
}

/// A slider's track while the engine decides: dashed, no handle, "Engine decides". A click or drag on it starts a
/// value there; until then nothing is set.
private struct NeutralTrack: View {
    let range: ClosedRange<Double>
    let step: Double
    let enabled: Bool
    let start: (Double) -> Void
    var body: some View {
        GeometryReader { geometry in
            ZStack {
                Capsule().strokeBorder(Color.secondary.opacity(0.45), style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
                    .frame(height: 6)
                Text("Engine decides").font(.system(size: 10, weight: .medium)).foregroundStyle(WindowColor.secondaryText)
                    .padding(.horizontal, 6).background(WindowColor.card)
            }
            .frame(width: geometry.size.width, height: geometry.size.height)
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0).onChanged { drag in
                guard enabled, geometry.size.width > 0 else { return }
                let fraction = min(1, max(0, drag.location.x / geometry.size.width))
                start(value(at: fraction))
            })
        }
        .frame(height: 20)
        .opacity(enabled ? 1 : 0.5)
        .help(enabled ? "Nothing is saved: the engine decides. Click or drag to choose a value." : "")
        .accessibilityElement(children: .ignore)
        .accessibilityValue("Engine decides")
        .accessibilityHint(enabled ? "Adjust to choose a value" : "")
        .accessibilityAdjustableAction { _ in if enabled { start(value(at: 0.5)) } }
    }
    private func value(at fraction: Double) -> Double {
        let raw = range.lowerBound + fraction * (range.upperBound - range.lowerBound)
        return (raw / step).rounded() * step
    }
}

private struct StepperSetting: View {
    let setting: SettingState
    @Binding var draft: SettingsDraft
    let range: ClosedRange<Double>
    let steps: [Double]
    var words: (Double) -> String = { JSONValue.numberText($0) }
    var body: some View {
        // "auto" until a value is saved, chosen or known from the engine; the first press starts at a sensible value.
        let face = Inspector.face(setting, draft: draft, neutral: "auto") { $0.double.map(words) ?? Words.value($0) }
        VStack(alignment: .leading, spacing: 4) {
            SettingLabel(setting: setting, draft: $draft)
            HStack(spacing: 10) {
                Text(face.text).font(.system(size: 13, weight: .semibold, design: .rounded)).monospacedDigit()
                    .foregroundStyle(face.source == .draft || face.source == .saved ? .primary : .secondary)
                Spacer()
                Stepper(setting.title, onIncrement: { press(up: true) }, onDecrement: { press(up: false) })
                    .labelsHidden().controlSize(.small).disabled(!setting.gate.isAllowed)
                    .accessibilityLabel(setting.title)
                    .accessibilityValue((face.neutral ? "the engine decides" : face.text) + ", " + setting.badge.word)
            }
            if let reason = setting.gate.reason { ReasonLine(reason) }
        }
    }
    private func press(up: Bool) {
        if let next = Inspector.step(setting, draft: draft, steps: steps, range: range, up: up) {
            draft.values[setting.key] = .set(.number(next))
        }
    }
}

/// Thinking: Engine, On or Off. "Engine" is what nothing saved means; the window never shows "Off" for it.
private struct ThinkingSetting: View {
    let setting: SettingState
    @Binding var draft: SettingsDraft
    var body: some View {
        let choice = Inspector.thinking(setting, draft: draft)
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                SettingLabel(setting: setting, draft: $draft)
                Picker(setting.title, selection: Binding(get: { choice },
                                                        set: { Inspector.choose($0, for: setting, in: &draft) })) {
                    ForEach(ThinkingChoice.allCases) { Text($0.title).tag($0) }
                }
                .labelsHidden().pickerStyle(.segmented).fixedSize().controlSize(.small)
                .disabled(!setting.gate.isAllowed)
                .accessibilityLabel(setting.title)
                .accessibilityValue(choice == .engine ? "the engine decides" : choice.title)
            }
            if let reason = setting.gate.reason { ReasonLine(reason) }
        }
    }
}

private struct ToggleSetting: View {
    let setting: SettingState
    @Binding var draft: SettingsDraft
    var detail: String?
    var body: some View {
        let on = shownValue(setting, draft: draft)?.bool ?? false
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                SettingLabel(setting: setting, draft: $draft)
                Toggle(setting.title, isOn: Binding(get: { on }, set: { draft.values[setting.key] = .set(.bool($0)) }))
                    .labelsHidden().toggleStyle(.switch).controlSize(.mini).disabled(!setting.gate.isAllowed)
                    .accessibilityLabel(setting.title)
                    .accessibilityValue((on ? "on" : "off") + ", " + setting.badge.word)
            }
            if let detail { Text(detail).font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText) }
            if let reason = setting.gate.reason { ReasonLine(reason) }
        }
    }
}

private struct ReasonLine: View {
    let reason: String
    init(_ reason: String) { self.reason = reason }
    var body: some View {
        Label(reason, systemImage: setting(reason) ? "lock" : "info.circle")
            .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).labelStyle(TightLabelStyle()).lineLimit(2)
    }
    private func setting(_ text: String) -> Bool { text.hasPrefix("Set by") }
}

// MARK: Cards

private struct AnswerCard: View {
    let target: TargetSettings
    @Binding var draft: SettingsDraft
    let engineModel: EngineModel?
    var body: some View {
        let rows = Dictionary(uniqueKeysWithValues: Inspector.answerSettings(target, engineModel: engineModel).map { ($0.key, $0) })
        VStack(alignment: .leading, spacing: 14) {
            CardTitle(title: "Answers", symbol: "text.bubble", trailing: "take effect on the next answer")
            if let row = rows["temperature"] { SliderSetting(setting: row, draft: $draft, range: Inspector.range("temperature")) }
            if let row = rows["top_p"] { SliderSetting(setting: row, draft: $draft, range: Inspector.range("top_p"), step: 0.01) }
            if let row = rows["max_tokens"] {
                StepperSetting(setting: row, draft: $draft, range: Inspector.range("max_tokens"),
                               steps: [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072]) { Words.tokens(Int($0)) + " tokens" }
            }
            if let row = rows["reasoning.enabled"] { ThinkingSetting(setting: row, draft: $draft) }
            if let guardRow = rows["reasoning_guard.enabled"] {
                let on = shownValue(guardRow, draft: draft)?.bool ?? false
                ToggleSetting(setting: guardRow, draft: $draft,
                              detail: "Turns thinking off when an answer is allowed fewer tokens than the minimum, so it never comes back empty")
                if on, let minimum = rows["reasoning_guard.min_tokens"] {
                    StepperSetting(setting: minimum, draft: $draft, range: Inspector.range("reasoning_guard.min_tokens"),
                                   steps: [256, 512, 1024, 2048, 4096, 8192]) { Words.tokens(Int($0)) + " tokens" }
                        .padding(.leading, 14)
                }
            }
        }
        .windowCard()
    }
}

private struct LoadCard: View {
    let target: TargetSettings
    let rows: [SettingState]
    @Binding var draft: SettingsDraft
    let engineModel: EngineModel?
    static let contexts: [Double] = [2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144]
    var body: some View {
        let byKey = Dictionary(uniqueKeysWithValues: rows.map { ($0.key, $0) })
        VStack(alignment: .leading, spacing: 14) {
            CardTitle(title: "Loading", symbol: "memorychip",
                      trailing: engineModel?.status == "loaded" ? "takes effect on next load" : "takes effect when it loads")
            if let context = byKey["load.ctx_size"] { contextRow(context, cache: byKey["load.cache_type_k"]) }
            if let cache = byKey["load.cache_type_k"] {
                VStack(alignment: .leading, spacing: 6) {
                    SettingLabel(setting: cache, draft: $draft)
                    Picker("KV cache", selection: Binding(
                        get: { shownValue(cache, draft: draft)?.string ?? "f16" },
                        set: { draft.values[cache.key] = .set(.string($0)) })) {
                        ForEach(Inspector.cacheTypes, id: \.self) { type in Text(type).tag(type) }
                        if let current = shownValue(cache, draft: draft)?.string, !Inspector.cacheTypes.contains(current) {
                            Text(current).tag(current)
                        }
                    }
                    .labelsHidden().pickerStyle(.segmented).frame(maxWidth: 260).disabled(!cache.gate.isAllowed)
                    .accessibilityLabel("KV cache type")
                    Text("Smaller types fit longer contexts in the same memory").font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
                }
            }
            if let parallel = byKey["load.parallel"] {
                StepperSetting(setting: parallel, draft: $draft, range: Inspector.range("load.parallel"),
                               steps: (1...16).map(Double.init)) { "\(Int($0)) at once" }
            }
        }
        .windowCard()
    }

    private func contextRow(_ context: SettingState, cache: SettingState?) -> some View {
        let range = Inspector.range(context.key, target: target)
        let steps = LoadCard.contexts.filter { range.contains($0) }
        // No handle and no verdict until a size is chosen or the engine's own size is known.
        let face = Inspector.contextFace(context, cache: cache, draft: draft, steps: steps, target: target, engineModel: engineModel)
        return VStack(alignment: .leading, spacing: 6) {
            SettingLabel(setting: context, draft: $draft)
            HStack(spacing: 10) {
                if let index = face.index {
                    Slider(value: Binding(get: { Double(index) },
                                          set: { draft.values[context.key] = .set(.number(steps[safe: Int($0.rounded())] ?? steps.last ?? 32768)) }),
                           in: 0...Double(max(1, steps.count - 1)), step: 1)
                        .controlSize(.small).disabled(!context.gate.isAllowed || steps.count < 2)
                        .accessibilityLabel("Context size")
                        .accessibilityValue(face.text + " tokens" + (face.fit.map { ", " + $0.text } ?? ""))
                    Text(face.text).font(.system(size: 12, weight: .semibold, design: .rounded))
                        .monospacedDigit().frame(width: 42, alignment: .trailing)
                } else {
                    NeutralTrack(range: 0...Double(max(1, steps.count - 1)), step: 1, enabled: context.gate.isAllowed && steps.count > 1) {
                        draft.values[context.key] = .set(.number(steps[safe: Int($0)] ?? steps.last ?? 32768))
                    }
                    .accessibilityLabel("Context size")
                    Color.clear.frame(width: 42, height: 1)
                }
            }
            if let fit = face.fit { FitBar(fit: fit) }
            if let reason = context.gate.reason { ReasonLine(reason) }
        }
    }
}

/// "Fits / won't fit in memory" as a bar against the budget.
private struct FitBar: View {
    let fit: FitHint
    var body: some View {
        HStack(spacing: 8) {
            if let needed = fit.neededBytes, let budget = fit.budgetBytes, budget > 0 {
                GeometryReader { geometry in
                    ZStack(alignment: .leading) {
                        Capsule().fill(WindowColor.track)
                        Capsule().fill(WindowColor.tone(fit.verdict)).frame(width: max(6, geometry.size.width * min(1, needed / budget)))
                    }
                }
                .frame(width: 150, height: 6)
            } else {
                Capsule().strokeBorder(Color.secondary.opacity(0.4), style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
                    .frame(width: 150, height: 6)
            }
            Image(systemName: fit.verdict == .wontFit ? "xmark.octagon.fill" : fit.verdict == .tight ? "exclamationmark.triangle.fill"
                  : fit.verdict == .fits ? "checkmark.circle.fill" : "questionmark.circle")
                .font(.system(size: 10)).foregroundStyle(WindowColor.tone(fit.verdict))
            Text(fit.text).font(.system(size: 11)).monospacedDigit().foregroundStyle(WindowColor.secondaryText).lineLimit(1)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Memory: " + fit.text)
    }
}

/// The edits waiting, with "Review" opening the preview: what changes, whether it reloads, the commands.
private struct ApplyBar: View {
    let store: ControlStore
    @Bindable var state: ControlWindowState
    let target: TargetSettings
    let draft: SettingsDraft
    let machines: [MachineCard]
    @State private var askingDiscard = false
    var body: some View {
        let plan = Inspector.plan(target: target, draft: draft, engines: store.engines, machines: machines)
        let id = "apply:" + target.target
        let feedback = store.feedback[id]
        VStack(alignment: .leading, spacing: 0) {
            if state.reviewing { preview(plan, id: id, feedback: feedback) }
            HStack(spacing: 10) {
                Circle().fill(Color.accentColor).frame(width: 7, height: 7).accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 1) {
                    Text(plan.isEmpty ? "No change" : "\(plan.lines.count) \(plan.lines.count == 1 ? "change" : "changes") to \(target.target)")
                        .font(.system(size: 12, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                    Text(plan.notes.last ?? "").font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).lineLimit(1)
                }
                Spacer(minLength: 8)
                Button("Discard") { discard() }
                    .controlSize(.regular)
                Button(state.reviewing ? "Hide" : "Review…") { state.reviewing.toggle() }
                    .controlSize(.regular).buttonStyle(.borderedProminent).disabled(plan.isEmpty)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 16).frame(height: 54)
        }
        .background(.bar)
        .overlay(alignment: .top) { Rectangle().fill(WindowColor.hairline).frame(height: 1) }
        .background {
            // Escape asks before unsaved edits are thrown away.
            Button("") { askingDiscard = true }.keyboardShortcut(.cancelAction).opacity(0).accessibilityHidden(true)
        }
        .confirmationDialog(discardQuestion(plan), isPresented: $askingDiscard, titleVisibility: .visible) {
            Button("Discard", role: .destructive) { discard() }
            Button("Keep editing", role: .cancel) {}
        } message: { Text("Nothing has been applied to \(target.target) yet.") }
    }

    private func discardQuestion(_ plan: ChangePlan) -> String {
        plan.lines.count == 1 ? "Discard 1 unsaved change?" : "Discard \(max(plan.lines.count, draft.values.count)) unsaved changes?"
    }
    private func discard() {
        state.drafts[target.target] = nil
        state.reviewing = false
    }

    private func preview(_ plan: ChangePlan, id: String, feedback: ActionFeedback?) -> some View {
        let loaded = store.engines?.model(target.target)?.model.status == "loaded"
        let loadChange = plan.lines.contains { $0.key.hasPrefix("load.") }
        return VStack(alignment: .leading, spacing: 10) {
            Text("What changes").font(.system(size: 12, weight: .semibold)).foregroundStyle(WindowColor.secondaryText)
            ForEach(plan.lines) { line in
                HStack(spacing: 8) {
                    Text(line.title).font(.system(size: 12, weight: .medium)).frame(width: 120, alignment: .leading).lineLimit(1)
                    Text(line.from).font(.system(size: 12)).foregroundStyle(WindowColor.secondaryText).strikethrough(!line.from.hasPrefix("engine default"))
                        .lineLimit(1).truncationMode(.middle)
                    Image(systemName: "arrow.right").font(.system(size: 9, weight: .bold)).foregroundStyle(.tertiary)
                    Text(line.to).font(.system(size: 12, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                    Spacer(minLength: 0)
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("\(line.title): from \(line.from) to \(line.to)")
            }
            ForEach(plan.notes, id: \.self) { note in
                Label(note, systemImage: note.hasPrefix("Reloads") ? "arrow.clockwise.circle.fill" : "clock")
                    .font(.system(size: 11)).foregroundStyle(note.hasPrefix("Reloads") ? Color.orange : WindowColor.secondaryText)
                    .labelStyle(TightLabelStyle())
            }
            if loaded && loadChange {
                Picker("When", selection: Binding(get: { draft.apply },
                                                  set: { var d = draft; d.apply = $0; state.drafts[target.target] = d })) {
                    Text("On next load").tag(ApplyMode.nextLoad)
                    Text("Reload now").tag(ApplyMode.now)
                }
                .pickerStyle(.segmented).frame(maxWidth: 260).labelsHidden()
                .accessibilityLabel("When the loaded model takes the change")
            }
            VStack(alignment: .leading, spacing: 4) {
                ForEach(plan.actions) { action in
                    HStack(spacing: 6) {
                        Text(action.command.display).font(.system(size: 10, design: .monospaced)).lineLimit(3).truncationMode(.middle)
                            .fixedSize(horizontal: false, vertical: true)
                            .textSelection(.enabled).help(action.command.display)
                        Spacer(minLength: 2)
                        CommandCopyButton(command: action.command)
                    }
                    .padding(.horizontal, 8).padding(.vertical, 5).frame(minHeight: 26)
                    .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 6))
                }
            }
            HStack(spacing: 10) {
                ResultLine(feedback: feedback == .running ? nil : feedback)
                Spacer()
                Button {
                    Task {
                        let outcome = await store.apply(plan, id: id)
                        if case .done = outcome { state.drafts[target.target] = nil; state.reviewing = false }
                    }
                } label: {
                    HStack(spacing: 6) {
                        if feedback == .running { ProgressView().controlSize(.mini) }
                        Text(plan.reloads ? "Apply and reload" : "Apply")
                    }
                }
                .buttonStyle(.borderedProminent).disabled(plan.isEmpty || feedback == .running)
                .accessibilityHint(plan.notes.joined(separator: ". "))
            }
        }
        .padding(16)
        .overlay(alignment: .bottom) { Rectangle().fill(WindowColor.hairline).frame(height: 1) }
    }
}

private struct TryItCard: View {
    let store: ControlStore
    @Bindable var state: ControlWindowState
    let row: ModelRow
    let machines: [MachineCard]
    /// The plan the owner is being asked about, made at the click.
    @State private var asking: ActionPlan?
    var body: some View {
        let prompt = state.prompts[row.name] ?? ""
        let plan = store.tryIt(row, prompt: prompt)
        let chat = store.chats[row.name]
        VStack(alignment: .leading, spacing: 10) {
            CardTitle(title: "Try it", symbol: "paperplane")
            HStack(alignment: .bottom, spacing: 8) {
                TextField("Ask something short", text: Binding(get: { prompt }, set: { state.prompts[row.name] = $0 }), axis: .vertical)
                    .textFieldStyle(.plain).font(.system(size: 12)).lineLimit(1...4)
                    .padding(8).background(Color.primary.opacity(0.045), in: RoundedRectangle(cornerRadius: 8))
                    .onSubmit { send() }
                    .accessibilityLabel("Prompt for \(row.name)")
                Button { send() } label: {
                    HStack(spacing: 5) {
                        if chat == .running { ProgressView().controlSize(.mini) }
                        else { Image(systemName: plan.wakes ? "sunrise.fill" : "paperplane.fill").font(.system(size: 11)) }
                        Text(plan.title).font(.system(size: 12, weight: .semibold)).lineLimit(1)
                    }
                    .padding(.horizontal, 12).frame(height: 30)
                    .foregroundStyle(plan.wakes ? Color.black.opacity(0.85) : .white)
                    .background(plan.wakes ? Color.yellow.opacity(0.85) : Color.accentColor, in: RoundedRectangle(cornerRadius: 8))
                    .opacity(plan.gate.isAllowed && chat != .running ? 1 : 0.45)
                }
                .buttonStyle(PanelPressStyle()).focusable().disabled(!plan.gate.isAllowed || chat == .running)
                .keyboardShortcut(.return, modifiers: .command)
                .help(plan.gate.reason ?? (plan.confirmation.map { $0.title + " · asks first (⌘Return)" } ?? "Send (⌘Return)"))
                .accessibilityLabel(plan.title)
                .accessibilityHint(plan.confirmation == nil ? "" : "Asks first")
                CommandCopyButton(command: plan.command)
            }
            if let note = Inspector.tryItNote(targets: row.targets, machines: machines) {
                Label(note, systemImage: "desktopcomputer").font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
                    .labelStyle(TightLabelStyle())
            }
            switch chat {
            case .answered(let answer):
                VStack(alignment: .leading, spacing: 8) {
                    ScrollView {
                        Text(answer.answer.isEmpty ? "(empty answer)" : answer.answer).font(.system(size: 12))
                            .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 140).fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 6) {
                        if let speed = answer.tokensPerSecond { Pill(text: Words.speed(speed), symbol: "bolt.fill", tint: .green) }
                        Pill(text: "\(answer.completionTokens) tokens", symbol: "text.word.spacing")
                        if let first = answer.firstTokenMs { Pill(text: "first token \(Int(first)) ms", symbol: "timer") }
                        if let route = answer.route { Pill(text: route, symbol: "arrow.triangle.branch", tint: .purple).help("The model that answered") }
                    }
                }
                .padding(10).background(Color.primary.opacity(0.03), in: RoundedRectangle(cornerRadius: 8))
            case .failed(let line): ResultLine(feedback: .failed(line))
            case .running, nil: EmptyView()
            }
        }
        .windowCard()
        .confirmationDialog(asking?.confirmation?.title ?? "", isPresented: Binding(get: { asking != nil }, set: { if !$0 { asking = nil } }),
                            titleVisibility: .visible, presenting: asking) { question in
            Button(question.confirmation?.confirm ?? "Send") { Task { await store.send(question, model: row.name, confirmed: true) } }
            Button("Cancel", role: .cancel) {}
        } message: { question in Text(question.confirmation?.message ?? "") }
    }
    /// Decides at the click, from the power as it is now: a reading that went stale since the button was drawn
    /// asks first.
    private func send() {
        let plan = store.tryIt(row, prompt: state.prompts[row.name] ?? "")
        guard plan.gate.isAllowed else { return }
        if plan.confirmation != nil { asking = plan } else { Task { await store.send(plan, model: row.name) } }
    }
}

private struct SpeedCard: View {
    let row: ModelRow
    let store: ControlStore
    @Bindable var state: ControlWindowState
    let target: TargetSettings?
    var body: some View {
        let report = (target.map(\.target) ?? row.targets.first).flatMap { store.doctor?.speed($0) }
        let findings = Inspector.findings(store.doctor, targets: target.map { [$0.target] } ?? row.targets)
        VStack(alignment: .leading, spacing: 12) {
            CardTitle(title: "Speed", symbol: "speedometer", trailing: report.map { "\($0.requests) requests measured" })
            if let speed = row.speed {
                HStack(alignment: .center, spacing: 14) {
                    VStack(alignment: .leading, spacing: 0) {
                        Text(String(Words.speed(speed.tokensPerSecond).dropLast(6))).font(.system(size: 26, weight: .semibold, design: .rounded))
                            .monospacedDigit()
                        Text("tokens a second").font(.system(size: 10)).foregroundStyle(WindowColor.secondaryText)
                    }
                    VStack(alignment: .leading, spacing: 5) {
                        SpeedBarView(speed: speed, width: 200, height: 8)
                        Text(speed.ceiling.map { "\(Int((speed.tokensPerSecond / $0 * 100).rounded()))% of its \(Words.speed($0)) ceiling" }
                             ?? "No ceiling saved · bar compares with the fastest model")
                            .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
                    }
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("Speed: " + speed.detail)
                HStack(spacing: 6) {
                    if let first = report?.firstTokenMs { Pill(text: "first token \(Int(first)) ms", symbol: "timer") }
                    if let prompt = report?.promptTPS { Pill(text: "reads \(Words.speed(prompt))", symbol: "doc.text") }
                }
            } else {
                Text("No speed measured yet").font(.system(size: 12)).foregroundStyle(.tertiary)
            }
            ForEach(findings) { finding in
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: "lightbulb.fill").font(.system(size: 11)).foregroundStyle(.yellow).padding(.top, 2).accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 5) {
                        Text(finding.advice).font(.system(size: 12, weight: .semibold))
                        Text(finding.text).font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
                            .fixedSize(horizontal: false, vertical: true)
                        HStack(spacing: 6) {
                            if let suggestion = finding.draft, let target {
                                Button("Use this") {
                                    var draft = state.drafts[target.target] ?? SettingsDraft()
                                    for (key, value) in suggestion.values where key != "reasoning" { draft.values[key] = value }
                                    state.drafts[target.target] = draft
                                }
                                .controlSize(.small)
                                .help("Puts the suggested settings in the inspector; nothing changes until Apply")
                            }
                            if let command = finding.command { TextCopyButton(text: command) }
                        }
                    }
                }
                .padding(10).background(Color.yellow.opacity(0.07), in: RoundedRectangle(cornerRadius: 8))
                .accessibilityElement(children: .contain)
            }
        }
        .windowCard()
    }
}

/// Copies a command the speed report suggested; the window never runs it as text.
private struct TextCopyButton: View {
    let text: String
    @State private var copied = false
    var body: some View {
        Button {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            copied = true
        } label: {
            Label(copied ? "Copied" : "Copy command", systemImage: copied ? "checkmark" : "terminal")
        }
        .controlSize(.small).help(text)
    }
}

private struct HistoryCard: View {
    let store: ControlStore
    let history: SettingsHistory
    var body: some View {
        let rows = Inspector.versions(history, now: store.now, machines: store.machines)
        VStack(alignment: .leading, spacing: 8) {
            CardTitle(title: "History", symbol: "clock.arrow.circlepath", trailing: "\(rows.count) versions")
            ForEach(rows.prefix(8)) { row in
                HStack(alignment: .top, spacing: 10) {
                    Circle().fill(row.isCurrent ? Color.accentColor : Color.secondary.opacity(0.35)).frame(width: 7, height: 7).padding(.top, 5)
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 1) {
                        HStack(spacing: 6) {
                            Text(row.title).font(.system(size: 12, weight: .semibold))
                            if row.isCurrent { Pill(text: "Now", tint: .accentColor) }
                            Text(row.when).font(.system(size: 11)).foregroundStyle(.tertiary)
                        }
                        Text(row.what).font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).lineLimit(1).truncationMode(.tail)
                    }
                    .layoutPriority(1)
                    Spacer(minLength: 6)
                    if let undo = row.undo { ActionButton(plan: undo, store: store, symbol: "arrow.uturn.backward", compact: true).fixedSize() }
                }
                .accessibilityElement(children: .contain)
            }
            if rows.count > 8 { Text("+\(rows.count - 8) older").font(.system(size: 11)).foregroundStyle(.tertiary) }
        }
        .windowCard()
    }
}

extension Array {
    subscript(safe index: Int) -> Element? { indices.contains(index) ? self[index] : nil }
}
