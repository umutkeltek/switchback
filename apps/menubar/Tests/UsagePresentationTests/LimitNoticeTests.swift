import Foundation
import Testing
@testable import UsagePresentation

private let fiveHourID = "claude:seat4|five_hour|||0"
private let weeklyID = "claude:seat4|seven_day|||0"
private func reading(_ used: Double, weekly: Double = 10, sample: TimeInterval = 0) throws -> UsageSnapshot {
    try snapshot([account("seat4", windows: [window("five_hour", used: used, remaining: 100 - used, sample: sample),
                                             window("seven_day", used: weekly, remaining: 100 - weekly, sample: sample, reset: 86_400)],
                          sample: sample)])
}
private func notices(_ data: UsageSnapshot, after previous: [String: Int], enabled: Bool = true,
                     pins: Set<String> = ["claude:seat4"]) -> LimitNoticeResult {
    Presenter.limitNotices(data, now: instant, pins: pins, enabled: enabled, previous: previous)
}

@Test func limitNoticeFirstSightOnlyRecordsLevels() throws {
    let result = notices(try reading(97, weekly: 85), after: [:])
    #expect(result.notices.isEmpty)
    #expect(result.levels == [fiveHourID: 95, weeklyID: 80])
}
@Test func limitNoticePassingEightyNotifiesOnce() throws {
    let baseline = notices(try reading(70), after: [:])
    #expect(baseline.levels[fiveHourID] == 0)
    let crossed = notices(try reading(85), after: baseline.levels)
    #expect(crossed.notices.count == 1)
    let notice = try #require(crossed.notices.first)
    #expect(notice.kind == .used80)
    #expect(notice.accountKey == "claude:seat4")
    #expect(notice.title == "Claude seat4: 5-hour window over 80% used")
    #expect(notice.body == "85% used · 15% left · Resets in 1h 0m")
    #expect(notice.id == "switchback.limit." + fiveHourID + ".used80")
    #expect(crossed.levels[fiveHourID] == 80)
}
@Test func limitNoticePassingNinetyFiveNotifiesOnceEvenWhenSkippingEighty() throws {
    let crossed = notices(try reading(96), after: [fiveHourID: 80, weeklyID: 0])
    #expect(crossed.notices.map(\.kind) == [.used95])
    #expect(crossed.notices.first?.title == "Claude seat4: 5-hour window over 95% used")
    #expect(crossed.levels[fiveHourID] == 95)
    let jump = notices(try reading(99), after: [fiveHourID: 0, weeklyID: 0])
    #expect(jump.notices.map(\.kind) == [.used95])
}
@Test func limitNoticeResetAfterNinetyFive() throws {
    let reset = notices(try reading(3), after: [fiveHourID: 95, weeklyID: 0])
    #expect(reset.notices.map(\.kind) == [.reset])
    #expect(reset.notices.first?.title == "Claude seat4: 5-hour window reset")
    #expect(reset.notices.first?.body == "3% used · 97% left")
    #expect(reset.levels[fiveHourID] == 0)
    let quiet = notices(try reading(3), after: [fiveHourID: 80, weeklyID: 0])
    #expect(quiet.notices.isEmpty)
    #expect(quiet.levels[fiveHourID] == 0)
}
@Test func limitNoticeDoesNotRepeatOnNextRefresh() throws {
    let first = notices(try reading(85), after: [fiveHourID: 0, weeklyID: 0])
    #expect(first.notices.count == 1)
    let second = notices(try reading(85), after: first.levels)
    #expect(second.notices.isEmpty)
    #expect(second.levels == first.levels)
    let critical = notices(try reading(96), after: second.levels)
    #expect(critical.notices.count == 1)
    let wobble = notices(try reading(88), after: critical.levels)
    #expect(wobble.notices.isEmpty)
    #expect(wobble.levels[fiveHourID] == 95)
    #expect(notices(try reading(97), after: wobble.levels).notices.isEmpty)
}
@Test func limitNoticeOptionOffSendsNothing() throws {
    let off = notices(try reading(99, weekly: 99), after: [fiveHourID: 0, weeklyID: 0], enabled: false)
    #expect(off.notices.isEmpty)
    #expect(off.levels.isEmpty)
    let resetOff = notices(try reading(1), after: [fiveHourID: 95], enabled: false)
    #expect(resetOff.notices.isEmpty)
}
@Test func limitNoticeIgnoresUnpinnedAndUntrustedReadings() throws {
    let unpinned = notices(try reading(99), after: [fiveHourID: 0, weeklyID: 0], pins: ["claude:seat1"])
    #expect(unpinned.notices.isEmpty)
    #expect(unpinned.levels.isEmpty)
    let stale = notices(try reading(99, sample: -901), after: [fiveHourID: 0, weeklyID: 0])
    #expect(stale.notices.isEmpty)
    #expect(stale.levels == [fiveHourID: 0, weeklyID: 0])
    // A failed client refresh does not make a fresh reading untrusted.
    let failed = Presenter.limitNotices(try reading(99), now: instant, pins: ["claude:seat4"], enabled: true,
                                        previous: [fiveHourID: 0], failure: "Could not read")
    #expect(!failed.notices.isEmpty)
}
