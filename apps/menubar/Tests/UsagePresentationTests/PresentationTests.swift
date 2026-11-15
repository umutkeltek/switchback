import Foundation
import Testing
@testable import UsagePresentation

let instant = Date(timeIntervalSince1970: 1_800_000_000)
func stamp(_ offset: TimeInterval = 0) -> String {
    ISO8601DateFormatter().string(from: instant.addingTimeInterval(offset))
}
func window(_ name: String = "5h", used: Any = 20, remaining: Any = 80,
            sample: TimeInterval = 0, reset: TimeInterval = 3600,
            state: String = "fresh", estimated: Bool = false) -> [String: Any] {
    ["name": name, "used_percent": used, "remaining_percent": remaining,
     "resets_at": stamp(reset), "sampled_at": stamp(sample),
     "state": state, "window_minutes": 300, "estimated": estimated]
}
func account(_ slot: String = "1", provider: String = "claude", windows: [[String: Any]]? = nil,
             sample: TimeInterval = 0, error: String? = nil, login: Bool = false,
             balances: [[String: Any]] = []) -> [String: Any] {
    var row: [String: Any] = ["provider": provider, "slot": slot, "label": "Synthetic Account",
        "email": "person@example.invalid", "source": "synthetic-fixture", "observed_at": stamp(sample),
        "state": "fresh", "windows": windows ?? [window()], "balances": balances, "notes": [],
        "relogin_required": login, "account_id": "must-not-render", "login_revision": "must-not-render"]
    if let error { row["refresh_error"] = error }
    return row
}
func snapshot(_ accounts: [[String: Any]], generated: TimeInterval = 0) throws -> UsageSnapshot {
    try UsageSnapshot.decode(JSONSerialization.data(withJSONObject: [
        "schema": "switchback/account-usage@1", "generated_at": stamp(generated),
        "purpose": "usage_observation_not_admission_authority", "accounts": accounts,
        "coverage": [["provider": "zai", "state": "unavailable", "reason": "missing_credentials"]]
    ]))
}
@Test func distinctSlotsAndEveryWindow() throws {
    let data = try snapshot((1...5).map { account(String($0), windows: [window(), window("week", used: 40, remaining: 60), window("month", used: 10, remaining: 90)]) })
    let menu = Presenter.make(data, now: instant, pins: Set(data.accounts.map(\.key)))
    #expect(menu.groups[0].accounts.count == 5)
    #expect(Set(menu.groups[0].accounts.map(\.key)).count == 5)
    #expect(menu.groups[0].accounts.allSatisfy { $0.summary == "60%" })
    let text = Presenter.text(menu)
    #expect(text.contains("week"))
    #expect(text.contains("month"))
    #expect(text.contains("Resets in 1h"))
    #expect(!text.contains("must-not-render"))
}
@Test(arguments: [-901.0, 31.0])
func validatesAllSampleClocks(offset: Double) throws {
    let badSnapshot = Presenter.make(try snapshot([account()], generated: offset), now: instant)
    let badAccount = Presenter.make(try snapshot([account(sample: offset)]), now: instant)
    let badWindow = Presenter.make(try snapshot([account(windows: [window(sample: offset)])]), now: instant)
    #expect(badSnapshot.groups[0].accounts[0].summary == "?")
    #expect(badAccount.groups[0].accounts[0].summary == "?")
    #expect(badWindow.groups[0].accounts[0].summary == "?")
}
@Test func resetErrorLoginAndInvalidPercentNeverBecomeCapacity() throws {
    let cases = [
        account("passed", windows: [window(reset: -1)]),
        account("reset-state", windows: [window(state: "reset_passed")]),
        account("error", error: "timeout"), account("login", login: true),
        account("negative", windows: [window(used: -1, remaining: 101)]),
        account("inconsistent", windows: [window(used: 20, remaining: 90)]),
        account("null", windows: [window(used: NSNull(), remaining: NSNull())])
    ]
    let menus = Presenter.make(try snapshot(cases), now: instant).groups[0].accounts
    #expect(menus.allSatisfy { $0.summary == "?" })
    #expect(menus[0].lines.contains { $0.contains("Reset passed") })
    #expect(menus[2].lines.contains("Refresh timed out"))
    #expect(menus[3].lines.contains("Sign in again"))
}
@Test func balanceIsNotQuotaAndEstimatesStaySeparate() throws {
    let balance: [String: Any] = ["kind": "credits", "balance": 12.25, "currencyCode": "USD"]
    let rows = [account("balance", windows: [], balances: [balance]),
                account("estimate", windows: [window(estimated: true)]),
                account("mixed", windows: [window(), window("week", state: "unknown")])]
    let menu = Presenter.make(try snapshot(rows), now: instant)
    #expect(menu.groups[0].accounts.allSatisfy { $0.summary == "?" })
    #expect(menu.groups[0].accounts[0].lines.contains { $0.contains("balance 12.25 USD") })
    #expect(menu.groups[0].accounts[1].lines.contains { $0.contains("Estimate") })
    #expect(menu.groups.first { $0.provider == "zai" }?.unavailable == "Account sign-in unavailable")
}
@Test func defaultsAndExplicitPinsNeverSumPercentages() throws {
    let data = try snapshot([account("1"), account("2"), account("1", provider: "codex")])
    #expect(Presenter.defaults(data) == ["claude:1", "codex:1"])
    let menu = Presenter.make(data, now: instant, pins: ["claude:2"])
    #expect(menu.title == "Claude 2: 80%")
    #expect(Presenter.make(data, now: instant, pins: []).title == "Switchback ◇")
}
@Test func refreshRequestsUseFixedOwnerArguments() {
    #expect(RefreshRequest.saved.arguments == ["limits", "--json"])
    #expect(RefreshRequest.scheduled.arguments == ["limits", "--refresh-if-due", "--json"])
    #expect(RefreshRequest.forced.arguments == ["limits", "--refresh", "--json"])
}
@Test func processOutputAndTimeoutAreBounded() async throws {
    // Synthetic executable fixtures are written exclusively under the external test scratch.
    let scratch = ProcessInfo.processInfo.environment["SWITCHBACK_MENUBAR_TEST_SCRATCH"]
        .map { URL(fileURLWithPath: $0) } ?? FileManager.default.temporaryDirectory
    let root = scratch.appendingPathComponent("switchback-bar-" + UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let fake = root.appendingPathComponent("sb")
    try Data("#!/bin/sh\nwhile :; do printf 1234567890; done\n".utf8).write(to: fake)
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: fake.path)
    do {
        _ = try await OwnerRunner.run(.saved, executable: fake, timeout: 2, maximumBytes: 100)
        Issue.record("Unbounded output accepted")
    } catch let error as OwnerError { #expect(error == .outputTooLarge) }
    try Data("#!/bin/sh\nwhile :; do :; done\n".utf8).write(to: fake)
    do {
        _ = try await OwnerRunner.run(.saved, executable: fake, timeout: 0.1)
        Issue.record("Timeout ignored")
    } catch let error as OwnerError { #expect(error == .timedOut) }
    let pending = Task { try await OwnerRunner.run(.saved, executable: fake, timeout: 5) }
    try await Task.sleep(for: .milliseconds(50))
    pending.cancel()
    do {
        _ = try await pending.value
        Issue.record("Cancellation ignored")
    } catch let error as OwnerError { #expect(error == .cancelled) }
}

@Test func sparseNativeShapeAndNanosecondTime() throws {
    let row: [String: Any] = [
        "provider": "claude", "slot": "max", "enabled": true, "source": "native/claude",
        "windows": [["name": "seven_day", "used_percent": 94, "remaining_percent": 6,
                     "resets_at": stamp(3600), "sampled_at": stamp(),
                     "state": "stale", "window_minutes": 10080]],
        "observed_at": stamp(), "relogin_required": false, "email": "synthetic@example.invalid",
        "identity_state": "matched", "state": "stale"]
    let data = try snapshot([row])
    #expect(data.accounts[0].notes.isEmpty)
    #expect(data.accounts[0].balances.isEmpty)
    #expect(Presenter.make(data, now: instant).groups[0].accounts[0].summary == "?")
    #expect(UsageTime.parse("2026-11-15T18:05:22.793000123Z") != nil)
}
@Test func rejectsWrongSchemaAndPurpose() throws {
    for (schema, purpose) in [("different@1", "usage_observation_not_admission_authority"),
                              ("switchback/account-usage@1", "admission")] {
        let data = try JSONSerialization.data(withJSONObject: [
            "schema": schema, "purpose": purpose, "generated_at": stamp(), "accounts": []])
        #expect(throws: SnapshotError.self) { try UsageSnapshot.decode(data) }
    }
}
@Test func goPlanRemainsSeparateFromOpenCodePlatform() throws {
    let data = try snapshot([account("go", provider: "opencodego"),
                             account("platform", provider: "opencode")])
    let menu = Presenter.make(data, now: instant)
    #expect(menu.groups.first { $0.provider == "opencodego" }?.accounts.first?.key == "opencodego:go")
    #expect(menu.groups.first { $0.provider == "opencode" }?.accounts.first?.key == "opencode:platform")
    #expect(Presenter.defaults(data) == ["opencodego:go"])
}
@Test func failedClientReadDoesNotLeaveApparentlyFreshCapacity() throws {
    let data = try snapshot([account()])
    let menu = Presenter.make(data, now: instant, failure: "Switchback refresh timed out")
    #expect(menu.groups[0].accounts[0].summary == "?")
    #expect(menu.status == "Switchback refresh timed out")
}

@Test func nullTimestampsDoNotHideHealthyAccounts() throws {
    var missingAccountTime = account("null-account")
    missingAccountTime["observed_at"] = NSNull()
    var missingWindowTime = window(state: "unknown")
    missingWindowTime["sampled_at"] = NSNull()
    let data = try snapshot([account("healthy"), missingAccountTime,
                             account("null-window", windows: [missingWindowTime])])
    let rows = Presenter.make(data, now: instant).groups[0].accounts
    #expect(rows.count == 3)
    #expect(rows[0].summary == "80%")
    #expect(rows[1].summary == "?")
    #expect(rows[2].summary == "?")
    #expect(rows[1].lines.contains("Sample time unavailable"))
}
@Test func ownerTimeoutMatchesRefreshCost() {
    #expect(RefreshRequest.saved.timeout == 30)
    #expect(RefreshRequest.scheduled.timeout == 600)
    #expect(RefreshRequest.forced.timeout == 600)
}

@Test func toolsAndUnknownScopesDoNotChangeGenerationPins() throws {
    var tools = window("mcp", used: 100, remaining: 0)
    tools["scope"] = "tools"
    var unknown = window("other", state: "unknown")
    unknown["scope"] = "unrecognized"
    var unknownTools = window("tools-missing", state: "unknown")
    unknownTools["scope"] = "tools"
    let data = try snapshot([account(windows: [window(), tools, unknown, unknownTools])])
    let menu = Presenter.make(data, now: instant)
    #expect(menu.groups[0].accounts[0].summary == "80%")
    #expect(menu.groups[0].accounts[0].lines.contains { $0.contains("mcp · tools: 0%") })
    #expect(menu.groups[0].accounts[0].lines.contains { $0.contains("unrecognized") })
    #expect(menu.compactTitle == "Cl 80%")
}
@Test func usageKindEstimatesNeverBecomeMeasuredPins() throws {
    var estimatedWindow = window()
    estimatedWindow["usage_kind"] = "estimated"
    var estimatedAccount = account("account-estimate")
    estimatedAccount["usage_kind"] = "estimated"
    let rows = Presenter.make(try snapshot([
        account("window-estimate", windows: [estimatedWindow]), estimatedAccount
    ]), now: instant).groups[0].accounts
    #expect(rows.allSatisfy { $0.summary == "?" })
    #expect(rows.allSatisfy { $0.lines.contains { $0.hasPrefix("Estimate ·") } })
}
@Test func compactDefaultsKeepFullIdentityInTooltipPresentation() throws {
    let data = try snapshot(Presenter.core.map { account("long-synthetic-slot-name", provider: $0) })
    let menu = Presenter.make(data, now: instant)
    #expect(menu.compactTitle.count < 50)
    #expect(menu.title.contains("long-synthetic-slot-name"))
}

@Test func providerAccountLabelFallbackUsesFamiliarHeading() throws {
    var row = account("max")
    row.removeValue(forKey: "label")
    row["account_label"] = "Synthetic Provider Account"
    let data = try snapshot([row])
    let menu = Presenter.make(data, now: instant)
    let account = menu.groups[0].accounts[0]
    #expect(account.title.hasPrefix("Account max"))
    #expect(account.title.contains("Synthetic Provider Account"))
    #expect(account.details.contains("Account: Claude · max"))
}

@Test func accountBooleanEstimateNeverBecomesMeasuredPin() throws {
    var estimated = account("boolean-estimate", windows: [window(), window("week", used: 40, remaining: 60)])
    estimated["estimated"] = true
    var measured = account("measured")
    measured["estimated"] = false
    let rows = Presenter.make(try snapshot([estimated, measured, account("unspecified")]),
                              now: instant).groups[0].accounts
    #expect(rows[0].summary == "?")
    #expect(rows[0].lines.contains("Estimated usage"))
    #expect(rows[0].lines.filter { $0.hasPrefix("Estimate ·") }.count == 2)
    #expect(rows[1].summary == "80%")
    #expect(rows[2].summary == "80%")
}
