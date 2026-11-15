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
func snapshot(_ accounts: [[String: Any]], generated: TimeInterval = 0, coverage: [[String: Any]] = []) throws -> UsageSnapshot {
    try UsageSnapshot.decode(JSONSerialization.data(withJSONObject: [
        "schema": "switchback/account-usage@1", "generated_at": stamp(generated),
        "purpose": "usage_observation_not_admission_authority", "accounts": accounts,
        "coverage": coverage
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
    let gaps: [[String: Any]] = [["provider": "zai", "state": "unavailable", "reason": "missing_credentials"]]
    let menu = Presenter.make(try snapshot(rows, coverage: gaps), now: instant)
    #expect(menu.groups[0].accounts[0].summary == "USD 12.25")
    #expect(menu.groups[0].accounts[1].summary == "?")
    #expect(menu.groups[0].accounts[2].summary == "?")
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

@Test(arguments: [0.0, 12.25])
func explicitZenBalancePinsCurrency(amount: Double) throws {
    let balance: [String: Any] = ["kind": "prepaid", "balance": amount, "currencyCode": "USD", "period": "Zen balance"]
    let data = try snapshot([account("workspace", provider: "opencodezen", windows: [], balances: [balance])])
    let menu = Presenter.make(data, now: instant)
    let zen = try #require(menu.groups.first { $0.provider == "opencodezen" })
    let expected = String(format: "USD %.2f", amount)
    #expect(zen.title == "OpenCode Zen")
    #expect(zen.accounts[0].summary == expected)
    #expect(zen.accounts[0].pinned)
    #expect(menu.compactTitle == "Zen " + expected)
    #expect(!menu.title.contains("%"))
    #expect(zen.accounts[0].lines.contains { $0.hasPrefix("Balance sampled: ") && $0.contains(stamp()) })
}
@Test func untrustedZenBalancesKeepLastReadingButNeverPinIt() throws {
    let balance: [String: Any] = ["kind": "prepaid", "balance": 12.25, "currencyCode": "USD"]
    func zen(_ slot: String) -> [String: Any] { account(slot, provider: "opencodezen", windows: [], balances: [balance]) }
    var old = zen("old"); old["observed_at"] = stamp(-901)
    var unknown = zen("unknown"); unknown["state"] = "unknown"
    var failed = zen("failed"); failed["refresh_error"] = "timeout"
    var disabled = zen("disabled"); disabled["enabled"] = false
    var estimate = zen("estimate"); estimate["estimated"] = true
    var kindEstimate = zen("kind-estimate"); kindEstimate["usage_kind"] = "estimated"
    var future = zen("future"); future["observed_at"] = stamp(31)
    var login = zen("login"); login["relogin_required"] = true
    var balanceEstimate = balance; balanceEstimate["estimated"] = true
    let estimatedReading = account("reading-estimate", provider: "opencodezen", windows: [], balances: [balanceEstimate])
    let data = try snapshot([old, unknown, failed, disabled, estimate, kindEstimate, future, login, estimatedReading])
    let menu = Presenter.make(data, now: instant, pins: Set(data.accounts.map(\.key)))
    let rows = try #require(menu.groups.first { $0.provider == "opencodezen" }).accounts
    #expect(rows.allSatisfy { $0.summary == "?" && $0.pinned })
    #expect(rows.allSatisfy { $0.lines.contains { $0.hasPrefix("Last reading ·") && $0.contains("12.25 USD") } })
    #expect(rows.allSatisfy { $0.lines.contains { $0.hasPrefix("Balance sampled: ") } })
    let staleSnapshot = Presenter.make(try snapshot([zen("snapshot")], generated: -901), now: instant)
    #expect(staleSnapshot.groups.first { $0.provider == "opencodezen" }?.accounts.first?.summary == "?")
}
@Test func balancePinRequiresExplicitAmountAndCurrency() throws {
    let readings: [[String: Any]] = [
        ["kind": "prepaid", "used": 12.25, "limit": 0, "currencyCode": "USD"],
        ["kind": "prepaid", "balance": 12.25],
        ["kind": "prepaid", "balance": 12.25, "currencyCode": "   "]
    ]
    let data = try snapshot(readings.enumerated().map {
        account(String($0.offset), provider: "opencodezen", windows: [], balances: [$0.element])
    })
    let rows = try #require(Presenter.make(data, now: instant).groups.first { $0.provider == "opencodezen" }).accounts
    #expect(rows.allSatisfy { $0.summary == "?" })
}
@Test func goQuotaAndZenBalanceRemainSeparatePins() throws {
    let balance: [String: Any] = ["kind": "prepaid", "balance": 12.25, "currencyCode": "USD"]
    let data = try snapshot([
        account("workspace", provider: "opencodego"),
        account("workspace", provider: "opencodezen", windows: [], balances: [balance]),
        account("legacy", provider: "opencode")
    ])
    let menu = Presenter.make(data, now: instant)
    #expect(Presenter.defaults(data) == ["opencodego:workspace", "opencodezen:workspace"])
    #expect(menu.groups.first { $0.provider == "opencodego" }?.accounts.first?.summary == "80%")
    #expect(menu.groups.first { $0.provider == "opencodezen" }?.accounts.first?.summary == "USD 12.25")
    #expect(menu.compactTitle == "Go 80%  Zen USD 12.25")
    #expect(menu.groups.first { $0.provider == "opencode" }?.accounts.first?.key == "opencode:legacy")
    #expect(Presenter.canonical("opencode") == "opencode")
}
@Test func quotaWindowsTakePrecedenceOverCurrencyBalance() throws {
    let balance: [String: Any] = ["kind": "prepaid", "balance": 12.25, "currencyCode": "USD"]
    let data = try snapshot([account("workspace", provider: "opencodego", balances: [balance])])
    #expect(Presenter.make(data, now: instant).groups.first { $0.provider == "opencodego" }?.accounts.first?.summary == "80%")
}

@Test func explicitCoreCoverageAppearsWithoutInventingAccounts() throws {
    let gaps: [[String: Any]] = [
        ["provider": "opencodego", "state": "unavailable", "reason": "no_saved_usage"],
        ["provider": "opencodezen", "state": "unknown", "reason": "no_saved_usage"]
    ]
    let data = try snapshot([], coverage: gaps)
    let menu = Presenter.make(data, now: instant, pins: [])
    #expect(Presenter.defaults(data).isEmpty)
    #expect(menu.groups.flatMap(\.accounts).isEmpty)
    #expect(menu.compactTitle == "Go ?  Zen ?")
    #expect(menu.title == "OpenCode Go: ?  ·  OpenCode Zen: ?")
    #expect(menu.groups.first { $0.provider == "opencodego" }?.unavailable == "No saved usage yet")
    #expect(menu.groups.first { $0.provider == "opencodezen" }?.unavailable == "No saved usage yet")
}
@Test func coverageStatusDoesNotChangeAccountPinPreferences() throws {
    let gaps: [[String: Any]] = [
        ["provider": "opencodego", "state": "unknown", "reason": "no_saved_usage"],
        ["provider": "opencodezen", "state": "unknown", "reason": "no_saved_usage"],
        ["provider": "legacy-provider", "state": "unknown", "reason": "no_saved_usage"]
    ]
    let data = try snapshot([account("workspace", provider: "opencodego")], coverage: gaps)
    #expect(Presenter.defaults(data) == ["opencodego:workspace"])
    let unpinned = Presenter.make(data, now: instant, pins: [])
    #expect(unpinned.compactTitle == "Zen ?")
    #expect(unpinned.groups.first { $0.provider == "opencodego" }?.accounts.count == 1)
    #expect(unpinned.groups.first { $0.provider == "opencodego" }?.accounts.first?.pinned == false)
    let defaults = Presenter.make(data, now: instant)
    #expect(defaults.compactTitle == "Go 80%  Zen ?")
    #expect(!defaults.title.contains("legacy-provider"))
}
@Test func noCoverageDoesNotCreateTopbarIndicators() throws {
    let data = try snapshot([])
    let menu = Presenter.make(data, now: instant)
    #expect(menu.compactTitle == "Switchback ◇")
    #expect(menu.groups.flatMap(\.accounts).isEmpty)
    #expect(Presenter.defaults(data).isEmpty)
}
