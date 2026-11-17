import SwiftUI
import UsagePresentation

/// Direction A, "Ledger": one row per account with the 5-hour and weekly windows in aligned columns,
/// so every account is compared by scanning down one column. The account with the most room is the tinted row.
struct LedgerList: View {
    let providers: [ProviderPanel]
    let state: UsagePanelState

    enum Metrics {
        static let name = 84.0
        static let cell = 60.0
        static let gap = 10.0
        static let row = 38.0
        static let forecast = 16.0
    }

    var body: some View {
        let groups = Glance.group(providers)
        VStack(alignment: .leading, spacing: 14) {
            ForEach(groups.quota, id: \.key) { provider in
                LedgerSection(provider: provider, state: state)
            }
            if !groups.silent.isEmpty { GlanceSilent(providers: groups.silent, titleInset: 10) }
            if !groups.balances.isEmpty { GlanceBalances(items: groups.balances, titleInset: 10) }
        }
    }

    static func contentHeight(_ providers: [ProviderPanel], expanded: Set<String>) -> Double {
        let groups = Glance.group(providers)
        var sections: [Double] = groups.quota.map { provider in
            var height = 16 + 6.0
            if provider.advice != nil && provider.advice?.key == nil { height += 22 }
            for account in provider.accounts {
                height += Metrics.row + Glance.extraLines(account)
                if expanded.contains(account.key) { height += Glance.detailHeight(account) }
            }
            return height + Glance.rules(provider.accounts.count)
        }
        if !groups.silent.isEmpty { sections.append(Glance.silentHeight(groups.silent.count)) }
        if !groups.balances.isEmpty { sections.append(Glance.balancesHeight(groups.balances)) }
        return sections.reduce(0, +) + Double(max(0, sections.count - 1)) * 14
    }
}

private struct LedgerSection: View {
    let provider: ProviderPanel
    let state: UsagePanelState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: LedgerList.Metrics.gap) {
                GlanceSectionTitle(title: provider.title, count: provider.accounts.count) { EmptyView() }
                    .frame(width: LedgerList.Metrics.name, alignment: .leading)
                Text("5-hour").frame(width: LedgerList.Metrics.cell, alignment: .leading)
                Text("Weekly").frame(width: LedgerList.Metrics.cell, alignment: .leading)
                Spacer(minLength: 0)
                Text("Resets")
            }
            .font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
            .padding(.horizontal, 10)
            .accessibilityElement(children: .combine)
            if let advice = provider.advice, advice.key == nil {
                HStack(spacing: 5) {
                    Image(systemName: "questionmark.circle").accessibilityHidden(true)
                    Text(advice.text).lineLimit(1)
                }
                .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
                .padding(.horizontal, 2).frame(height: 16)
                .help(advice.detail)
            }
            VStack(spacing: 0) {
                ForEach(Array(provider.accounts.enumerated()), id: \.element.key) { index, account in
                    if index > 0 { PanelRule() }
                    LedgerRow(account: account, anchor: provider.advice?.key == account.key,
                              expanded: state.expandedAccounts.contains(account.key), onPin: state.onPin) {
                        withAnimation(PanelMotion.disclosure(reduceMotion: reduceMotion, pointerInitiated: PanelMotion.pointerInitiated)) {
                            state.toggleExpansion(account.key)
                        }
                    }
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: 9))
            .panelCard()
        }
    }
}

private struct LedgerRow: View {
    let account: AccountPanel
    let anchor: Bool
    let expanded: Bool
    let onPin: (String) -> Void
    let onToggle: () -> Void
    @Environment(\.colorSchemeContrast) private var contrast

    private var glance: AccountGlance { account.glance }
    private var isOut: Bool { glance.status == .out }
    /// Session window in the left column, weekly in the right; any other window fills a free column.
    private var cells: (left: QuotaRow?, right: QuotaRow?) {
        var left = account.primary.first { $0.lane == .session }
        var right = account.primary.first { $0.lane == .weekly }
        for row in account.primary where row.lane == .other {
            if left == nil { left = row } else if right == nil { right = row }
        }
        return (left, right)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            GlanceRowButton(spoken: Glance.spoken(account, anchor: anchor), expanded: expanded, action: onToggle) {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: LedgerList.Metrics.gap) {
                        nameColumn.frame(width: LedgerList.Metrics.name, alignment: .leading)
                        LedgerCell(row: cells.left)
                        LedgerCell(row: cells.right)
                        Spacer(minLength: 0)
                        resetColumn
                    }
                    .frame(height: LedgerList.Metrics.row - 6)
                    if let forecast = glance.forecast, !isOut {
                        GlanceForecastLine(forecast: forecast)
                            .padding(.leading, LedgerList.Metrics.name + LedgerList.Metrics.gap)
                            .frame(height: LedgerList.Metrics.forecast - 3, alignment: .top)
                    }
                    if let cost = glance.cost {
                        GlanceCostLine(cost: cost)
                            .padding(.leading, LedgerList.Metrics.name + LedgerList.Metrics.gap)
                            .frame(height: 12, alignment: .top)
                    }
                }
                .padding(.horizontal, 10).padding(.vertical, 3)
                .background(rowTint)
                .overlay(alignment: .leading) {
                    if anchor { Rectangle().fill(Color.accentColor).frame(width: 3).accessibilityHidden(true) }
                }
            }
            if expanded {
                GlanceDetail(account: account, onPin: onPin)
                    .padding(.horizontal, 10).padding(.top, 4).padding(.bottom, 10)
            }
        }
    }

    private var rowTint: Color {
        if anchor { return Color.accentColor.opacity(contrast == .increased ? 0.16 : 0.09) }
        if isOut { return Color.red.opacity(contrast == .increased ? 0.12 : 0.06) }
        return .clear
    }

    private var nameColumn: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(account.label).font(.system(size: 12, weight: .semibold)).lineLimit(1).truncationMode(.tail)
                .foregroundStyle(anchor ? Color.accentColor : Color.primary)
            if anchor {
                Text("Most room now").font(.system(size: 10, weight: .medium)).foregroundStyle(Color.accentColor)
            } else if let notice = account.notice, !isOut {
                Text(notice).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            }
        }
    }

    @ViewBuilder private var resetColumn: some View {
        VStack(alignment: .trailing, spacing: 1) {
            switch glance.status {
            case .out:
                Text(glance.backInText ?? "Out").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color.red)
                Text((glance.tighterTitle ?? "Quota") + " used up").font(.system(size: 10)).foregroundStyle(Color.red.opacity(0.85))
            case .ready:
                Text(glance.resetText ?? "—").font(.system(size: 12, weight: .medium)).monospacedDigit()
                Text(glance.tighterTitle ?? "").font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
            case .unavailable:
                Text("—").font(.system(size: 12)).foregroundStyle(.tertiary)
                Text(account.sampleText.replacingOccurrences(of: "Sampled ", with: ""))
                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
            case .partial, .empty:
                Text("—").font(.system(size: 12)).foregroundStyle(.tertiary)
            }
        }
        .lineLimit(1).fixedSize()
    }
}

private struct LedgerCell: View {
    let row: QuotaRow?
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let row {
                Text(row.valueText)
                    .font(.system(size: 12, weight: row.state == .measured ? .semibold : .regular)).monospacedDigit()
                    .foregroundStyle(row.state == .measured ? Color.primary : PanelColor.secondaryText)
                MiniMeter(fraction: row.remainingFraction, height: 3)
            }
        }
        .frame(width: LedgerList.Metrics.cell, alignment: .leading)
    }
}
