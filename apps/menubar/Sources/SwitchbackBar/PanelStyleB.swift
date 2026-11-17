import SwiftUI
import UsagePresentation

/// Direction B, "Focus": a hero block for the Claude account with the most room, then one compact line
/// per other account, so the decision is made at the top and the rest is a glance.
struct FocusList: View {
    let providers: [ProviderPanel]
    let state: UsagePanelState

    enum Metrics {
        static let name = 70.0
        static let row = 36.0
        static let forecast = 16.0
        static let hero = 106.0
    }

    var body: some View {
        let groups = Glance.group(providers)
        VStack(alignment: .leading, spacing: 14) {
            ForEach(groups.quota, id: \.key) { provider in
                FocusSection(provider: provider, state: state)
            }
            if !groups.silent.isEmpty { GlanceSilent(providers: groups.silent) }
            if !groups.balances.isEmpty { GlanceBalances(items: groups.balances) }
        }
    }

    static func hero(_ provider: ProviderPanel) -> AccountPanel? {
        guard let key = provider.advice?.key else { return nil }
        return provider.accounts.first { $0.key == key }
    }

    static func contentHeight(_ providers: [ProviderPanel], expanded: Set<String>) -> Double {
        let groups = Glance.group(providers)
        var sections: [Double] = groups.quota.map { provider in
            var height = 16 + 6.0
            let top = hero(provider)
            if let top {
                height += Metrics.hero + 6 + Glance.extraLines(top)
                if expanded.contains(top.key) { height += Glance.detailHeight(top) }
            } else if provider.advice != nil {
                height += 22
            }
            let rows = provider.accounts.filter { $0.key != top?.key }
            for account in rows {
                height += Metrics.row + Glance.extraLines(account)
                if expanded.contains(account.key) { height += Glance.detailHeight(account) }
            }
            return height
        }
        if !groups.silent.isEmpty { sections.append(Glance.silentHeight(groups.silent.count)) }
        if !groups.balances.isEmpty { sections.append(Glance.balancesHeight(groups.balances)) }
        return sections.reduce(0, +) + Double(max(0, sections.count - 1)) * 14
    }
}

private struct FocusSection: View {
    let provider: ProviderPanel
    let state: UsagePanelState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    private func toggle(_ key: String) {
        withAnimation(PanelMotion.disclosure(reduceMotion: reduceMotion, pointerInitiated: PanelMotion.pointerInitiated)) {
            state.toggleExpansion(key)
        }
    }

    var body: some View {
        let top = FocusList.hero(provider)
        let rows = provider.accounts.filter { $0.key != top?.key }
        VStack(alignment: .leading, spacing: 6) {
            GlanceSectionTitle(title: provider.title, count: provider.accounts.count) { EmptyView() }
                .padding(.horizontal, 2)
            if let top, let advice = provider.advice {
                FocusHero(account: top, advice: advice, expanded: state.expandedAccounts.contains(top.key),
                          onPin: state.onPin) { toggle(top.key) }
            } else if let advice = provider.advice {
                HStack(spacing: 5) {
                    Image(systemName: "questionmark.circle").accessibilityHidden(true)
                    Text(advice.text).lineLimit(1)
                }
                .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
                .padding(.horizontal, 2).frame(height: 16)
                .help(advice.detail)
            }
            if !rows.isEmpty {
                VStack(spacing: 0) {
                    ForEach(Array(rows.enumerated()), id: \.element.key) { index, account in
                        if index > 0 { PanelRule() }
                        FocusRow(account: account, expanded: state.expandedAccounts.contains(account.key),
                                 onPin: state.onPin) { toggle(account.key) }
                    }
                }
                .clipShape(RoundedRectangle(cornerRadius: 9))
                .panelCard()
            }
        }
    }
}

private struct FocusHero: View {
    let account: AccountPanel
    let advice: SeatAdvice
    let expanded: Bool
    let onPin: (String) -> Void
    let onToggle: () -> Void
    @Environment(\.colorSchemeContrast) private var contrast
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            GlanceRowButton(spoken: Glance.spoken(account, anchor: true) + ". " + advice.detail,
                            expanded: expanded, action: onToggle) {
                VStack(alignment: .leading, spacing: 9) {
                    HStack(alignment: .lastTextBaseline, spacing: 8) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text("MOST ROOM NOW").font(.system(size: 10, weight: .semibold)).tracking(0.6)
                                .foregroundStyle(Color.accentColor)
                            Text(account.label).font(.system(size: 18, weight: .semibold)).lineLimit(1)
                        }
                        Spacer(minLength: 0)
                        HStack(alignment: .lastTextBaseline, spacing: 3) {
                            Text(account.glance.roomText ?? "—")
                                .font(.system(size: 30, weight: .semibold, design: .rounded)).monospacedDigit()
                            Text("left").font(.system(size: 12)).foregroundStyle(PanelColor.secondaryText)
                        }
                    }
                    VStack(spacing: 7) {
                        ForEach(account.primary, id: \.id) { row in FocusLane(row: row) }
                    }
                    if let forecast = account.glance.forecast { GlanceForecastLine(forecast: forecast) }
                    if let cost = account.glance.cost { GlanceCostLine(cost: cost) }
                }
                .padding(12)
            }
            if expanded {
                GlanceDetail(account: account, onPin: onPin).padding(.horizontal, 12).padding(.bottom, 12)
            }
        }
        .background(Color.accentColor.opacity(contrast == .increased ? 0.16 : 0.08), in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(
            Color.accentColor.opacity(contrast == .increased ? 0.60 : 0.28), lineWidth: 1 / displayScale))
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }
}

/// One window inside the hero: name, bar, percent left and time to reset.
private struct FocusLane: View {
    let row: QuotaRow
    var body: some View {
        HStack(spacing: 8) {
            Text(Glance.laneTitle(row)).font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
                .frame(width: 46, alignment: .leading)
            MiniMeter(fraction: row.remainingFraction, height: 6)
            Text(row.valueText).font(.system(size: 11, weight: .semibold)).monospacedDigit()
                .frame(width: 36, alignment: .trailing)
            Text(row.resetIn.map { "↻ " + $0 } ?? "—").font(.system(size: 10)).monospacedDigit()
                .foregroundStyle(PanelColor.secondaryText).frame(width: 58, alignment: .trailing)
        }
    }
}

private struct FocusRow: View {
    let account: AccountPanel
    let expanded: Bool
    let onPin: (String) -> Void
    let onToggle: () -> Void
    @Environment(\.colorSchemeContrast) private var contrast

    private var glance: AccountGlance { account.glance }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            GlanceRowButton(spoken: Glance.spoken(account, anchor: false), expanded: expanded, action: onToggle) {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 1) {
                            Text(account.label).font(.system(size: 12, weight: .semibold)).lineLimit(1)
                            if let notice = account.notice, glance.status != .out {
                                Text(notice).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
                            }
                        }
                        .frame(width: FocusList.Metrics.name, alignment: .leading)
                        content
                    }
                    .frame(height: FocusList.Metrics.row - 6)
                    if let forecast = glance.forecast, glance.status != .out {
                        GlanceForecastLine(forecast: forecast)
                            .padding(.leading, FocusList.Metrics.name + 10)
                            .frame(height: FocusList.Metrics.forecast - 3, alignment: .top)
                    }
                    if let cost = glance.cost {
                        GlanceCostLine(cost: cost)
                            .padding(.leading, FocusList.Metrics.name + 10)
                            .frame(height: 12, alignment: .top)
                    }
                }
                .padding(.horizontal, 10).padding(.vertical, 3)
                .background(glance.status == .out ? Color.red.opacity(contrast == .increased ? 0.12 : 0.06) : Color.clear)
            }
            if expanded {
                GlanceDetail(account: account, onPin: onPin)
                    .padding(.horizontal, 10).padding(.top, 4).padding(.bottom, 10)
            }
        }
    }

    @ViewBuilder private var content: some View {
        switch glance.status {
        case .ready:
            MiniMeter(fraction: glance.roomPercent.map { $0 / 100 }, height: 5)
            Text(glance.roomText ?? "—").font(.system(size: 12, weight: .semibold)).monospacedDigit()
                .frame(width: 36, alignment: .trailing)
            VStack(alignment: .trailing, spacing: 1) {
                Text(glance.resetText.map { "↻ " + $0 } ?? "—").font(.system(size: 11, weight: .medium)).monospacedDigit()
                Text(glance.tighterTitle ?? "").font(.system(size: 9)).foregroundStyle(PanelColor.secondaryText)
            }
            .frame(width: 64, alignment: .trailing)
        case .out:
            HStack(spacing: 5) {
                Image(systemName: "hourglass").font(.system(size: 11, weight: .semibold)).accessibilityHidden(true)
                Text(glance.backInText ?? "Out").font(.system(size: 13, weight: .semibold)).monospacedDigit()
            }
            .foregroundStyle(Color.red)
            Spacer(minLength: 4)
            Text((glance.tighterTitle ?? "Quota") + " used up").font(.system(size: 10)).foregroundStyle(Color.red.opacity(0.85))
        case .partial:
            Text(partialText).font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            Spacer(minLength: 0)
        case .unavailable:
            Text(account.sampleText).font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            Spacer(minLength: 0)
        case .empty:
            Text(account.extras.isEmpty ? "Usage unavailable" : "More limits in details")
                .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
            Spacer(minLength: 0)
        }
    }

    /// What is known, without drawing a bar that would overstate room.
    private var partialText: String {
        account.primary.map { row in
            row.state == .measured ? Glance.laneTitle(row) + " " + row.valueText + " left" : Glance.laneTitle(row) + " unknown"
        }.joined(separator: " · ")
    }
}
