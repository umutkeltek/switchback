import Foundation
import Testing
@testable import UsagePresentation

@Test func panelSharesFreshnessAndResetGuardsWithMenu() throws {
    let data = try snapshot([account("seat", windows: [window(reset: 30)])])
    let fresh = Presenter.panel(data, now: instant).providers[0].accounts[0].primary[0]
    #expect(fresh.remainingFraction == 0.8)
    #expect(fresh.state == .measured)
    let expired = Presenter.panel(data, now: instant.addingTimeInterval(31)).providers[0].accounts[0].primary[0]
    #expect(expired.remainingFraction == nil)
    #expect(expired.valueText == "—")
    #expect(expired.lastReading == "Last reading: 80% remaining")
    let failed = Presenter.panel(data, now: instant, failure: "Could not read").providers[0].accounts[0].primary[0]
    #expect(failed.remainingFraction == nil)
}
@Test func panelKeepsZeroMeasuredAndEstimatesDistinct() throws {
    var estimate = account("estimated", windows: [window(estimated: true)])
    estimate["estimated"] = true
    let rows = Presenter.panel(try snapshot([account("zero", windows: [window(used: 100, remaining: 0)]), estimate]), now: instant).providers[0].accounts
    #expect(rows[0].primary[0].remainingFraction == 0)
    #expect(rows[0].primary[0].valueText == "0%")
    #expect(rows[0].notice == "At limit")
    #expect(rows[1].primary[0].state == .estimated)
    #expect(rows[1].primary[0].remainingFraction == nil)
    #expect(rows[1].primary[0].valueText == "~80%")
}
@Test func panelSelectsPrimaryPairWithoutLosingToolsOrVariants() throws {
    var tools = window("tools"); tools["scope"] = "tools"
    var variant = window("five_hour"); variant["model"] = "variant"
    let raw = [tools, variant, window("five_hour"), window("seven_day"), window("five_hour")]
    let row = Presenter.panel(try snapshot([account(windows: raw)]), now: instant).providers[0].accounts[0]
    #expect(row.primary.map(\.title) == ["5-hour window", "Weekly"])
    #expect(row.extras.count == 3)
    #expect(Set((row.primary + row.extras).map(\.id)).count == raw.count)
    #expect(row.extras.contains { $0.qualifier == "tools" })
    #expect(row.extras.contains { $0.qualifier == "variant" })
}
@Test func panelNeverTurnsSpendingIntoZenFunds() throws {
    let money: [String: Any] = ["kind": "prepaid", "balance": 0, "currencyCode": "USD"]
    let spent: [String: Any] = ["kind": "providerCost", "used": 12.5, "limit": 0, "currencyCode": "USD"]
    let data = try snapshot([account("zero", provider: "opencodezen", windows: [], balances: [money]),
                             account("spent", provider: "opencodezen", windows: [], balances: [spent])])
    let rows = Presenter.panel(data, now: instant).providers[0].accounts
    #expect(rows[0].balances[0].valueText == "USD 0.00")
    #expect(rows[0].balances[0].state == .measured)
    #expect(rows[0].primary.isEmpty)
    #expect(rows[1].balances.isEmpty)
    let stale = Presenter.panel(data, now: instant.addingTimeInterval(901)).providers[0].accounts[0]
    #expect(stale.balances[0].state == .unavailable)
    #expect(stale.balances[0].valueText == "USD 0.00")
}
@Test func panelKeepsFiveIdenticalLabelsAndTheirPinKeysDistinct() throws {
    let data = try snapshot((1...5).map { account("seat" + String($0)) })
    let panel = Presenter.panel(data, now: instant, pins: ["claude:seat3"])
    let rows = panel.providers[0].accounts
    #expect(rows.count == 5)
    #expect(Set(rows.map(\.key)).count == 5)
    #expect(Set(rows.map(\.label)).count == 5)
    #expect(rows.filter(\.pinned).map(\.key) == ["claude:seat3"])
    #expect(panel.pinnedCount == 1)
}
@Test func panelShowsGoAndZenWithoutFabricatingAccounts() throws {
    let coverage: [[String: Any]] = [
        ["provider": "opencodego", "state": "unknown", "reason": "no_saved_usage"],
        ["provider": "opencodezen", "state": "unavailable", "reason": "source_error"]]
    let panel = Presenter.panel(try snapshot([], coverage: coverage), now: instant)
    #expect(panel.providers.map(\.key) == ["opencodego", "opencodezen"])
    #expect(panel.providers.allSatisfy { $0.accounts.isEmpty && $0.emptyMessage != nil })
    #expect(panel.accountCount == 0)
    #expect(Presenter.panel(try snapshot([]), now: instant).providers.isEmpty)
}

@Test func panelFiltersProvidersSearchAndPinsThroughProductionSeam() throws {
    let data = try snapshot((1...5).map { account("seat" + String($0)) } + [account("work", provider: "codex")])
    let panel = Presenter.panel(data, now: instant, pins: ["claude:seat3", "codex:work"])
    #expect(panel.filtered(provider: "claude").flatMap(\.accounts).count == 5)
    #expect(panel.filtered(query: "seat3").flatMap(\.accounts).map(\.key) == ["claude:seat3"])
    #expect(panel.filtered(query: "PERSON@EXAMPLE.INVALID").flatMap(\.accounts).count == 6)
    #expect(Set(panel.filtered(pinnedOnly: true).flatMap(\.accounts).map(\.key)) == ["claude:seat3", "codex:work"])
    #expect(panel.filtered(provider: "claude", query: "work").isEmpty)
    #expect(panel.providers.flatMap(\.accounts).count == 6)
}
@Test func panelOpenCodeFilterRetainsDistinctGoAndZenEmptySections() throws {
    let coverage: [[String: Any]] = [
        ["provider": "opencodego", "state": "unknown", "reason": "no_saved_usage"],
        ["provider": "opencodezen", "state": "unknown", "reason": "no_saved_usage"]]
    let panel = Presenter.panel(try snapshot([account()], coverage: coverage), now: instant)
    #expect(panel.filtered(provider: "opencode").map(\.key) == ["opencodego", "opencodezen"])
    #expect(panel.filtered(provider: "opencode", pinnedOnly: true).isEmpty)
}

@Test func panelShowsModelOnlyMiniMaxLimitsAsPrimary() throws {
    var first = window("general_weekly", used: 55, remaining: 45)
    first["model"] = "general"; first["scope"] = "generation"
    var second = window("general_interval", used: 0, remaining: 100)
    second["model"] = "general"; second["scope"] = "generation"
    let row = Presenter.panel(try snapshot([account("coding", provider: "minimax", windows: [first, second])]), now: instant).providers[0].accounts[0]
    #expect(row.primary.count == 2)
    #expect(row.primary[0].remainingFraction == 0.45)
    #expect(row.primary[1].remainingFraction == 1)
    #expect(row.extras.isEmpty)
}
@Test func panelKeepsStaleEstimatedMoneyUnavailableAndDated() throws {
    let amount: [String: Any] = ["kind": "prepaid", "balance": 12.25, "currencyCode": "USD", "estimated": true]
    var source = account("balance", provider: "opencodezen", windows: [], balances: [amount])
    source["estimated"] = true
    let data = try snapshot([source])
    let current = Presenter.panel(data, now: instant).providers[0].accounts[0]
    #expect(current.balances[0].state == .estimated)
    let older = Presenter.panel(data, now: instant.addingTimeInterval(901)).providers[0].accounts[0]
    #expect(older.balances[0].state == .unavailable)
    #expect(older.balances[0].isEstimate)
    #expect(older.notice == "Older reading")
    #expect(older.balances[0].accessibilityText.contains("Last estimate"))
    var future = source; future["observed_at"] = stamp(31)
    #expect(Presenter.panel(try snapshot([future]), now: instant).providers[0].accounts[0].balances[0].state == .unavailable)
}
@Test func panelAccessibilityRetainsModelEstimateAndSample() throws {
    var variant = window("five_hour", estimated: true); variant["model"] = "variant-one"
    let row = Presenter.panel(try snapshot([account(windows: [variant])]), now: instant).providers[0].accounts[0].primary[0]
    #expect(row.accessibilityText.contains("variant-one"))
    #expect(row.accessibilityText.contains("Estimated reading"))
    #expect(row.accessibilityText.contains("Sampled just now"))
    let stale = Presenter.panel(try snapshot([account(windows: [variant])]), now: instant.addingTimeInterval(901)).providers[0].accounts[0].primary[0]
    #expect(stale.accessibilityText.contains("Reading unavailable"))
    #expect(stale.accessibilityText.contains("Last estimate"))
}

@Test func panelFillsMissingAggregateSlotWithModelLimit() throws {
    var model = window("model_limit"); model["model"] = "variant"
    let row = Presenter.panel(try snapshot([account(windows: [model, window("seven_day")])]), now: instant).providers[0].accounts[0]
    #expect(row.primary.count == 2)
    #expect(row.primary[0].title == "Weekly")
    #expect(row.primary[1].qualifier == "variant")
    #expect(row.extras.isEmpty)
}
