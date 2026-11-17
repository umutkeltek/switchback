import SwiftUI
import UsagePresentation

/// Pieces of the ledger panel around the rows themselves. The card look does not use them.
enum Glance {
    static func tint(_ fraction: Double?) -> Color {
        guard let fraction else { return .secondary }
        if fraction <= 0.10 { return .red }
        if fraction <= 0.30 { return .orange }
        return .green
    }
    static func title(_ provider: ProviderPanel) -> String {
        Presenter.displayTitle(key: provider.key, title: provider.title)
    }
    struct BalanceItem: Identifiable {
        let provider: ProviderPanel
        let account: AccountPanel
        var id: String { account.key }
    }
    struct Groups {
        var quota: [ProviderPanel] = []
        var balances: [BalanceItem] = []
        var silent: [ProviderPanel] = []
    }
    /// Quota accounts stay in their provider section; balance-only accounts share one compact list;
    /// providers with no account reading share one line each.
    static func group(_ providers: [ProviderPanel]) -> Groups {
        var groups = Groups()
        for provider in providers {
            if provider.accounts.isEmpty {
                if provider.emptyMessage != nil { groups.silent.append(provider) }
                continue
            }
            let balanceOnly = provider.accounts.filter { $0.primary.isEmpty && !$0.balances.isEmpty }
            let quota = provider.accounts.filter { !($0.primary.isEmpty && !$0.balances.isEmpty) }
            groups.balances += balanceOnly.map { BalanceItem(provider: provider, account: $0) }
            if !quota.isEmpty {
                groups.quota.append(ProviderPanel(key: provider.key, title: title(provider), emptyMessage: provider.emptyMessage,
                                                  accounts: quota, advice: provider.advice))
            }
        }
        return groups
    }
    static func silentHeight(_ count: Int) -> Double { count == 0 ? 0 : 22 + Double(count) * 26 + rules(count) }
    static func balancesHeight(_ items: [BalanceItem]) -> Double {
        guard !items.isEmpty else { return 0 }
        let rows = items.reduce(0) { $0 + max(1, $1.account.balances.count) }
        return 22 + Double(rows) * 30 + rules(items.count)
    }
    /// Height of the optional lines under a row: labelled windows, the run-out line and the spend line.
    static func extraLines(_ account: AccountPanel) -> Double {
        var height = Double(account.ledger.lines.count) * 15
        if account.glance.forecast != nil && account.glance.status != .out { height += 16 }
        if account.glance.cost != nil { height += 15 }
        return height
    }
    /// Measured from renders: padding, sample line and disclosure 66, slot line 21, each gauge 42,
    /// each balance 30, two-line token note 34.
    static func detailHeight(_ account: AccountPanel) -> Double {
        var height = 87.0 + Double(account.primary.count + account.extras.count) * 42 + Double(account.balances.count) * 30
        if account.glance.cost != nil { height += 34 }
        return min(400, height)
    }
    /// One physical-pixel rule between rows, rounded up to a point each.
    static func rules(_ count: Int) -> Double { Double(max(0, count - 1)) }
    /// One sentence for VoiceOver and the tooltip of a row. Carries the full slot the short name hides.
    static func spoken(_ account: AccountPanel, anchor: Bool) -> String {
        var parts = [account.shortName, "account " + account.slot]
        if anchor { parts.append("Most room now") }
        if let notice = account.notice { parts.append(notice) }
        let glance = account.glance
        if glance.status == .out, let backIn = glance.backInText { parts.append("Out of " + (glance.tighterTitle ?? "quota") + ". " + backIn) }
        if let note = account.ledger.note { parts.append(note) }
        parts += account.primary.map(\.accessibilityText)
        parts += account.extras.filter { !$0.generation }.map(\.accessibilityText)
        if let forecast = glance.forecast { parts.append(forecast.text) }
        if let cost = glance.cost { parts.append("Spend today " + cost.todayText + ", last 7 days " + cost.weekText) }
        return parts.joined(separator: ". ")
    }
}

/// A thin capacity bar. Untrusted readings pass nil and draw only the track. Values never animate.
struct MiniMeter: View {
    let fraction: Double?
    var height = 4.0
    var tint: Color?
    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.08))
                if let fraction, fraction > 0 {
                    Capsule().fill(tint ?? Glance.tint(fraction))
                        .frame(width: max(height, geometry.size.width * min(1, fraction)))
                }
            }
        }
        .frame(height: height)
        .animation(nil, value: fraction)
        .accessibilityHidden(true)
    }
}

/// A whole-row button with hover and press feedback; keyboard focus reaches it.
struct GlanceRowButton<Label: View>: View {
    let spoken: String
    let expanded: Bool
    let action: () -> Void
    @ViewBuilder let label: () -> Label
    @State private var hovered = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var body: some View {
        Button(action: action) {
            label()
                .contentShape(Rectangle())
                .background(hovered ? Color.primary.opacity(0.04) : Color.clear)
        }
        .buttonStyle(PanelPressStyle()).focusable()
        .onHover { hovered = $0 }
        .animation(reduceMotion ? nil : PanelMotion.feedback, value: hovered)
        .help(spoken)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
        .accessibilityHint(expanded ? "Hides details" : "Shows every limit and the menu bar pin")
        .accessibilityAddTraits(.isButton)
    }
}

/// Everything the card look shows, revealed when a row is expanded.
struct GlanceDetail: View {
    let account: AccountPanel
    let onPin: (String) -> Void
    /// What the spend line does not say: token counts and where the number comes from.
    static func costNote(_ cost: CostGlance) -> String {
        var parts: [String] = []
        if let today = cost.todayTokens, let week = cost.weekTokens {
            parts.append("Tokens " + today + " today · " + week + " in 7 days")
        }
        if cost.estimated { parts.append("Spend is estimated from local logs, not a bill") }
        return parts.joined(separator: ". ") + "."
    }
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            let identity = ["Account " + account.slot, account.email].compactMap { $0 }.filter { !$0.isEmpty }
            Text(identity.joined(separator: " · ")).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                .lineLimit(1).truncationMode(.middle).help(identity.joined(separator: " · "))
                .textSelection(.enabled)
            ForEach(account.primary, id: \.id) { row in QuotaGauge(row: row, showHistory: true) }
            ForEach(account.extras, id: \.id) { row in QuotaGauge(row: row, showHistory: true) }
            ForEach(account.balances, id: \.id) { row in
                HStack(alignment: .firstTextBaseline) {
                    Text(row.title).foregroundStyle(PanelColor.secondaryText)
                    Spacer()
                    Text(row.valueText).fontWeight(.semibold).monospacedDigit()
                        .foregroundStyle(row.state == .measured ? Color.primary : PanelColor.secondaryText)
                }
                .font(.system(size: 11))
                .accessibilityElement(children: .ignore).accessibilityLabel(row.accessibilityText)
            }
            if let cost = account.glance.cost {
                Text(Self.costNote(cost))
                    .font(.system(size: 10)).monospacedDigit().foregroundStyle(PanelColor.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                Text(account.sampleText).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                Spacer(minLength: 0)
                Button { onPin(account.key) } label: {
                    Label(account.pinned ? "Pinned in menu bar" : "Pin in menu bar",
                          systemImage: account.pinned ? "pin.fill" : "pin")
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(account.pinned ? Color.accentColor : PanelColor.secondaryText)
                }
                .buttonStyle(PanelPressStyle()).focusable()
                .accessibilityLabel((account.pinned ? "Unpin " : "Pin ") + account.shortName + ", account " + account.slot)
            }
            DisclosureGroup("Reading details") {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(Array(account.details.enumerated()), id: \.offset) { _, line in
                        Text(line).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                            .frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
                    }
                }.padding(.top, 5)
            }
            .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
        }
        .transition(.opacity)
    }
}

/// Section title with a count, as in the card look.
struct GlanceSectionTitle<Trailing: View>: View {
    let title: String
    let count: Int?
    @ViewBuilder var trailing: () -> Trailing
    var body: some View {
        HStack(spacing: 6) {
            Text(title).font(.system(size: 12, weight: .semibold)).foregroundStyle(Color.primary)
            if let count { Text("\(count)").font(.system(size: 10)).monospacedDigit().foregroundStyle(.tertiary) }
            Spacer(minLength: 0)
            trailing()
        }
        .frame(height: 16)
    }
}

/// The run-out projection line. Orange because it is a warning, never a measured value.
struct GlanceForecastLine: View {
    let forecast: RunOutForecast
    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "chart.line.uptrend.xyaxis").font(.system(size: 9, weight: .semibold)).accessibilityHidden(true)
            Text(forecast.text).lineLimit(1).truncationMode(.tail)
        }
        .font(.system(size: 10, weight: .medium)).foregroundStyle(Color.orange)
        .help(forecast.detail)
    }
}

/// Providers that report no account reading, one line each.
struct GlanceSilent: View {
    let providers: [ProviderPanel]
    var titleInset = 2.0
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            GlanceSectionTitle(title: "No reading", count: nil) { EmptyView() }.padding(.horizontal, titleInset)
            VStack(spacing: 0) {
                ForEach(Array(providers.enumerated()), id: \.element.key) { index, provider in
                    if index > 0 { PanelRule() }
                    HStack(spacing: 8) {
                        Text(Glance.title(provider)).font(.system(size: 12, weight: .medium))
                        Spacer(minLength: 8)
                        Text((provider.emptyMessage ?? "No usage reported yet") + " · refresh to check")
                            .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
                    }
                    .padding(.horizontal, 10).frame(height: 26)
                    .accessibilityElement(children: .combine)
                }
            }
            .panelCard()
        }
    }
}

/// Prepaid balances, one compact line per balance.
struct GlanceBalances: View {
    let items: [Glance.BalanceItem]
    var titleInset = 2.0
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            GlanceSectionTitle(title: "Prepaid balances", count: nil) { EmptyView() }.padding(.horizontal, titleInset)
            VStack(spacing: 0) {
                ForEach(Array(items.enumerated()), id: \.element.id) { index, item in
                    if index > 0 { PanelRule() }
                    let shared = items.filter { $0.provider.key == item.provider.key }.count > 1
                    ForEach(item.account.balances, id: \.id) { row in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Text(Glance.title(item.provider) + (shared ? " · " + item.account.shortName : ""))
                                .font(.system(size: 12, weight: .medium)).lineLimit(1)
                            if let notice = item.account.notice {
                                Text(notice).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
                            }
                            Spacer(minLength: 8)
                            if row.state != .measured {
                                Text(row.state == .estimated ? "Estimate" : row.isEstimate ? "Last estimate" : "Last reading")
                                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                            }
                            Text(row.valueText).font(.system(size: 12, weight: .semibold)).monospacedDigit()
                                .foregroundStyle(row.state == .measured ? Color.primary : PanelColor.secondaryText)
                        }
                        .padding(.horizontal, 10).frame(height: 30)
                        .help(row.sampleText + " · account " + item.account.slot)
                        .accessibilityElement(children: .ignore)
                        .accessibilityLabel(Glance.title(item.provider) + ", account " + item.account.slot + ". " + row.accessibilityText)
                    }
                }
            }
            .panelCard()
        }
    }
}

/// Today and 7-day spend for one account, drawn only when the snapshot carries cost.
/// "~" marks an estimate, as it does for estimated quota readings.
struct GlanceCostLine: View {
    let cost: CostGlance
    static func help(_ cost: CostGlance) -> String {
        var parts: [String] = []
        if let tokens = cost.todayTokens { parts.append("Today " + tokens + " tokens") }
        if let tokens = cost.weekTokens { parts.append("7 days " + tokens + " tokens") }
        parts.append(cost.detail)
        return parts.joined(separator: " · ")
    }
    var body: some View {
        let mark = cost.estimated ? "~" : ""
        HStack(spacing: 4) {
            Text("Spend").foregroundStyle(PanelColor.secondaryText)
            Text(mark + cost.todayText).fontWeight(.semibold)
            Text("today ·").foregroundStyle(PanelColor.secondaryText)
            Text(mark + cost.weekText).fontWeight(.semibold)
            Text("in 7 days").foregroundStyle(PanelColor.secondaryText)
        }
        .font(.system(size: 10)).monospacedDigit().lineLimit(1)
        .help(Self.help(cost))
    }
}
