import Foundation
import Testing
@testable import UsagePresentation

private func panel(_ accounts: [[String: Any]], summary: [[String: Any]]? = nil) throws -> PanelPresentation {
    var object: [String: Any] = ["schema": "switchback/account-usage@1", "generated_at": stamp(),
                                 "purpose": "usage_observation_not_admission_authority", "accounts": accounts, "coverage": []]
    if let summary { object["cost_summary"] = summary }
    return Presenter.panel(try UsageSnapshot.decode(JSONSerialization.data(withJSONObject: object)), now: instant)
}
private func rowCost(_ windows: [String: Any]) throws -> CostGlance? {
    var row = account("seat2")
    row["cost"] = ["currency_code": "USD", "basis": "estimated_from_local_logs", "sampled_at": stamp()]
        .merging(windows) { $1 }
    return try panel([row]).providers[0].accounts[0].glance.cost
}
/// A `cost_summary` entry; the unattributed share is given as a fraction of each window's tokens.
private func entry(_ provider: String = "claude", today: Any? = 2_267.23, week: Any? = 11_992.89,
                   looseToday: Double? = 0.33, looseWeek: Double? = 0.86, sample: TimeInterval = 0,
                   currency: String = "USD") -> [String: Any] {
    var row: [String: Any] = ["provider": provider, "currency_code": currency, "basis": "estimated_from_local_logs",
                              "sampled_at": stamp(sample)]
    var loose: [String: Any] = [:]
    if let today {
        row["today"] = ["amount": today, "tokens": 3_740_000_000.0]
        if let looseToday { loose["today"] = ["amount": 0, "tokens": 3_740_000_000.0 * looseToday] }
    }
    if let week {
        row["last_7_days"] = ["amount": week, "tokens": 17_350_000_000.0]
        if let looseWeek { loose["last_7_days"] = ["amount": 0, "tokens": 17_350_000_000.0 * looseWeek] }
    }
    if !loose.isEmpty { row["unattributed"] = loose }
    return row
}
private func total(_ summary: [[String: Any]], provider: String = "claude") throws -> CostTotal? {
    let accounts = [account("seat1"), account("work", provider: "codex")]
    return try panel(accounts, summary: summary).providers.first { $0.key == provider }?.costTotal
}

@Test func moneyKeepsEveryAmountShortEnoughToFit() {
    let usd = { (value: Double) in Presenter.money(value, currency: "USD") }
    #expect(usd(0) == "$0.00")
    #expect(usd(6.42) == "$6.42")
    #expect(usd(99.99) == "$99.99")
    #expect(usd(99.996) == "$100")
    #expect(usd(100) == "$100")
    #expect(usd(713.53) == "$714")
    #expect(usd(1_098.76) == "$1,099")
    #expect(usd(9_999.49) == "$9,999")
    #expect(usd(9_999.5) == "$10.0k")
    #expect(usd(10_000) == "$10.0k")
    #expect(usd(11_992.89) == "$12.0k")
    #expect(usd(99_949) == "$99.9k")
    #expect(Presenter.money(714.2, currency: "EUR") == "EUR 714")
}
@Test func rowCostDrawsOnlyTheWindowsThatWereSent() throws {
    let today = try #require(try rowCost(["today": ["amount": 713.53, "tokens": 2_600_000]]))
    #expect(today.text == "Spend ~$714 today")
    #expect(today.todayText == "$714")
    #expect(today.weekText == nil)
    #expect(today.tokensText == "Tokens 2.6M today")
    #expect(!today.spoken.contains("7 days"))
    let week = try #require(try rowCost(["last_7_days": ["amount": 9_120.4]]))
    #expect(week.text == "Spend ~$9,120 in 7 days")
    #expect(week.todayText == nil)
    #expect(try rowCost([:]) == nil)
    #expect(try rowCost(["today": ["tokens": 5]]) == nil)
    let widest = try #require(try rowCost(["today": ["amount": 9_999], "last_7_days": ["amount": 99_900]]))
    #expect(widest.text == "Spend ~$9,999 today · ~$99.9k in 7 days")
    #expect(!widest.text.contains("—"))
}
@Test func totalLineAppearsOnlyForProvidersInTheSummary() throws {
    let claude = try #require(try total([entry()]))
    #expect(claude.text == "Est. spend ~$2,267 today · ~$12.0k in 7 days")
    #expect(claude.help.contains(Presenter.costEstimateSentence))
    #expect(claude.help.hasPrefix("Claude, all accounts."))
    #expect(claude.accessibilityText.contains(Presenter.costEstimateSentence))
    #expect(claude.accessibilityText.contains("about $2,267 today, about $12.0 thousand in the last 7 days"))
    #expect(try total([entry()], provider: "codex") == nil)
    #expect(try total([]) == nil)
    let accounts = [account("seat1"), account("work", provider: "codex")]
    #expect(try panel(accounts).providers.allSatisfy { $0.costTotal == nil })
    let filtered = try panel(accounts, summary: [entry()]).filtered(provider: "claude")
    #expect(filtered.first?.costTotal?.text == claude.text)
    #expect(try total([entry(week: nil)])?.text == "Est. spend ~$2,267 today")
    #expect(try total([entry(today: nil)])?.text == "Est. spend ~$12.0k in 7 days")
    #expect(try total([entry(today: nil, week: nil)]) == nil)
}
@Test func totalLineHidesWhenItCannotBeTrusted() throws {
    #expect(try total([entry(sample: -901)]) == nil)
    #expect(try total([entry(sample: -840)]) != nil)
    #expect(try total([entry(currency: " ")]) == nil)
    #expect(try total([entry(today: -1)]) == nil)
    #expect(try total([entry(today: 50, week: 40)]) == nil)
    #expect(try total([entry(), entry()]) == nil)
    // A malformed entry is dropped without breaking the snapshot or the other entries.
    let mixed = try panel([account("seat1"), account("work", provider: "codex")],
                          summary: [entry(today: "lots"), entry("codex", today: 1_098.76, week: nil)])
    #expect(mixed.providers.first { $0.key == "claude" }?.costTotal == nil)
    #expect(mixed.providers.first { $0.key == "codex" }?.costTotal?.text == "Est. spend ~$1,099 today")
    #expect(mixed.providers.first { $0.key == "claude" }?.accounts.count == 1)
}
@Test func totalFromYesterdayIsNotToday() throws {
    var utc = Calendar(identifier: .gregorian)
    utc.timeZone = try #require(TimeZone(identifier: "UTC"))
    let now = utc.startOfDay(for: instant).addingTimeInterval(120)
    let summary = try JSONDecoder().decode(UsageCostSummary.self, from: JSONSerialization.data(withJSONObject: [
        "provider": "claude", "currency_code": "USD",
        "sampled_at": ISO8601DateFormatter().string(from: now.addingTimeInterval(-300)), "today": ["amount": 1.0]]))
    #expect(Presenter.costTotal([summary], provider: "claude", title: "Claude", now: now, calendar: utc) == nil)
    #expect(Presenter.costTotal([summary], provider: "claude", title: "Claude", now: now.addingTimeInterval(-240), calendar: utc)?
        .text == "Est. spend ~$1.00 today")
}
@Test func unattributedShareAddsAQuietNote() throws {
    let split = try #require(try total([entry(looseToday: 0.33, looseWeek: 0.01)]))
    #expect(split.figures.map(\.splitUnknown) == [true, false])
    #expect(split.splitNote == "Split by account not known yet for today.")
    #expect(split.help.contains("Split by account not known yet for today."))
    #expect(split.accessibilityText.contains("Split by account not known yet for today."))
    #expect(split.text == "Est. spend ~$2,267 today · ~$12.0k in 7 days")
    #expect(try total([entry(looseToday: 0.33, looseWeek: 0.86)])?.splitNote == "Split by account not known yet for today and 7 days.")
    #expect(try total([entry(looseToday: 0.02, looseWeek: 0.021)])?.figures.map(\.splitUnknown) == [false, true])
    let none = try #require(try total([entry(looseToday: nil, looseWeek: nil)]))
    #expect(none.figures.allSatisfy { !$0.splitUnknown })
    #expect(none.splitNote == nil)
    #expect(!none.help.contains("Split"))
    // Without token counts the share comes from the amounts.
    let amounts = try JSONDecoder().decode(UsageCostSummary.self, from: JSONSerialization.data(withJSONObject: [
        "provider": "codex", "currency_code": "USD", "sampled_at": stamp(), "today": ["amount": 100.0],
        "unattributed": ["today": ["amount": 30.0]]]))
    #expect(Presenter.costTotal([amounts], provider: "codex", title: "Codex", now: instant)?.splitNote
        == "Split by account not known yet for today.")
}
