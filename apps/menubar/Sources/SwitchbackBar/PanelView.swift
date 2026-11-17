import AppKit
import SwiftUI
import Observation
import UsagePresentation

enum PanelMetrics {
    static let width = 360.0
    static let maximumHeight = 600.0
    static let minimumHeight = 300.0
    static let statusIconWidth = 24.0
    static let statusTextWidth = 96.0
}
enum PanelMotion {
    static let feedback = Animation.timingCurve(0.23, 1, 0.32, 1, duration: 0.12)
    static func disclosure(reduceMotion: Bool, pointerInitiated: Bool) -> Animation? {
        guard !reduceMotion && pointerInitiated else { return nil }
        return .timingCurve(0.23, 1, 0.32, 1, duration: 0.16)
    }
    static var pointerInitiated: Bool {
        guard let event = NSApp.currentEvent else { return false }
        return event.type == .leftMouseUp || event.type == .leftMouseDown
    }
}
@Observable @MainActor final class UsagePanelState {
    var presentation = PanelPresentation()
    var selectedProvider = "all"
    var query = ""
    var focusSearchRequest = 0
    var pinnedOnly = false
    var busy = false
    var failure: String?
    var checkedText = "Waiting for a reading"
    var iconOnly = true
    var limitNotices = false
    var maximumHeight = PanelMetrics.maximumHeight
    @ObservationIgnored var onResize: (Double) -> Void = { _ in }
    var expandedAccounts: Set<String> = []
    var preferredHeight: Double {
        let groups = visibleProviders
        var content = groups.isEmpty ? 128.0 : 20.0
        for provider in groups {
            content += 20
            if provider.advice != nil { content += 20 }
            if provider.accounts.isEmpty { content += 66 }
            for account in provider.accounts {
                content += account.email?.isEmpty == false ? 92 : 84
                if account.primary.contains(where: { $0.qualifier != nil }) { content += 12 }
                if expandedAccounts.contains(account.key) { content += min(320, Double(account.extras.count) * 58 + 60) }
            }
            content += Double(max(0, provider.accounts.count - 1)) * 6
        }
        content += Double(max(0, groups.count - 1)) * 14
        return min(maximumHeight, max(PanelMetrics.minimumHeight, 164 + content + (failure == nil ? 0 : 40)))
    }
    func toggleExpansion(_ key: String) {
        if expandedAccounts.contains(key) { expandedAccounts.remove(key) } else { expandedAccounts.insert(key) }
    }

    @ObservationIgnored var onPin: (String) -> Void = { _ in }
    @ObservationIgnored var onRefresh: () -> Void = {}
    @ObservationIgnored var onQuit: () -> Void = {}
    @ObservationIgnored var onClose: () -> Void = {}
    @ObservationIgnored var onProviderChange: (String) -> Void = { _ in }
    @ObservationIgnored var onIconOnly: (Bool) -> Void = { _ in }
    @ObservationIgnored var onLimitNotices: (Bool) -> Void = { _ in }

    var visibleProviders: [ProviderPanel] {
        presentation.filtered(provider: selectedProvider, query: query, pinnedOnly: pinnedOnly)
    }
}
private enum PanelColor {
    static let canvas = Color(nsColor: .windowBackgroundColor)
    static let card = Color(nsColor: .controlBackgroundColor)
    static let hairline = Color.primary.opacity(0.075)
    static let secondaryText = Color.primary.opacity(0.70)
}
struct UsagePanelView: View {
    @Bindable var state: UsagePanelState
    @FocusState private var searchFocused: Bool
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.displayScale) private var displayScale
    var interactive = true
    var solidBackground = false

    var body: some View {
        VStack(spacing: 0) {
            header
            providerFilters.padding(.horizontal, 12).padding(.bottom, 8)
            search.padding(.horizontal, 12).padding(.bottom, 8)
            PanelRule()
            if let failure = state.failure {
                HStack(spacing: 7) {
                    Image(systemName: "exclamationmark.circle")
                    Text(state.presentation.accountCount > 0 ? "Couldn’t update. Showing saved readings." : "Couldn’t load account readings.").lineLimit(2)
                    Spacer(minLength: 0)
                }
                .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
                .padding(.horizontal, 12).padding(.vertical, 8).help(failure)
            }
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    ForEach(state.visibleProviders, id: \.key) { provider in
                        ProviderSection(provider: provider, state: state)
                    }
                    if state.visibleProviders.isEmpty {
                        VStack(spacing: 8) {
                            Image(systemName: "magnifyingglass").font(.system(size: 24)).foregroundStyle(.tertiary)
                            Text(state.busy && state.presentation.providers.isEmpty ? "Reading your accounts…" : state.query.isEmpty && !state.pinnedOnly && state.selectedProvider == "all" ? "No account readings yet" : "No matching accounts").font(.system(size: 13, weight: .medium))
                            Text(state.presentation.providers.isEmpty ? "Usage will appear here after a successful read." : "Try another provider or search term.").font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
                            if !state.presentation.providers.isEmpty {
                                Button("Show all accounts") {
                                    state.query = ""; state.selectedProvider = "all"; state.pinnedOnly = false
                                }.buttonStyle(.link).font(.system(size: 11))
                            }
                        }
                        .frame(maxWidth: .infinity).padding(.vertical, 35)
                    }
                }
                .padding(12)
            }
            .scrollIndicators(.automatic)
            PanelRule()
            footer
        }
        .frame(width: PanelMetrics.width, height: state.preferredHeight)
        .background {
            if reduceTransparency || solidBackground { PanelColor.canvas }
            else { Rectangle().fill(.regularMaterial) }
        }
        .onExitCommand { state.onClose() }
        .onChange(of: state.preferredHeight) { _, height in state.onResize(height) }
        .onChange(of: state.selectedProvider) { _, provider in
            if interactive { state.onProviderChange(provider) }
        }
        .onAppear { if interactive { searchFocused = true } }
        .onChange(of: state.focusSearchRequest) { _, _ in if interactive { searchFocused = true } }
    }
    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.triangle.swap")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(Color.accentColor)
                .frame(width: 28, height: 28)
                .background(Color.accentColor.opacity(0.10), in: RoundedRectangle(cornerRadius: 7))
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text("Switchback").font(.system(size: 14, weight: .semibold))
                Text("\(state.presentation.accountCount) \(state.presentation.accountCount == 1 ? "account" : "accounts") · \(state.presentation.pinnedCount) pinned")
                    .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
            }
            Spacer()
            if state.busy {
                ProgressView().controlSize(.small).frame(width: 28, height: 28)
                    .help("Refreshing account usage")
            } else {
                PanelIconButton(symbol: "arrow.clockwise", label: "Refresh usage", action: state.onRefresh)
                    .keyboardShortcut("r", modifiers: .command)
            }
        }
        .padding(12)
    }
    private var providerFilters: some View {
        HStack(spacing: 3) {
            providerChip("All", key: "all")
            ForEach(["claude", "codex", "zai", "minimax"], id: \.self) { key in
                if let provider = state.presentation.providers.first(where: { $0.key == key }) {
                    providerChip(provider.title, key: key,
                                 count: key == "claude" ? provider.accounts.count : nil)
                }
            }
            if state.presentation.providers.contains(where: { ["opencodego", "opencodezen"].contains($0.key) }) {
                providerChip("OpenCode", key: "opencode")
            }
            let extras = state.presentation.providers.filter {
                !["claude", "codex", "zai", "minimax", "opencodego", "opencodezen"].contains($0.key)
            }
            if !extras.isEmpty {
                Menu {
                    ForEach(extras, id: \.key) { provider in
                        Button(provider.title) { state.selectedProvider = provider.key }
                    }
                } label: {
                    Image(systemName: "ellipsis").font(.system(size: 12, weight: .medium))
                        .frame(width: 22, height: 24)
                        .foregroundStyle(extras.contains(where: { $0.key == state.selectedProvider }) ? Color.accentColor : PanelColor.secondaryText)
                        .background(extras.contains(where: { $0.key == state.selectedProvider }) ? Color.accentColor.opacity(0.12) : Color.clear, in: Capsule())
                }
                .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                .help("Other providers").accessibilityLabel("Other providers").focusable()
            }
            Spacer(minLength: 0)
        }
    }
    private func providerChip(_ title: String, key: String, count: Int? = nil) -> some View {
        Button {
            state.selectedProvider = key
        } label: {
            HStack(spacing: 3) {
                Text(title).lineLimit(1)
                if let count { Text("\(count)").opacity(0.65).monospacedDigit() }
            }
            .font(.system(size: 11, weight: state.selectedProvider == key ? .semibold : .medium))
            .padding(.horizontal, 5).frame(height: 24)
            .foregroundStyle(state.selectedProvider == key ? Color.accentColor : PanelColor.secondaryText)
            .background(state.selectedProvider == key ? Color.accentColor.opacity(0.12) : Color.primary.opacity(0.035),
                        in: Capsule())
            .contentShape(Capsule())
        }
        .buttonStyle(PanelPressStyle()).focusable()
        .accessibilityLabel("Show \(title) accounts")
        .accessibilityAddTraits(state.selectedProvider == key ? .isSelected : [])
    }
    private var search: some View {
        HStack(spacing: 7) {
            Image(systemName: "magnifyingglass").font(.system(size: 12)).foregroundStyle(.tertiary)
            TextField(text: $state.query, prompt: Text("Find an account").foregroundStyle(PanelColor.secondaryText)) { Text("Find an account") }
                .textFieldStyle(.plain).font(.system(size: 12)).focused($searchFocused)
                .accessibilityLabel("Find an account by label or email")
            if !state.query.isEmpty {
                Button { state.query = "" } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
                }.buttonStyle(PanelPressStyle()).focusable().help("Clear search")
            }
            Rectangle().fill(PanelColor.hairline).frame(width: 1, height: 15)
            Button { state.pinnedOnly.toggle() } label: {
                Image(systemName: state.pinnedOnly ? "pin.fill" : "pin")
                    .font(.system(size: 12))
                    .foregroundStyle(state.pinnedOnly ? Color.accentColor : Color.secondary)
                    .frame(width: 21, height: 25)
            }
            .buttonStyle(PanelPressStyle()).focusable()
            .help(state.pinnedOnly ? "Show all accounts" : "Show pinned accounts only")
            .accessibilityLabel(state.pinnedOnly ? "Show all accounts" : "Show pinned accounts only")
        }
        .padding(.horizontal, 9).frame(height: 30)
        .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(PanelColor.hairline, lineWidth: 1 / displayScale))
    }
    private var footer: some View {
        HStack {
            HStack(spacing: 5) {
                Circle().fill(state.busy ? Color.orange : Color.secondary.opacity(0.45)).frame(width: 5, height: 5)
                Text(state.busy ? "Refreshing…" : state.checkedText)
                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
            }
            Spacer()
            Menu {
                Toggle("Icon only in menu bar", isOn: Binding(
                    get: { state.iconOnly }, set: { state.onIconOnly($0) }))
                Toggle("Limit notifications (80%, 95%, reset)", isOn: Binding(
                    get: { state.limitNotices }, set: { state.onLimitNotices($0) }))
                Divider()
                Button("Quit Switchback", action: state.onQuit).keyboardShortcut("q", modifiers: .command)
            } label: {
                Image(systemName: "slider.horizontal.3").font(.system(size: 12))
                    .foregroundStyle(PanelColor.secondaryText).frame(width: 24, height: 24)
            }
            .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize().help("Options").accessibilityLabel("Options").focusable()
        }
        .padding(.horizontal, 12).frame(height: 30)
    }
}
private struct ProviderSection: View {
    let provider: ProviderPanel
    let state: UsagePanelState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Text(provider.title).font(.system(size: 12, weight: .semibold))
                if !provider.accounts.isEmpty {
                    Text("\(provider.accounts.count)").font(.system(size: 10)).monospacedDigit().foregroundStyle(.tertiary)
                }
                Spacer()
            }
            .padding(.horizontal, 2)
            if let advice = provider.advice {
                HStack(spacing: 5) {
                    Image(systemName: advice.key == nil ? "questionmark.circle" : "arrow.right.circle")
                        .font(.system(size: 11))
                        .foregroundStyle(advice.key == nil ? Color.secondary : Color.accentColor)
                        .accessibilityHidden(true)
                    Text(advice.text).font(.system(size: 11, weight: .medium)).monospacedDigit()
                        .foregroundStyle(advice.key == nil ? PanelColor.secondaryText : Color.primary)
                        .lineLimit(1).truncationMode(.tail)
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 2)
                .help(advice.detail)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(advice.text + ". " + advice.detail)
            }
            if let message = provider.emptyMessage, provider.accounts.isEmpty {
                HStack(alignment: .top, spacing: 9) {
                    Image(systemName: provider.key == "opencodezen" ? "creditcard" : "clock")
                        .font(.system(size: 15)).foregroundStyle(.tertiary).frame(width: 20).padding(.top, 2)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(provider.key == "opencodezen" ? "Balance unavailable" : "No usage reading yet")
                            .font(.system(size: 12, weight: .medium))
                        Text(message + ". Refresh to check again.")
                            .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText).fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer(minLength: 0)
                }
                .padding(13).frame(maxWidth: .infinity, alignment: .leading).panelCard()
            }
            ForEach(provider.accounts, id: \.key) { account in
                AccountCard(account: account, onPin: state.onPin, expanded: state.expandedAccounts.contains(account.key)) {
                    withAnimation(PanelMotion.disclosure(reduceMotion: reduceMotion, pointerInitiated: PanelMotion.pointerInitiated)) {
                        state.toggleExpansion(account.key)
                    }
                }
            }
        }
    }
}
private struct AccountCard: View {
    let account: AccountPanel
    let onPin: (String) -> Void
    let expanded: Bool
    let onExpand: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .center, spacing: 6) {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 5) {
                        Text(account.label).font(.system(size: 12, weight: .semibold))
                            .lineLimit(1).help(account.label).layoutPriority(1)
                        if let notice = account.notice {
                            HStack(spacing: 3) {
                                if notice == "At limit" {
                                    Circle().fill(Color.red).frame(width: 4, height: 4).accessibilityHidden(true)
                                }
                                Text(notice)
                            }
                            .font(.system(size: 9, weight: .medium)).foregroundStyle(Color.primary)
                            .padding(.horizontal, 5).padding(.vertical, 2)
                            .background(Color.primary.opacity(0.045), in: Capsule()).fixedSize()
                        }
                    }
                    if let email = account.email, !email.isEmpty {
                        Text(email).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                            .lineLimit(1).truncationMode(.middle).help(email)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                PanelIconButton(symbol: account.pinned ? "pin.fill" : "pin",
                                label: (account.pinned ? "Unpin " : "Pin ") + account.label,
                                selected: account.pinned) { onPin(account.key) }
                PanelIconButton(symbol: expanded ? "chevron.up" : "chevron.down",
                                label: expanded ? "Hide details for \(account.label)" : "Show all limits for \(account.label)") {
                    onExpand()
                }
            }
            if account.primary.count == 1, let row = account.primary.first {
                QuotaGauge(row: row, showHistory: expanded)
            } else if !account.primary.isEmpty {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(account.primary, id: \.id) { row in
                        QuotaGauge(row: row, showHistory: expanded).frame(maxWidth: .infinity)
                    }
                }
            }
            ForEach(account.balances, id: \.id) { row in
                HStack(alignment: .firstTextBaseline) {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(row.title).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                        Text(row.valueText).font(.system(size: 20, weight: .semibold, design: .rounded)).monospacedDigit()
                            .foregroundStyle(row.state == .measured ? Color.primary : PanelColor.secondaryText)
                    }
                    Spacer()
                    if row.state != .measured {
                        VStack(alignment: .trailing, spacing: 3) {
                            Text(row.state == .estimated ? "Estimate" : row.isEstimate ? "Last estimate" : "Last reading")
                            Text(row.sampleText)
                        }
                        .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                    }
                }
                .accessibilityElement(children: .ignore).accessibilityLabel(row.accessibilityText)
            }
            if account.primary.isEmpty && account.balances.isEmpty {
                Text(account.extras.isEmpty ? "Usage unavailable" : "More limits are available in details").font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
            }
            if expanded {
                VStack(alignment: .leading, spacing: 8) {
                    PanelRule()
                    ForEach(account.extras, id: \.id) { row in QuotaGauge(row: row, showHistory: true) }
                    Text(account.sampleText).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                    DisclosureGroup("Reading details") {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(Array(account.details.enumerated()), id: \.offset) { _, line in
                                Text(line).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                                    .frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
                            }
                        }.padding(.top, 5)
                    }
                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                }.transition(.opacity)
            }
        }
        .padding(.horizontal, 10).padding(.vertical, 9).frame(maxWidth: .infinity, alignment: .leading).panelCard()
    }
}
private struct QuotaGauge: View {
    let row: QuotaRow
    var showHistory = false
    private var tint: Color {
        guard let fraction = row.remainingFraction else { return .secondary }
        if fraction <= 0.10 { return .red }
        if fraction <= 0.30 { return .orange }
        return .green
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(row.title).font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
                Spacer(minLength: 0)
                Text(row.valueText + (row.state == .unavailable ? "" : " left"))
                    .font(.system(size: 11, weight: .semibold)).monospacedDigit()
                    .foregroundStyle(Color.primary)
                    .fixedSize()
            }
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.065))
                    if let fraction = row.remainingFraction {
                        Capsule().fill(tint).frame(width: geometry.size.width * max(0, min(1, fraction)))
                    }
                }
            }
            .frame(height: 4)
            .accessibilityHidden(true)
            HStack(spacing: 3) {
                if row.state == .estimated { Text("Estimate ·") }
                Text(row.resetText).lineLimit(1).help(row.resetText)
            }
            .font(.system(size: 10)).foregroundStyle(Color.primary.opacity(0.70))
            if let qualifier = row.qualifier {
                Text(qualifier).font(.system(size: 9)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            }
            if showHistory, let last = row.lastReading {
                Text(last).font(.system(size: 9)).foregroundStyle(PanelColor.secondaryText)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(row.accessibilityText)
    }
}
private struct PanelIconButton: View {
    let symbol: String
    let label: String
    var selected = false
    var action: () -> Void
    @State private var hovered = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        Button(action: action) {
            Image(systemName: symbol).font(.system(size: 11, weight: .medium))
                .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                .frame(width: 24, height: 24)
                .background(hovered ? Color.primary.opacity(0.055) : Color.clear, in: RoundedRectangle(cornerRadius: 6))
                .contentShape(Rectangle())
        }
        .buttonStyle(PanelPressStyle()).focusable().onHover { hovered = $0 }
        .animation(reduceMotion ? nil : PanelMotion.feedback, value: hovered)
        .help(label).accessibilityLabel(label)
    }
}
private struct PanelPressStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.opacity(configuration.isPressed ? 0.65 : 1)
    }
}
private struct PanelRule: View {
    @Environment(\.displayScale) private var displayScale
    var body: some View {
        Rectangle().fill(PanelColor.hairline).frame(height: 1 / displayScale)
    }
}
private struct PanelCardStyle: ViewModifier {
    @Environment(\.displayScale) private var displayScale
    @Environment(\.colorSchemeContrast) private var contrast
    func body(content: Content) -> some View {
        content.background(PanelColor.card, in: RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(
                Color.primary.opacity(contrast == .increased ? 0.30 : 0.075), lineWidth: 1 / displayScale))
    }
}
private extension View {
    func panelCard() -> some View { modifier(PanelCardStyle()) }
}
