import SwiftUI
import UsagePresentation

/// The ledger: one row per account with the 5-hour and weekly windows in aligned columns, so every account
/// is compared by scanning down one column. The account with the most room is the tinted row.
struct LedgerList: View {
    let providers: [ProviderPanel]
    let state: UsagePanelState

    enum Metrics {
        static let name = 84.0
        static let cell = 60.0
        static let gap = 10.0
        static let row = 38.0
        static let line = 15.0
        static let forecast = 16.0
    }

    var body: some View {
        let groups = Glance.group(providers)
        VStack(alignment: .leading, spacing: 14) {
            ForEach(Array(groups.quota.enumerated()), id: \.element.key) { index, provider in
                LedgerSection(provider: provider, showsColumns: index == 0, state: state)
            }
            if !groups.silent.isEmpty { GlanceSilent(providers: groups.silent, titleInset: 10) }
            if !groups.balances.isEmpty { GlanceBalances(items: groups.balances, titleInset: 10) }
        }
    }

    static func contentHeight(_ providers: [ProviderPanel], expanded: Set<String>) -> Double {
        let groups = Glance.group(providers)
        var sections: [Double] = groups.quota.map { provider in
            var height = 16 + 6.0
            if provider.costTotal != nil { height += GlanceCostTotalLine.height + 6 }
            if provider.advice != nil && provider.advice?.key == nil { height += 22 }
            height += Double(provider.advice?.models.count ?? 0) * 18
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
    /// Only the first section carries the column headers; every section shares the same columns.
    let showsColumns: Bool
    let state: UsagePanelState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: LedgerList.Metrics.gap) {
                GlanceSectionTitle(title: provider.title, count: provider.accounts.count) { EmptyView() }
                    .frame(width: LedgerList.Metrics.name, alignment: .leading)
                if showsColumns {
                    Text("5-hour left").frame(width: LedgerList.Metrics.cell, alignment: .leading)
                    Text("Weekly left").frame(width: LedgerList.Metrics.cell, alignment: .leading)
                    Spacer(minLength: 0)
                    Text("Resets")
                } else {
                    Spacer(minLength: 0)
                }
            }
            .font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
            .lineLimit(1)
            .padding(.horizontal, 10)
            .accessibilityElement(children: .combine)
            if let total = provider.costTotal {
                GlanceCostTotalLine(total: total).padding(.horizontal, 10)
            }
            if let advice = provider.advice, advice.key == nil {
                HStack(spacing: 5) {
                    Image(systemName: "questionmark.circle").accessibilityHidden(true)
                    Text(advice.text).lineLimit(1)
                }
                .font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText)
                .padding(.horizontal, 2).frame(height: 16)
                .help(advice.detail)
            }
            ForEach(provider.advice?.models ?? [], id: \.model) { seat in
                HStack(spacing: 5) {
                    Image(systemName: seat.key == nil ? "questionmark.circle" : "arrow.right.circle")
                        .foregroundStyle(seat.key == nil ? PanelColor.secondaryText : Color.accentColor)
                        .accessibilityHidden(true)
                    Text("Most room for " + seat.text).lineLimit(1).truncationMode(.tail)
                }
                .font(.system(size: 11, weight: .medium)).monospacedDigit()
                .padding(.horizontal, 2).frame(height: 12)
                .help(seat.model + " has its own weekly limit; this seat has the most room under it and the 5-hour and weekly limits.")
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("Most room for " + seat.text)
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

    var body: some View {
        let ledger = account.ledger
        VStack(alignment: .leading, spacing: 0) {
            GlanceRowButton(spoken: Glance.spoken(account, anchor: anchor), expanded: expanded, action: onToggle) {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: LedgerList.Metrics.gap) {
                        nameColumn(note: ledger.note).frame(width: LedgerList.Metrics.name, alignment: .leading)
                        LedgerCell(row: ledger.session)
                        LedgerCell(row: ledger.weekly)
                        Spacer(minLength: 0)
                        resetColumn
                    }
                    .frame(height: LedgerList.Metrics.row - 6)
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(ledger.lines, id: \.self) { line in
                            Text(line).font(.system(size: 10)).monospacedDigit()
                                .foregroundStyle(PanelColor.secondaryText).lineLimit(1).truncationMode(.tail)
                                .frame(height: LedgerList.Metrics.line, alignment: .top)
                        }
                        if let forecast = glance.forecast, !isOut {
                            GlanceForecastLine(forecast: forecast)
                                .frame(height: LedgerList.Metrics.forecast - 3, alignment: .top)
                        }
                        if let cost = glance.cost {
                            GlanceCostLine(cost: cost).frame(height: 12, alignment: .top)
                        }
                    }
                    .padding(.leading, LedgerList.Metrics.name + LedgerList.Metrics.gap)
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

    private func nameColumn(note: String?) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(account.shortName).font(.system(size: 12, weight: .semibold)).lineLimit(1).truncationMode(.tail)
                .foregroundStyle(anchor ? Color.accentColor : Color.primary)
            if anchor {
                Text("Most room now").font(.system(size: 10, weight: .medium)).foregroundStyle(Color.accentColor)
            } else if let notice = account.notice, !isOut {
                Text(notice).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            } else if let note {
                Text(note).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
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
                if let age = account.ageText {
                    Text(age + " old").font(.system(size: 10)).monospacedDigit().foregroundStyle(Color.orange)
                        .help("Reading taken " + age + " ago; it counts as current for 15 minutes.")
                } else {
                    Text(glance.tighterTitle ?? "").font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                }
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
