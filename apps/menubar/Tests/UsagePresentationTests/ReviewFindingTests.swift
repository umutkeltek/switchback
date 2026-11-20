import Foundation
import Testing
@testable import UsagePresentation

// One test (or more) per review finding. Each fails on the rules before the fix.

// MARK: A failed refresh hides nothing

@Test func reviewOneFailedRefreshHidesEveryFreshSeat() throws {
    // The saved snapshot is seconds old; the last scheduled refresh timed out.
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70), seat("seat2", fiveHour: 30, weekly: 50)])
    let ok = Presenter.directions(Presenter.panel(data, now: instant))
    #expect(ok.best?.id == "claude:seat1")
    let failed = Presenter.directions(Presenter.panel(data, now: instant, failure: "Switchback refresh timed out"))
    #expect(failed.best?.id == "claude:seat1", "a failed refresh hides the best seat")
    #expect(failed.seats.first?.roomText == "40", "a failed refresh hides a 0 s old reading")
}

@Test func reviewRefreshingHidesNothing() throws {
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70)])
    let busy = Presenter.directions(Presenter.panel(data, now: instant, refreshing: true))
    #expect(busy.best?.id == "claude:seat1")
    #expect(busy.seats.first?.roomText == "40")
}

@Test func aFailedRefreshIsOnlyInTheFooterWithTheReadingsAge() throws {
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70, sample: -180)])
    let panel = Presenter.panel(data, now: instant, failure: "Switchback refresh timed out")
    #expect(panel.readingsAgeSeconds == 180)
    #expect(Presenter.failureFooter(readingsAgeSeconds: panel.readingsAgeSeconds, hasReadings: true)
            == "Couldn’t update · showing readings from 3m ago")
    #expect(Presenter.failureFooter(readingsAgeSeconds: 10, hasReadings: true) == "Couldn’t update · showing the latest readings")
    #expect(Presenter.failureFooter(readingsAgeSeconds: nil, hasReadings: false) == "Couldn’t load readings")
    // The seat itself says nothing about the failed client refresh.
    let glyph = try #require(Presenter.directions(panel).seats.first)
    #expect(glyph.roomText == "40")
    #expect(!glyph.detail.contains("failed"))
}

@Test func aFailedRefreshFallsBackToTheSavedRead() {
    #expect(RefreshRequest.scheduled.afterFailure == .saved)
    #expect(RefreshRequest.forced.afterFailure == .saved)
    #expect(RefreshRequest.saved.afterFailure == nil)
    #expect(RefreshRequest.saved.arguments == ["limits", "--json"])
}

// MARK: The tile, the best seat and the status item use one rule

@Test func reviewIdleSeatWithNullFiveHourIsRecommendedOnItsWeeklyRoom() throws {
    // The vendor sends no 5-hour window (null) for an idle seat: the Python side publishes used=nil, state=unknown.
    var idle5 = window("five_hour", used: NSNull(), remaining: NSNull(), state: "unknown")
    idle5["resets_at"] = NSNull()
    let idle = account("idle", windows: [idle5, window("seven_day", used: 5, remaining: 95, reset: 86_400)])
    let data = try snapshot([idle, seat("busy", fiveHour: 20, weekly: 40)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == "claude:idle")
    #expect(advice.roomPercent == 95)
    #expect(advice.weeklyOnly)
    #expect(advice.text == "Most room now: idle · 95% left (weekly only)")
    let dirs = Presenter.directions(Presenter.panel(data, now: instant))
    let glyph = try #require(dirs.seats.first { $0.id == "claude:idle" })
    #expect(dirs.best?.id == "claude:idle", "hero recommends \(dirs.best?.id ?? "nil") while idle shows \(glyph.roomText)")
    // The tile and the card agree on the number and both say it is weekly only.
    #expect(glyph.roomText == "95")
    #expect(dirs.best?.roomText == "95")
    #expect(glyph.partial)
    #expect(glyph.partialNote == "weekly only")
    #expect(dirs.best?.partialNote == "weekly only")
    #expect(StatusGauge.number(dirs) == "95")
    #expect(StatusGauge.spoken(dirs).contains("weekly only"))
    // The menu bar readings title follows the same rule.
    #expect(Presenter.make(data, now: instant).groups[0].accounts.first { $0.key == "claude:idle" }?.summary == "95%")
}

@Test func reviewFiveHourResetPassedCountsAsAFullFiveHourWindow() throws {
    let passed = account("a", windows: [window("five_hour", used: 99, remaining: 1, reset: -60),
                                        window("seven_day", used: 30, remaining: 70, reset: 86_400)])
    let data = try snapshot([passed, seat("b", fiveHour: 50, weekly: 40)])
    let advice = try #require(Presenter.bestSeat(data, now: instant))
    #expect(advice.key == "claude:a")
    #expect(advice.roomPercent == 70)
    let dirs = Presenter.directions(Presenter.panel(data, now: instant))
    let glyph = try #require(dirs.seats.first { $0.id == "claude:a" })
    #expect(dirs.best?.id == "claude:a", "hero \(dirs.best?.roomText ?? "-") vs seat a \(glyph.roomText)")
    #expect(glyph.roomText == "70")
    #expect(!glyph.partial)
    #expect(glyph.tone == .good)
    #expect(StatusGauge.number(dirs) == "70")
    #expect(Presenter.make(data, now: instant).groups[0].accounts.first { $0.key == "claude:a" }?.summary == "70%")
    // The Python side marks such a window reset_passed; it counts the same way.
    var marked = window("five_hour", used: 99, remaining: 1, reset: -60, state: "reset_passed")
    marked["window_minutes"] = 300
    let markedData = try snapshot([account("a", windows: [marked, window("seven_day", used: 30, remaining: 70, reset: 86_400)])])
    #expect(Presenter.bestSeat(markedData, now: instant)?.roomPercent == 70)
}

// MARK: Sign-in needs say so

@Test func reviewSignInSeatSaysSignIn() throws {
    let data = try snapshot([seat("seat1", fiveHour: 80, weekly: 80, error: "authentication_required"), seat("seat2", fiveHour: 50, weekly: 60)])
    let glyph = try #require(Presenter.directions(Presenter.panel(data, now: instant)).seats.first { $0.id == "claude:seat1" })
    #expect(glyph.chip == "sign in")
}

// MARK: An old reading keeps its number, muted

@Test func reviewStaleReadingStillShowsANumber() throws {
    let data = try snapshot([seat("seat1", fiveHour: 80, weekly: 60, sample: -1200)])
    let dirs = Presenter.directions(Presenter.panel(data, now: instant))
    let glyph = try #require(dirs.seats.first)
    #expect(glyph.roomText == "60", "a 20-minute-old reading shows \(glyph.roomText) / \(glyph.chip ?? "")")
    #expect(glyph.chip == "20m ago")
    #expect(glyph.tone == .stale)
    // Muted: never room, never "Use now", never the status item's number.
    #expect(glyph.room == nil)
    #expect(dirs.best == nil)
    #expect(StatusGauge.number(dirs) == "—")
}

// MARK: Less text

@Test func theClockMarkWaitsTwentyMinutes() throws {
    let data = try snapshot([seat("seat1", fiveHour: 80, weekly: 60, sample: -540), seat("seat2", fiveHour: 80, weekly: 60, sample: -1_260)])
    let seats = Presenter.directions(Presenter.panel(data, now: instant)).seats
    // A 9-minute-old reading, refreshed normally every few minutes, carries no clock mark.
    #expect(seats.first { $0.id == "claude:seat1" }?.clock == false)
    #expect(seats.first { $0.id == "claude:seat2" }?.clock == true)
}

@Test func plansWithNoReadingAreOneQuietLine() throws {
    let data = try snapshot([seat("seat1", fiveHour: 80, weekly: 60),
                             account("solo", provider: "minimax", windows: []),
                             account("router", provider: "openrouter", windows: []),
                             account("team", provider: "codex", windows: [window("primary", used: 40, remaining: 60)]),
                             account("zai", provider: "zai", windows: [], error: "authentication_required")],
                            coverage: [["provider": "opencodego", "state": "unavailable", "reason": "source_error"]])
    let dirs = Presenter.directions(Presenter.panel(data, now: instant))
    let other = try #require(dirs.sections.first { $0.id == "other" })
    // A plan the owner can act on (sign in) keeps its tile.
    #expect(other.seats.map(\.name) == ["Claude", "Codex", "Z.ai"])
    #expect(other.seats.last?.chip == "sign in")
    let line = try #require(dirs.noReading)
    #expect(line.text == "No reading: MiniMax, Go, OpenRouter")
    #expect(line.help.contains("OpenCode Go"))
    #expect(line.ids.count == 3)
    #expect(GaugeLayout.height(dirs, searching: false, detail: nil, showBest: true)
            > GaugeLayout.height(DirectionPresentation(best: dirs.best, modelBest: [], sections: dirs.sections),
                                 searching: false, detail: nil, showBest: true))
}

// MARK: Numbers

@Test func reviewOutTileShowsWhatIsLeft() throws {
    let data = try snapshot([seat("seat1", fiveHour: 80, weekly: 3), seat("seat2", fiveHour: 50, weekly: 60)])
    let glyph = try #require(Presenter.directions(Presenter.panel(data, now: instant)).seats.first { $0.id == "claude:seat1" })
    #expect(glyph.roomText == "3", "out tile shows \(glyph.roomText) with 3% left")
    #expect(glyph.tone == .out)
}

@Test func theFableBarShowsTheRoomUsableForFable() throws {
    // seat1: 40 left overall, 90 of its Fable limit: 40 usable for Fable. seat2: 80 overall, 30 Fable: 30.
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 40, weekly: 70, fable: 90), gaugeSeat("seat2", fiveHour: 80, weekly: 90, fable: 30)])
    let dirs = Presenter.directions(Presenter.panel(data, now: instant))
    let claude = try #require(dirs.sections.first { $0.id == "claude" })
    #expect(claude.seats.first { $0.id == "claude:seat1" }?.modelLeft == 0.4)
    #expect(claude.seats.first { $0.id == "claude:seat2" }?.modelLeft == 0.3)
    #expect(claude.legend == "Room for Fable")
    // The card's "For Fable" uses the same number.
    let pick = try #require(dirs.modelBest.first)
    #expect(pick.seat?.id == "claude:seat1")
    #expect(pick.roomText == "40")
}

@Test func noRoomForFableNamesTheSeatThatIsBackFirst() throws {
    let data = try snapshot([gaugeSeat("seat1", fiveHour: 80, weekly: 70, fable: 0, weeklyReset: 86_400 + 5 * 3_600 + 60),
                             gaugeSeat("seat2", fiveHour: 60, weekly: 60, fable: 2, weeklyReset: 3 * 86_400)])
    let pick = try #require(Presenter.directions(Presenter.panel(data, now: instant)).modelBest.first)
    #expect(pick.backText == "Seat 1 back in 1d 5h")
}

// MARK: Polish

@Test func theStatusItemFallsBackToADashAndHasAVoiceOverLabel() throws {
    let allOut = try snapshot([seat("seat1", fiveHour: 0, weekly: 50), seat("seat2", fiveHour: 60, weekly: 0)])
    let out = Presenter.directions(Presenter.panel(allOut, now: instant))
    #expect(out.noRoom != nil)
    #expect(StatusGauge.number(out) == "—")
    #expect(StatusGauge.spoken(out).hasPrefix("Switchback limits. No seat has room"))
    let ok = Presenter.directions(Presenter.panel(try snapshot([seat("seat2", fiveHour: 64, weekly: 80)]), now: instant))
    #expect(StatusGauge.spoken(ok) == "Switchback limits. Use now: Claude, 64 percent left.")
}

@Test func detailIssuesFitOneLine() throws {
    var changed = seat("seat2", fiveHour: 40, weekly: 70)
    changed["identity_state"] = "changed"
    let data = try snapshot([seat("seat1", fiveHour: 40, weekly: 70, error: "native_usage_unavailable"), changed,
                             seat("seat3", fiveHour: 40, weekly: 70, error: "authentication_required"),
                             seat("seat4", fiveHour: 40, weekly: 70, sample: -1_200)])
    for account in Presenter.panel(data, now: instant).providers[0].accounts {
        for issue in Presenter.seatDetail(account).issues {
            // The details card is 316 points wide; the icon and gap take 18.
            #expect(textWidth(issue, size: 10, weight: .regular) <= 290, "\(issue)")
        }
    }
}

@Test @MainActor func aRefreshThatIsRunningIsNeverReplaced() async throws {
    let slot = RefreshSlot()
    var runs = 0
    var cancelled = false
    #expect(slot.start {
        runs += 1
        while !Task.isCancelled { try? await Task.sleep(nanoseconds: 5_000_000) }
        cancelled = true
    })
    let running = try #require(slot.task)
    // A timer tick or a panel opening while the refresh runs starts nothing and keeps the handle.
    #expect(!slot.start { runs += 100 })
    #expect(slot.task == running)
    #expect(slot.busy)
    // Quitting cancels the refresh that is really running.
    slot.task?.cancel()
    await running.value
    #expect(cancelled)
    #expect(runs == 1)
    #expect(!slot.busy)
}
