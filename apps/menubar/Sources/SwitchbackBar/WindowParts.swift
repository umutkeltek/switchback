import AppKit
import SwiftUI
import ControlPresentation

/// The window's sizes and colours: the gauge panel's language, at window scale.
enum WindowMetrics {
    static let width = 1080.0, height = 720.0
    static let minimumWidth = 900.0, minimumHeight = 600.0
    static let sidebar = 200.0
    static let gutter = 24.0
    static let cardRadius = 12.0
    static let list = 330.0
}
enum WindowColor {
    static let canvas = Color(nsColor: .windowBackgroundColor)
    static let card = Color(nsColor: .controlBackgroundColor)
    static let hairline = Color.primary.opacity(0.08)
    static let secondaryText = Color.primary.opacity(0.66)
    static let track = Color.primary.opacity(0.08)
    static func tone(_ verdict: FitHint.Verdict) -> Color {
        switch verdict {
        case .fits: .green
        case .tight: .orange
        case .wontFit: .red
        case .unknown: .secondary
        }
    }
}

/// A card: the panel's card at window size.
struct WindowCardStyle: ViewModifier {
    var padding = 16.0
    @Environment(\.displayScale) private var displayScale
    @Environment(\.colorSchemeContrast) private var contrast
    func body(content: Content) -> some View {
        content.padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(WindowColor.card, in: RoundedRectangle(cornerRadius: WindowMetrics.cardRadius))
            .overlay(RoundedRectangle(cornerRadius: WindowMetrics.cardRadius)
                .strokeBorder(Color.primary.opacity(contrast == .increased ? 0.32 : 0.08), lineWidth: 1 / displayScale))
    }
}
extension View {
    func windowCard(padding: Double = 16) -> some View { modifier(WindowCardStyle(padding: padding)) }
}

/// A small heading inside a card.
struct CardTitle: View {
    let title: String
    var symbol: String?
    var trailing: String?
    var body: some View {
        HStack(spacing: 6) {
            if let symbol { Image(systemName: symbol).font(.system(size: 11, weight: .semibold)).foregroundStyle(.secondary).accessibilityHidden(true) }
            Text(title).font(.system(size: 12, weight: .semibold)).foregroundStyle(WindowColor.secondaryText)
            Spacer(minLength: 4)
            if let trailing { Text(trailing).font(.system(size: 11)).monospacedDigit().foregroundStyle(.tertiary) }
        }
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }
}

/// The exact `sb` command behind a button: hover to read it, click to copy it.
struct CommandCopyButton: View {
    let command: ControlCommand
    @State private var copied = false
    @State private var hovered = false
    var body: some View {
        Button {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(command.display, forType: .string)
            copied = true
        } label: {
            Image(systemName: copied ? "checkmark" : "terminal")
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(copied ? Color.green : hovered ? Color.primary : Color.secondary)
                .frame(width: 22, height: 22)
                .background(hovered ? Color.primary.opacity(0.06) : .clear, in: RoundedRectangle(cornerRadius: 6))
                .contentShape(Rectangle())
        }
        .buttonStyle(PanelPressStyle()).focusable()
        .onHover { hovered = $0 }
        .help(command.display + (copied ? "\nCopied" : "\nClick to copy"))
        .accessibilityLabel(copied ? "Copied command" : "Copy command")
        .accessibilityValue(command.display)
        .onChange(of: command) { _, _ in copied = false }
    }
}

/// One result line under a button: what happened, or the reason `sb` gave.
struct ResultLine: View {
    let feedback: ActionFeedback?
    var body: some View {
        if let feedback, let line = feedback.line {
            let failed: Bool = if case .failed = feedback { true } else { false }
            HStack(alignment: .firstTextBaseline, spacing: 5) {
                Image(systemName: failed ? "exclamationmark.circle.fill" : "checkmark.circle.fill")
                    .font(.system(size: 10)).foregroundStyle(failed ? Color.orange : Color.green)
                    .accessibilityHidden(true)
                Text(line).font(.system(size: 11)).foregroundStyle(failed ? Color.primary : WindowColor.secondaryText)
                    .lineLimit(2).truncationMode(.tail).textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .help(line)
            .accessibilityElement(children: .combine)
            .accessibilityLabel((failed ? "Failed: " : "Done: ") + line)
        }
    }
}

/// A button for one action: its label (which says when it wakes the GPU box), a spinner while it runs, one
/// result line after, the reason when it may not run, the question first when it needs one, and its command.
struct ActionButton: View {
    enum Look { case prominent, plain, destructive }
    let plan: ActionPlan
    let store: ControlStore
    var look: Look = .plain
    var symbol: String?
    var showsReason = true
    var compact = false
    /// The plan as it is at the moment of the click, for actions whose question depends on how fresh the power
    /// reading is ("Keep on"); nil uses `plan`.
    var current: (() -> ActionPlan?)? = nil
    /// The plan the owner is being asked about.
    @State private var asking: ActionPlan?
    @State private var hovered = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private var feedback: ActionFeedback? { store.feedback[plan.id] }
    private var running: Bool { feedback == .running }
    private var enabled: Bool { plan.gate.isAllowed && !running }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 4) {
                Button { trigger() } label: { label }
                    .buttonStyle(PanelPressStyle()).focusable(enabled)
                    .disabled(!enabled)
                    .onHover { hovered = $0 }
                    .animation(reduceMotion ? nil : PanelMotion.feedback, value: hovered)
                    .help(plan.gate.reason ?? (plan.wakes ? (plan.confirmation?.title ?? "This wakes the GPU box") + " · asks first" : plan.title))
                    .accessibilityLabel(plan.spoken)
                    .accessibilityHint(plan.gate.reason ?? (plan.wakes ? (plan.confirmation?.title ?? "Wakes the GPU box") + ". Asks first." : ""))
                    .accessibilityValue(running ? "Running" : "")
                CommandCopyButton(command: plan.command)
            }
            if showsReason, let reason = plan.gate.reason {
                Label(reason, systemImage: "info.circle")
                    .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText).lineLimit(2)
                    .labelStyle(TightLabelStyle())
            }
            ResultLine(feedback: running ? nil : feedback)
        }
        .confirmationDialog(asking?.confirmation?.title ?? "", isPresented: Binding(get: { asking != nil }, set: { if !$0 { asking = nil } }),
                            titleVisibility: .visible, presenting: asking) { question in
            if let words = question.confirmation {
                Button(words.confirm, role: words.destructive ? .destructive : nil) { run(question, confirmed: true) }
                Button("Cancel", role: .cancel) {}
            }
        } message: { question in
            Text(question.confirmation?.message ?? "")
        }
    }

    private var label: some View {
        HStack(spacing: 6) {
            if running {
                ProgressView().controlSize(.mini).frame(width: 12, height: 12)
            } else if plan.wakes {
                Image(systemName: "sunrise.fill").font(.system(size: 11, weight: .semibold)).accessibilityHidden(true)
            } else if let symbol {
                Image(systemName: symbol).font(.system(size: 11, weight: .semibold)).accessibilityHidden(true)
            }
            Text(plan.title).font(.system(size: compact ? 11 : 12, weight: .semibold)).lineLimit(1)
        }
        .padding(.horizontal, compact ? 9 : 12).frame(height: compact ? 24 : 28)
        .foregroundStyle(foreground)
        .background(background, in: RoundedRectangle(cornerRadius: 7))
        .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(border, lineWidth: 1))
        .opacity(enabled || running ? 1 : 0.45)
        .contentShape(RoundedRectangle(cornerRadius: 7))
    }
    private var foreground: Color {
        switch look {
        case .prominent: plan.wakes ? .black.opacity(0.85) : .white
        case .destructive: .red
        case .plain: .primary
        }
    }
    private var background: Color {
        let lift = hovered && enabled ? 0.06 : 0
        switch look {
        case .prominent: return plan.wakes ? Color.yellow.opacity(0.85 + lift) : Color.accentColor.opacity(0.92 + lift)
        case .destructive: return Color.red.opacity(0.10 + lift)
        case .plain: return Color.primary.opacity(0.06 + lift)
        }
    }
    private var border: Color {
        switch look {
        case .prominent: .clear
        case .destructive: Color.red.opacity(0.25)
        case .plain: Color.primary.opacity(0.08)
        }
    }
    private func trigger() {
        let now: ActionPlan
        if let current {
            guard let fresh = current() else { return }
            now = fresh
        } else {
            now = plan
        }
        guard now.gate.isAllowed else { return }
        if now.confirmation != nil { asking = now } else { run(now, confirmed: false) }
    }
    private func run(_ plan: ActionPlan, confirmed: Bool) {
        Task { await store.perform(plan, confirmed: confirmed) }
    }
}

struct TightLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 4) { configuration.icon; configuration.title }
    }
}

/// Where a value comes from: engine default, saved for this model, a profile, or locked.
struct SourceBadgeView: View {
    let badge: SourceBadge
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: badge.symbol).font(.system(size: 8, weight: .bold))
            Text(badge.word).lineLimit(1)
        }
        .font(.system(size: 10, weight: .medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 6).frame(height: 18)
        .background(tint.opacity(0.12), in: Capsule())
        .fixedSize()
        .help(help)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(badge.word)
    }
    private var tint: Color {
        switch badge {
        case .engine: .secondary
        case .saved: .accentColor
        case .profile: .purple
        case .locked: .orange
        }
    }
    private var help: String {
        switch badge {
        case .engine: "Nothing is saved: the engine’s own value applies"
        case .saved: "Saved for this model"
        case .profile(let name): "Set by the profile \(name)"
        case .locked: "Saved and locked: a request cannot change it"
        }
    }
}

/// A loaded dot: filled green when loaded, hollow when resting, orange while loading, red when it failed.
struct LoadDot: View {
    let state: LoadState
    var size = 7.0
    var body: some View {
        Group {
            switch state {
            case .loaded: Circle().fill(Color.green)
            case .loading: Circle().fill(Color.orange)
            case .failed: Circle().fill(Color.red)
            case .resting: Circle().strokeBorder(Color.green, lineWidth: 1.5)
            case .unloaded: Circle().strokeBorder(Color.secondary.opacity(0.45), lineWidth: 1)
            case .none: Color.clear
            }
        }
        .frame(width: size, height: size)
        .help(state.word)
        .accessibilityHidden(true)
    }
}

/// Speed as a small bar against its ceiling; with no ceiling, lighter and against the fastest model in its group.
struct SpeedBarView: View {
    let speed: SpeedBar?
    var width = 46.0
    var height = 5.0
    var body: some View {
        ZStack(alignment: .leading) {
            Capsule().fill(WindowColor.track)
            if let speed {
                Capsule().fill(speed.againstCeiling ? tint(speed.fraction) : Color.accentColor.opacity(0.45))
                    .frame(width: max(height, width * speed.fraction))
            }
        }
        .frame(width: width, height: height)
        .overlay {
            if speed?.againstCeiling == false {
                Capsule().strokeBorder(Color.secondary.opacity(0.35), style: StrokeStyle(lineWidth: 0.75, dash: [2, 2]))
            }
        }
        .help(speed?.detail ?? "No speed measured yet")
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Speed")
        .accessibilityValue(speed.map { s in
            s.ceiling.map { "\(Int(s.tokensPerSecond)) tokens a second, \(Int((s.tokensPerSecond / $0 * 100).rounded())) percent of its ceiling" }
                ?? "\(Int(s.tokensPerSecond)) tokens a second, no ceiling saved"
        } ?? "not measured")
    }
    private func tint(_ fraction: Double) -> Color { fraction >= 0.7 ? .green : fraction >= 0.4 ? .orange : .red }
}

/// A machine's power as one glyph: on, asleep, waking, powering off.
struct PowerGlyph: View {
    let power: Power
    let kind: MachineKind
    var size = 36.0
    var body: some View {
        ZStack {
            Circle().fill(tint.opacity(0.14))
            Image(systemName: symbol).font(.system(size: size * 0.42, weight: .semibold)).foregroundStyle(tint)
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
    private var symbol: String {
        switch power {
        case .on: kind == .gpuBox ? "cpu.fill" : "desktopcomputer"
        case .asleep: kind == .gpuBox ? "moon.zzz.fill" : "exclamationmark.triangle.fill"
        case .waking: "sunrise.fill"
        case .poweringOff: "power"
        }
    }
    private var tint: Color {
        switch power {
        case .on: .green
        case .asleep: kind == .gpuBox ? .indigo : .red
        case .waking: .orange
        case .poweringOff: .secondary
        }
    }
}

/// Time left as a ring: the idle countdown or a hold's time left.
struct TimeRing: View {
    let fraction: Double
    var tint: Color = .accentColor
    var size = 18.0
    var body: some View {
        ZStack {
            Circle().stroke(WindowColor.track, lineWidth: 2.5)
            Circle().trim(from: 0, to: max(0.02, min(1, fraction)))
                .stroke(tint, style: StrokeStyle(lineWidth: 2.5, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

/// Memory used against the machine's budget when known; else a pill with the number.
struct MemoryBar: View {
    let used: Double?
    let budget: Double?
    var width = 120.0
    /// "33 of 24 GB" instead of "33 GB": for a machine's total.
    var showsBudget = false
    var body: some View {
        HStack(spacing: 6) {
            if let used, let budget, budget > 0 {
                ZStack(alignment: .leading) {
                    Capsule().fill(WindowColor.track)
                    Capsule().fill(used / budget > 0.92 ? Color.orange : Color.accentColor).frame(width: max(5, width * min(1, used / budget)))
                }
                .frame(width: width, height: 5)
            }
            // Memory shows only when the engine measured it.
            if let used {
                Text(showsBudget ? budget.map { Words.bytes(used).replacingOccurrences(of: " GB", with: "") + " of " + Words.bytes($0) } ?? Words.bytes(used)
                                 : Words.bytes(used))
                    .font(.system(size: 11, weight: .medium)).monospacedDigit()
                    .foregroundStyle(WindowColor.secondaryText)
                    .fixedSize()
            }
        }
        .help(used.map { u in budget.map { "\(Words.bytes(u)) of \(Words.bytes($0)) GPU memory" } ?? "\(Words.bytes(u)) of GPU memory" }
              ?? "The engine has not measured this model’s memory yet")
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Memory")
        .accessibilityValue(used.map { Words.bytes($0) + (budget.map { " of " + Words.bytes($0) } ?? "") } ?? "unknown")
    }
}

/// Where a model runs, as an icon and a word.
struct LocationLabel: View {
    let location: Location
    var showsWord = true
    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: location.symbol).font(.system(size: 10, weight: .medium))
            if showsWord { Text(location.word).lineLimit(1) }
        }
        .font(.system(size: 11)).foregroundStyle(WindowColor.secondaryText)
        .help("Runs on: " + location.word)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Runs on " + location.word)
    }
}

/// A small pill: a number or a short word with an icon.
struct Pill: View {
    let text: String
    var symbol: String?
    var tint: Color = .secondary
    var body: some View {
        HStack(spacing: 4) {
            if let symbol { Image(systemName: symbol).font(.system(size: 9, weight: .bold)) }
            Text(text).lineLimit(1)
        }
        .font(.system(size: 11, weight: .medium)).monospacedDigit()
        .foregroundStyle(tint == .secondary ? WindowColor.secondaryText : tint)
        .padding(.horizontal, 8).frame(height: 22)
        .background((tint == .secondary ? Color.primary : tint).opacity(tint == .secondary ? 0.06 : 0.12), in: Capsule())
        .fixedSize()
    }
}

/// The sidebar's material, or a solid colour for renders and Reduce Transparency.
struct SidebarBackground: NSViewRepresentable {
    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = .sidebar
        view.blendingMode = .behindWindow
        view.state = .followsWindowActiveState
        return view
    }
    func updateNSView(_ nsView: NSVisualEffectView, context: Context) {}
}
