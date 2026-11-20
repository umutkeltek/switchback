import AppKit
import Foundation
import Testing
@testable import UsagePresentation

/// A Claude seat whose label repeats its slot, so it reads "Seat 2"; optional Fable limit and missing 5-hour reading.
func gaugeSeat(_ slot: String, fiveHour: Double?, weekly: Double, fable: Double? = nil, sample: TimeInterval = -60,
               fiveHourReset: TimeInterval = 7_200, weeklyReset: TimeInterval = 4 * 86_400 + 19 * 3_600) -> [String: Any] {
    var windows: [[String: Any]] = []
    if let fiveHour {
        windows.append(window("five_hour", used: 100 - fiveHour, remaining: fiveHour, sample: sample, reset: fiveHourReset))
    } else {
        windows.append(window("five_hour", used: NSNull(), remaining: NSNull(), sample: sample, reset: fiveHourReset, state: "unknown"))
    }
    var week = window("seven_day", used: 100 - weekly, remaining: weekly, sample: sample, reset: weeklyReset)
    week["window_minutes"] = 10_080
    windows.append(week)
    if let fable {
        var model = window("seven_day_fable", used: 100 - fable, remaining: fable, sample: sample, reset: weeklyReset - 60)
        model["window_minutes"] = 10_080
        model["model"] = "Fable"
        windows.append(model)
    }
    var row = account(slot, windows: windows, sample: sample)
    row["label"] = slot
    return row
}

/// Eight single-account plans: three quotas, two balances, one account without a reading, two providers without one.
func eightOtherPlans() -> (accounts: [[String: Any]], coverage: [[String: Any]]) {
    func balance(_ value: Double) -> [String: Any] { ["kind": "prepaid", "balance": value, "currencyCode": "USD"] }
    var weekly = window("secondary", used: 23, remaining: 77, reset: 4 * 86_400)
    weekly["window_minutes"] = 10_080
    let accounts: [[String: Any]] = [
        account("team", provider: "codex", windows: [window("primary", used: 81, remaining: 19, reset: 3 * 3_600), weekly]),
        account("zai-coding--configured", provider: "zai", windows: [window("tokens_300m", used: 13, remaining: 87)]),
        account("minimax-coding--configured", provider: "minimax", windows: [window("interval", used: 53, remaining: 47)]),
        account("deepseek--configured", provider: "deepseek", windows: [], balances: [balance(1_099.5)]),
        account("neuralwatt--configured", provider: "neuralwatt", windows: [], balances: [balance(0)]),
        account("openrouter--configured", provider: "openrouter", windows: []),
    ]
    let coverage: [[String: Any]] = [["provider": "opencodego", "state": "unavailable", "reason": "source_error"],
                                     ["provider": "opencodezen", "state": "unknown", "reason": "no_saved_usage"]]
    return (accounts, coverage)
}

/// Width of a string in the font the panel draws it with.
func textWidth(_ text: String, size: Double, weight: NSFont.Weight, rounded: Bool = false) -> Double {
    var font = NSFont.monospacedDigitSystemFont(ofSize: size, weight: weight)
    if rounded, let descriptor = font.fontDescriptor.withDesign(.rounded) { font = NSFont(descriptor: descriptor, size: size) ?? font }
    return (text as NSString).size(withAttributes: [.font: font]).width
}

@Test func gaugeWrapsToRowsOfAtMostFiveWithEvenWidths() {
    #expect(GaugeLayout.columns(1) == 1)
    #expect(GaugeLayout.columns(5) == 5)
    #expect(GaugeLayout.columns(6) == 3)
    #expect(GaugeLayout.columns(7) == 4)
    #expect(GaugeLayout.columns(8) == 4)
    #expect(GaugeLayout.columns(10) == 5)
    #expect(GaugeLayout.columns(11) == 4)
    for count in 1...23 {
        let rows = GaugeLayout.rows(Array(0..<count))
        // Never more than five in a row; every row but the last is full; nothing is lost or reordered.
        #expect(rows.allSatisfy { $0.count <= GaugeLayout.maxColumns })
        #expect(rows.dropLast().allSatisfy { $0.count == GaugeLayout.columns(count) })
        #expect(rows.flatMap { $0 } == Array(0..<count))
        // The fewest rows that hold them.
        #expect(rows.count == (count + 4) / 5)
    }
    #expect(GaugeLayout.rows(Array(0..<8)).map(\.count) == [4, 4])
    #expect(GaugeLayout.rows(Array(0..<7)).map(\.count) == [4, 3])
    // Five segments and their gaps fill the content width exactly.
    let five = GaugeLayout.segmentWidth(columns: 5)
    #expect(abs(five * 5 + GaugeLayout.segmentGap * 4 - GaugeLayout.contentWidth) < 0.001)
}

@Test func otherPlansUseShortProviderNamesAndKeepFullNamesInTheTooltip() throws {
    let plans = eightOtherPlans()
    let data = try snapshot(plans.accounts, coverage: plans.coverage)
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    let other = try #require(directions.sections.first { $0.id == "other" })
    #expect(other.seats.map(\.name) == ["Codex", "Z.ai", "MiniMax", "Go", "Zen", "DeepSeek", "Neural", "OpenR."])
    let neural = try #require(other.seats.first { $0.provider == "neuralwatt" })
    #expect(neural.title == "Neuralwatt")
    #expect(neural.detail.hasPrefix("Neuralwatt"))
    #expect(neural.spoken.hasPrefix("Neuralwatt: prepaid balance $0.00"))
    #expect(neural.balanceEmpty)
    let deepseek = try #require(other.seats.first { $0.provider == "deepseek" })
    #expect(deepseek.chip == "$1,100")
    #expect(GaugeLayout.rows(other.seats).map(\.count) == [4, 4])
}

@Test func gaugeNeverTruncatesNumbersBalancesOrShortNames() throws {
    let plans = eightOtherPlans()
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 0, weekly: 28), gaugeSeat("seat2", fiveHour: 84, weekly: 38, fable: 27),
                             gaugeSeat("seat3", fiveHour: 100, weekly: 0), gaugeSeat("seat4", fiveHour: 72, weekly: 60),
                             gaugeSeat("personal", fiveHour: nil, weekly: 11)] + plans.accounts, coverage: plans.coverage)
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    for section in directions.sections {
        let width = GaugeLayout.segmentWidth(columns: GaugeLayout.columns(section.seats.count))
        for seat in section.seats {
            let number = seat.tone == .balance ? seat.chip ?? "—" : seat.roomText
            let size = seat.tone == .balance ? GaugeLayout.balanceFont : GaugeLayout.numberFont
            #expect(textWidth(number, size: size, weight: .semibold, rounded: true) <= width, "\(number) in \(width)")
            #expect(textWidth(seat.chip ?? "", size: 9, weight: .regular) <= width, "\(seat.chip ?? "")")
            if section.id == "other" {
                // Symbol (8) and spacing (2) sit before a provider's short name.
                #expect(textWidth(seat.name, size: 10, weight: .medium) + 10 <= width, "\(seat.name)")
            }
        }
    }
    // The widest values a segment can be asked to show, at the narrowest width (five in a row).
    let narrowest = GaugeLayout.segmentWidth(columns: GaugeLayout.maxColumns)
    for number in ["100", "—", "0"] {
        #expect(textWidth(number, size: GaugeLayout.numberFont, weight: .semibold, rounded: true) <= narrowest)
    }
    for balance in ["$99.99", "$9,999", "$99.9k", "~$9,999", "€99.99"] {
        #expect(textWidth(balance, size: GaugeLayout.balanceFont, weight: .semibold, rounded: true) <= narrowest, "\(balance)")
    }
    for chip in ["↻ 6d 23h", "↻ 23h 59m", "back 6d 23h", "no reading", "old reading", "14m old", "prepaid"] {
        #expect(textWidth(chip, size: 9, weight: .regular) <= narrowest, "\(chip)")
    }
    for name in ["Codex", "Z.ai", "MiniMax", "DeepSeek", "Neural", "OpenR.", "Qwen", "Go", "Zen"] {
        #expect(textWidth(name, size: 10, weight: .medium) + 10 <= narrowest, "\(name)")
    }
}

@Test func freshReadingShowsItsResetAndAnOldOneAddsItsAge() throws {
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 60, weekly: 90, sample: -60),
                             gaugeSeat("seat2", fiveHour: 60, weekly: 90, sample: -540)])
    let seats = Presenter.directions(Presenter.panel(data, now: instant)).sections[0].seats
    let fresh = try #require(seats.first { $0.id == "claude:seat1" })
    #expect(fresh.chip == "↻ 2h 0m")
    #expect(fresh.age == nil)
    let older = try #require(seats.first { $0.id == "claude:seat2" })
    // Nine minutes old: the reset chip stays, the age is a separate mark.
    #expect(older.chip == "↻ 2h 0m")
    #expect(older.age == "9m old")
    #expect(older.spoken.contains("reading 9m old"))
    #expect(older.detail.contains("Reading 9m old"))
}

@Test func seatWithoutAFiveHourReadingShowsItsWeeklyRoomAsPartial() throws {
    let data = try snapshot([gaugeSeat("personal", fiveHour: nil, weekly: 11, fable: 0), gaugeSeat("seat2", fiveHour: 84, weekly: 38)])
    let seats = Presenter.directions(Presenter.panel(data, now: instant)).sections[0].seats
    let partial = try #require(seats.first { $0.id == "claude:personal" })
    #expect(partial.partial)
    #expect(partial.room == 0.11)
    #expect(partial.roomText == "11")
    #expect(partial.roomLane == .weekly)
    #expect(partial.tone == .low)
    #expect(partial.chip == "↻ 4d 19h")
    #expect(partial.spoken.hasPrefix("Personal: 11 percent left, weekly only, 5-hour unknown; Fable limit used up"))
    #expect(partial.detail.contains("5-hour unknown · Weekly 11% left"))
    // Partial room is shown, never counted: the best seat still needs both windows.
    #expect(!partial.best)
    #expect(seats.first { $0.id == "claude:seat2" }?.best == true)
    let complete = try #require(seats.first { $0.id == "claude:seat2" })
    #expect(!complete.partial)
}

@Test func segmentFillIsTheTighterWindowAndBothShowOnHover() throws {
    let data = try snapshot([gaugeSeat("seat2", fiveHour: 84, weekly: 38, fable: 27), gaugeSeat("seat4", fiveHour: 72, weekly: 60)])
    let directions = Presenter.directions(Presenter.panel(data, now: instant))
    let seat = try #require(directions.seats.first { $0.id == "claude:seat2" })
    #expect(seat.room == 0.38)
    #expect(seat.roomLane == .weekly)
    #expect(seat.modelName == "Fable")
    #expect(seat.modelLeft == 0.27)
    #expect(seat.detail.contains("5-hour 84% left · Weekly 38% left · Fable limit 27% left"))
    #expect(seat.spoken == "Seat 2: 38 percent left, weekly; Fable 73 percent used; resets in 4 days 19 hours")
    let best = try #require(directions.seats.first { $0.id == "claude:seat4" })
    #expect(best.spoken == "Seat 4: 60 percent left, weekly; resets in 4 days 19 hours; most room now")
}

@Test func legendAppearsOnlyWhenAModelHasItsOwnLimit() throws {
    let withModel = try snapshot([gaugeSeat("seat1", fiveHour: 80, weekly: 70, fable: 40), gaugeSeat("seat2", fiveHour: 80, weekly: 70)])
    let section = try #require(Presenter.directions(Presenter.panel(withModel, now: instant)).sections.first)
    #expect(section.legend == "Fable limit")
    #expect(section.hasModelBar)
    let without = try snapshot([gaugeSeat("seat1", fiveHour: 80, weekly: 70), gaugeSeat("seat2", fiveHour: 80, weekly: 70)])
    let plain = try #require(Presenter.directions(Presenter.panel(without, now: instant)).sections.first)
    #expect(plain.legend == nil)
    #expect(!plain.hasModelBar)
    // The legend adds a line to the section only when it is drawn.
    #expect(GaugeLayout.sectionHeight(section) > GaugeLayout.sectionHeight(plain))
}

@Test func gaugeIsTheDefaultLookAndASavedPreferenceIsKept() {
    #expect(PanelStyle(preference: nil) == .gauge)
    #expect(PanelStyle(preference: "") == .gauge)
    #expect(PanelStyle(preference: "unknown") == .gauge)
    #expect(PanelStyle(preference: "a") == .ledger)
    #expect(PanelStyle(preference: "cards") == .cards)
    #expect(PanelStyle(preference: "rings") == .rings)
    #expect(PanelStyle(preference: "strip") == .strip)
    #expect(PanelStyle.preferenceKey == "panelStyle")
    #expect(PanelStyle.looks.map(\.title) == ["Gauge", "Ledger", "Rings", "Strip"])
    // A choice from Options › Look reads back as the same look.
    #expect(PanelStyle.looks.allSatisfy { PanelStyle(preference: $0.rawValue) == $0 })
}

@Test func seatDetailListsEveryLimitWithResetAndAgeThenIssuesAndPin() throws {
    var signIn = gaugeSeat("seat3", fiveHour: 50, weekly: 50)
    signIn["relogin_required"] = true
    let data = try snapshot([gaugeSeat("seat2", fiveHour: 84, weekly: 38, fable: 27, sample: -540), signIn])
    let panel = Presenter.panel(data, now: instant, pins: ["claude:seat2"])
    let accounts = try #require(panel.providers.first).accounts
    let detail = Presenter.seatDetail(try #require(accounts.first { $0.key == "claude:seat2" }))
    #expect(detail.lines.map(\.title) == ["5-hour", "Weekly", "Fable · Weekly"])
    #expect(detail.lines.map(\.value) == ["84% left", "38% left", "27% left"])
    #expect(detail.lines[0].reset == "↻ 2h 0m")
    #expect(detail.lines[1].reset == "↻ 4d 19h")
    #expect(detail.lines.allSatisfy { $0.age == "9m ago" })
    #expect(detail.lines[2].modelLimit)
    #expect(detail.pinned)
    #expect(detail.issues.isEmpty)
    let blocked = Presenter.seatDetail(try #require(accounts.first { $0.key == "claude:seat3" }))
    #expect(blocked.issues.first?.hasPrefix("Sign in needed") == true)
    #expect(blocked.lines.allSatisfy { $0.value == "unknown" })
    #expect(!blocked.pinned)
    // An open detail adds its height; the search bar adds its own.
    let directions = Presenter.directions(panel)
    let closed = GaugeLayout.height(directions, searching: false, detail: nil, showBest: true)
    #expect(GaugeLayout.height(directions, searching: false, detail: detail, showBest: true) > closed + GaugeLayout.detailHeight(detail) - 1)
    #expect(GaugeLayout.height(directions, searching: true, detail: nil, showBest: true) == closed + GaugeLayout.searchBar)
}

@Test func menuBarGaugeFitsTheBudgetOrFallsBackToTheIcon() throws {
    let number = textWidth("100", size: 12, weight: .medium)
    #expect(StatusGauge.fits(bars: 5, numberWidth: number))
    #expect(StatusGauge.width(bars: 5, numberWidth: number) <= StatusGauge.budget)
    // Crowded: too many pinned seats for the budget.
    #expect(!StatusGauge.fits(bars: 30, numberWidth: number))
    // No pinned Claude seat: nothing to draw but the icon.
    #expect(!StatusGauge.fits(bars: 0, numberWidth: number))
    #expect(StatusGauge.budget == 96)
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 80, weekly: 70), gaugeSeat("seat2", fiveHour: 80, weekly: 70),
                             account("solo", provider: "codex", windows: [window("primary", used: 40, remaining: 60)])])
    let panel = Presenter.panel(data, now: instant, pins: ["claude:seat2", "codex:solo"])
    #expect(StatusGauge.bars(panel, data: Presenter.directions(panel)).map(\.id) == ["claude:seat2"])
}

@Test func partialSeatNamesTheWindowThatIsMissing() throws {
    // A 4-hour interval whose reset has passed, beside a fresh weekly window.
    var interval = window("interval", used: 5, remaining: 95, reset: -60)
    interval["window_minutes"] = 240
    var weekly = window("weekly", used: 53, remaining: 47, reset: 3 * 86_400)
    weekly["window_minutes"] = 10_080
    let data = try snapshot([account("plan", provider: "minimax", windows: [interval, weekly])])
    let seat = try #require(Presenter.directions(Presenter.panel(data, now: instant)).seats.first)
    #expect(seat.partial)
    #expect(seat.roomText == "47")
    #expect(seat.spoken.contains("47 percent left, weekly only, interval unknown"))
    #expect(!seat.spoken.contains("5-hour"))
}
