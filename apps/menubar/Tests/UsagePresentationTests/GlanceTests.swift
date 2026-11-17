import Foundation
import Testing
@testable import UsagePresentation

private func glance(_ windows: [[String: Any]], sample: TimeInterval = 0, cost: [String: Any]? = nil,
                    provider: String = "claude") throws -> AccountGlance {
    var row = account("seat4", provider: provider, windows: windows, sample: sample)
    if let cost { row["cost"] = cost }
    let panel = Presenter.panel(try snapshot([row]), now: instant)
    return try #require(panel.providers.first?.accounts.first).glance
}
private func costField(today: Any = 4.12, week: Any = 61.4, sample: TimeInterval = 0,
                       currency: Any = "USD") -> [String: Any] {
    ["currency_code": currency, "sampled_at": stamp(sample), "basis": "estimated_from_local_logs",
     "today": ["amount": today, "tokens": 1_830_412], "last_7_days": ["amount": week, "tokens": 25_103_377]]
}

@Test func outAccountSaysWhenItIsBack() throws {
    let out = try glance([window("five_hour", used: 97, remaining: 3, reset: 2400),
                          window("seven_day", used: 62, remaining: 38, reset: 183_600)])
    #expect(out.status == .out)
    #expect(out.backInText == "Back in 40m")
    #expect(out.tighterTitle == "5-hour")
    #expect(out.forecast == nil)
    let almost = try glance([window("five_hour", used: 94, remaining: 6, reset: 2400),
                             window("seven_day", used: 62, remaining: 38, reset: 183_600)])
    #expect(almost.status == .ready)
    #expect(almost.backInText == nil)
    #expect(almost.roomText == "6%")
    #expect(almost.resetText == "40m")
}
@Test func outWaitsForEveryOutWindowAndNeverTrustsOldReadings() throws {
    let both = try glance([window("five_hour", used: 97, remaining: 3, reset: 2400),
                           window("seven_day", used: 96, remaining: 4, reset: 172_800)])
    #expect(both.backInText == "Back in 2d 0h")
    #expect(both.tighterTitle == "Weekly")
    var noReset = window("five_hour", used: 100, remaining: 0)
    noReset.removeValue(forKey: "resets_at")
    #expect(try glance([noReset, window("seven_day", used: 10, remaining: 90)]).backInText == "Reset time unknown")
    let stale = try glance([window("five_hour", used: 97, remaining: 3, sample: -901, reset: 2400),
                            window("seven_day", used: 62, remaining: 38, sample: -901, reset: 183_600)], sample: -901)
    #expect(stale.status == .unavailable)
    #expect(stale.backInText == nil)
    #expect(stale.roomPercent == nil)
    let estimate = try glance([window("five_hour", used: 99, remaining: 1, reset: 2400, estimated: true),
                               window("seven_day", used: 10, remaining: 90)])
    #expect(estimate.status != .out)
}
@Test func roomComesOnlyFromFullyTrustedWindows() throws {
    let ready = try glance([window("five_hour", used: 4, remaining: 96, reset: 15_600),
                            window("seven_day", used: 9, remaining: 91, reset: 525_600)])
    #expect(ready.status == .ready)
    #expect(ready.roomPercent == 91)
    #expect(ready.tighterTitle == "Weekly")
    #expect(ready.resetText == "6d 2h")
    let partial = try glance([window("five_hour", used: NSNull(), remaining: NSNull(), state: "unknown"),
                              window("seven_day", used: 16, remaining: 84, reset: 270_000)])
    #expect(partial.status == .partial)
    #expect(partial.roomPercent == nil)
    #expect(partial.roomText == nil)
    let balanceOnly = try glance([], cost: nil, provider: "deepseek")
    #expect(balanceOnly.status == .empty)
}
@Test func forecastOnlyWhenTheAverageRateRunsOutBeforeReset() throws {
    // 62% used after 1h 55m of a 5-hour window that resets in 3h 05m.
    let busy = try glance([window("five_hour", used: 62, remaining: 38, reset: 11_100),
                           window("seven_day", used: 27, remaining: 73, reset: 399_600)])
    let forecast = try #require(busy.forecast)
    #expect(forecast.lane == .session)
    #expect(forecast.text == "5-hour runs out in ~1h 10m at this pace")
    #expect(forecast.detail.contains("resets in 3h 5m"))
    // 91% of a week used with 1d 6h to go.
    let codex = try glance([window("primary", used: 91, remaining: 9, reset: 108_000)].map {
        var w = $0; w["window_minutes"] = 10080; return w
    }, provider: "codex")
    #expect(codex.forecast?.text == "Weekly runs out in ~13h 38m at this pace")
    let lasts = try glance([window("five_hour", used: 20, remaining: 80, reset: 3600)])
    #expect(lasts.forecast == nil)
    // Only 20 minutes of the window have passed: too early to project.
    let early = try glance([window("five_hour", used: 10, remaining: 90, reset: 16_800)])
    #expect(early.forecast == nil)
    var unknownLength = window("custom", used: 60, remaining: 40, reset: 600)
    unknownLength.removeValue(forKey: "window_minutes")
    #expect(try glance([unknownLength]).forecast == nil)
    // A reset further away than the window is long means the length does not fit.
    #expect(try glance([window("five_hour", used: 60, remaining: 40, reset: 20_000)]).forecast == nil)
    let stale = try glance([window("five_hour", used: 62, remaining: 38, sample: -901, reset: 11_100)], sample: -901)
    #expect(stale.forecast == nil)
    let estimate = try glance([window("five_hour", used: 62, remaining: 38, reset: 11_100, estimated: true)])
    #expect(estimate.forecast == nil)
}
@Test func runOutLineOnlyWithinTwentyFourHours() throws {
    // Weekly window, three days in, four days to go.
    let soon = try glance([window("seven_day", used: 76, remaining: 24, reset: 345_600)])
    #expect(soon.forecast?.text == "Weekly runs out in ~22h 44m at this pace")
    let later = try glance([window("seven_day", used: 74, remaining: 26, reset: 345_600)])
    #expect(later.forecast == nil)
    #expect(Presenter.forecastHorizon == 86_400)
}
@Test func costIsShownOnlyAsReportedAndFresh() throws {
    #expect(try glance([window()]).cost == nil)
    let shown = try #require(try glance([window()], cost: costField()).cost)
    #expect(shown.todayText == "$4.12")
    #expect(shown.weekText == "$61.40")
    #expect(shown.todayTokens == "1.8M")
    #expect(shown.weekTokens == "25.1M")
    #expect(shown.estimated)
    #expect(try glance([window()], cost: costField(currency: "EUR")).cost?.todayText == "EUR 4.12")
    #expect(try glance([window()], cost: costField(sample: -901)).cost == nil)
    #expect(try glance([window()], cost: costField(currency: "")).cost == nil)
    #expect(try glance([window()], cost: costField(today: 70, week: 61.4)).cost == nil)
    #expect(try glance([window()], cost: costField(today: -1)).cost?.todayText == "—")
    #expect(try glance([window()], cost: costField(today: -1, week: -2)).cost == nil)
    // A malformed cost never breaks the rest of the snapshot.
    let broken = try glance([window()], cost: costField(today: "lots"))
    #expect(broken.cost == nil)
    #expect(broken.status == .ready)
}
@Test func costFromYesterdayIsNotToday() throws {
    var utc = Calendar(identifier: .gregorian)
    utc.timeZone = try #require(TimeZone(identifier: "UTC"))
    let now = utc.startOfDay(for: instant).addingTimeInterval(120)
    let formatter = ISO8601DateFormatter()
    let cost = try JSONDecoder().decode(UsageCost.self, from: JSONSerialization.data(withJSONObject: [
        "currency_code": "USD", "sampled_at": formatter.string(from: now.addingTimeInterval(-300)),
        "today": ["amount": 1.0], "last_7_days": ["amount": 2.0]]))
    #expect(Presenter.costGlance(cost, now: now, calendar: utc) == nil)
    #expect(Presenter.costGlance(cost, now: now.addingTimeInterval(-240), calendar: utc)?.todayText == "$1.00")
}
@Test func windowsKnowTheirLaneAndShortReset() throws {
    var codexWeek = window("primary", used: 50, remaining: 50, reset: 7_200)
    codexWeek["window_minutes"] = 10080
    let rows = Presenter.panel(try snapshot([account(windows: [window("five_hour", reset: 4_500), codexWeek]),
                                             account("2", windows: [window("seven_day", reset: -10)])]), now: instant)
        .providers[0].accounts
    #expect(rows[0].primary.map(\.lane) == [.session, .weekly])
    #expect(rows[0].primary.map(\.resetIn) == ["1h 15m", "2h 0m"])
    #expect(rows[1].primary.first?.resetIn == nil)
}
@Test func panelStyleDefaultsToTheLedger() {
    #expect(PanelStyle(preference: nil) == .ledger)
    #expect(PanelStyle(preference: "") == .ledger)
    #expect(PanelStyle(preference: "unknown") == .ledger)
    #expect(PanelStyle(preference: "a") == .ledger)
    #expect(PanelStyle(preference: "cards") == .cards)
    #expect(PanelStyle.allCases == [.ledger, .cards])
}
