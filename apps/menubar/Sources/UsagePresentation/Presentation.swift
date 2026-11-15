import Foundation

public struct MenuPresentation: Codable, Sendable {
    public let title: String
    public let compactTitle: String
    public let status: String
    public let groups: [ProviderMenu]
}
public struct ProviderMenu: Codable, Sendable {
    public let provider: String
    public let title: String
    public let accounts: [AccountMenu]
    public let unavailable: String?
}
public struct AccountMenu: Codable, Sendable {
    public let key: String
    public let title: String
    public let summary: String
    public let pinned: Bool
    public let lines: [String]
    public let details: [String]
}
public enum Presenter {
    public static let core = ["claude", "codex", "zai", "minimax", "opencodego"]
    public static func canonical(_ provider: String) -> String {
        switch provider.lowercased() {
        case "anthropic", "claude-code", "claude_code": "claude"
        case "openai", "openai-codex": "codex"
        case "z.ai", "z-ai": "zai"
        case "opencode-go", "opencode_go": "opencodego"
        default: provider.lowercased()
        }
    }
    public static func name(_ provider: String) -> String {
        switch canonical(provider) {
        case "claude": "Claude"
        case "codex": "Codex"
        case "zai": "ZAI"
        case "minimax": "MiniMax"
        case "opencodego": "OpenCode Go"
        default: clean(provider)
        }
    }
    static func abbreviation(_ provider: String) -> String {
        switch canonical(provider) {
        case "claude": "Cl"
        case "codex": "Cx"
        case "zai": "ZAI"
        case "minimax": "MM"
        case "opencodego": "Go"
        default: String(name(provider).prefix(5))
        }
    }
    public static func clean(_ text: String) -> String {
        String(text.unicodeScalars.map { CharacterSet.controlCharacters.contains($0) ? " " : String($0) }.joined().prefix(500))
    }
    static func percent(_ value: Double?) -> String? {
        guard let value, value.isFinite, (0...100).contains(value) else { return nil }
        return String(format: "%.0f%%", value)
    }
    public static func issue(_ code: String) -> String {
        switch code {
        case "login_required", "relogin_required", "authentication_failed", "unauthorized", "expired_credentials", "credentials_expired": "Sign in again"
        case "rate_limited", "http_429": "Refresh paused by provider"
        case "timeout", "refresh_timeout", "request_timeout": "Refresh timed out"
        case "network_error", "network_unavailable", "connection_error": "Could not reach provider"
        case "missing_credentials", "credentials_unavailable": "Account sign-in unavailable"
        case "unsupported", "unsupported_provider", "no_usage_endpoint": "Usage unavailable for this provider"
        case "invalid_response", "parse_error", "invalid_usage": "Provider returned unreadable usage"
        case "not_configured", "no_accounts": "No account available"
        default: "Refresh unavailable"
        }
    }
    static func age(_ value: String?, now: Date) -> String {
        guard let date = UsageTime.parse(value) else { return "Sample time unavailable" }
        let seconds = now.timeIntervalSince(date)
        guard seconds >= -30 else { return "Sample time is in the future" }
        if seconds < 60 { return "Sampled just now" }
        return "Sampled " + duration(seconds) + " ago"
    }
    static func duration(_ seconds: Double) -> String {
        let minutes = max(0, Int(seconds / 60))
        if minutes >= 1440 { return "\(minutes / 1440)d \((minutes % 1440) / 60)h" }
        if minutes >= 60 { return "\(minutes / 60)h \(minutes % 60)m" }
        return "\(minutes)m"
    }
    static func reset(_ window: UsageWindow, now: Date) -> String {
        guard let raw = window.resetsAt else { return "Reset time unavailable" }
        guard let date = UsageTime.parse(raw) else { return "Reset time invalid" }
        return date <= now ? "Reset passed · refresh needed" : "Resets in " + duration(date.timeIntervalSince(now))
    }
    static func valid(_ window: UsageWindow, account: UsageAccount, snapshotFresh: Bool, now: Date) -> Bool {
        guard snapshotFresh, account.state == "fresh", account.enabled != false,
              account.refreshError == nil, account.reloginRequired != true,
              UsageTime.fresh(account.observedAt, now: now),
              window.state == "fresh", UsageTime.fresh(window.sampledAt, now: now),
              percent(window.usedPercent) != nil || percent(window.remainingPercent) != nil else { return false }
        if let raw = window.resetsAt {
            guard let reset = UsageTime.parse(raw), reset > now else { return false }
        }
        if let used = window.usedPercent, percent(used) == nil { return false }
        if let left = window.remainingPercent, percent(left) == nil { return false }
        if let used = window.usedPercent, let left = window.remainingPercent, abs(used + left - 100) > 1 { return false }
        return true
    }
    public static func defaults(_ snapshot: UsageSnapshot) -> Set<String> {
        Set(core.compactMap { provider in snapshot.accounts.first { canonical($0.provider) == provider && $0.enabled != false }?.key })
    }
    public static func make(_ snapshot: UsageSnapshot, now: Date = Date(), pins: Set<String>? = nil,
                            refreshing: Bool = false, failure: String? = nil) -> MenuPresentation {
        let pins = pins ?? defaults(snapshot)
        let snapshotFresh = failure == nil && UsageTime.fresh(snapshot.generatedAt, now: now)
        let extras = Set(snapshot.accounts.map { canonical($0.provider) }).subtracting(core + ["localgateway"]).sorted()
        var groups: [ProviderMenu] = []
        var summaries: [String] = []
        var compactSummaries: [String] = []
        for provider in core + extras {
            let rows = snapshot.accounts.filter { canonical($0.provider) == provider }
            if rows.isEmpty && !core.contains(provider) { continue }
            let menus = rows.map { account -> AccountMenu in
                let identity = [account.label, account.email].compactMap { $0 }.filter { !$0.isEmpty }.map(clean)
                let title = "Slot \(clean(account.slot))" + (identity.isEmpty ? "" : " · " + Array(NSOrderedSet(array: identity)).compactMap { $0 as? String }.joined(separator: " · "))
                var lines: [String] = [age(account.observedAt, now: now)]
                if account.enabled == false { lines.append("Account disabled") }
                if account.reloginRequired == true { lines.append("Sign in again") }
                if let error = account.refreshError { lines.append(issue(error)) }
                if !snapshotFresh || !UsageTime.fresh(account.observedAt, now: now) || account.state != "fresh" {
                    lines.append("Usage is old or unavailable · refresh needed")
                }
                var remaining: [Double] = []
                var hasUnknown = false
                for window in account.windows {
                    let good = valid(window, account: account, snapshotFresh: snapshotFresh, now: now)
                    let estimate = window.estimated == true || window.usageKind == "estimated" || account.usageKind == "estimated"
                    let scope = window.scope?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
                    let generation = scope == nil || scope == "" || scope == "generation"
                    let qualified = [window.name, window.model, window.scope].compactMap { $0 }.map(clean).joined(separator: " · ")
                    let prefix = estimate ? "Estimate · " : ""
                    let usage: String
                    if good {
                        let left = window.remainingPercent ?? window.usedPercent.map { 100 - $0 }
                        let used = window.usedPercent ?? window.remainingPercent.map { 100 - $0 }
                        usage = "\(percent(left) ?? "?") remaining · \(percent(used) ?? "?") used"
                        if !estimate && generation, let left { remaining.append(left) }
                    } else {
                        usage = "? remaining · refresh needed"
                        if !estimate && generation { hasUnknown = true }
                    }
                    lines.append(prefix + qualified + ": " + usage)
                    lines.append("  " + reset(window, now: now) + " · " + age(window.sampledAt, now: now))
                }
                for balance in account.balances {
                    let currency = clean(balance.currencyCode ?? "")
                    func amount(_ number: Double?) -> String? {
                        guard let number, number.isFinite else { return nil }
                        return String(format: "%.2f", number) + (currency.isEmpty ? "" : " " + currency)
                    }
                    let fields = [amount(balance.balance).map { "balance " + $0 },
                                  amount(balance.used).map { "spent " + $0 },
                                  amount(balance.limit).map { "limit " + $0 }].compactMap { $0 }
                    lines.append("Balance · " + clean(balance.kind) + ": " + (fields.isEmpty ? "unavailable" : fields.joined(separator: " · ")) + (balance.period.map { " · " + clean($0) } ?? ""))
                }
                lines.append(contentsOf: account.notes.map { clean($0) })
                if account.windows.isEmpty && account.balances.isEmpty { lines.append("Usage unavailable") }
                let summary = hasUnknown ? "?" : remaining.min().flatMap { percent($0) } ?? "?"
                let pinned = pins.contains(account.key)
                if pinned {
                    summaries.append("\(name(provider)) \(clean(account.slot)): \(summary)")
                    let sameProviderPins = rows.filter { pins.contains($0.key) }.count
                    let shortSlot = sameProviderPins > 1 ? " " + String(clean(account.slot).prefix(8)) + ":" : ""
                    compactSummaries.append(abbreviation(provider) + shortSlot + " " + summary)
                }
                return AccountMenu(key: account.key, title: title, summary: summary, pinned: pinned, lines: lines,
                                   details: ["Source: " + clean(account.source), age(account.observedAt, now: now)])
            }
            let gap = snapshot.coverage.first { canonical($0.provider) == provider }
            groups.append(ProviderMenu(provider: provider, title: name(provider), accounts: menus,
                                       unavailable: menus.isEmpty ? gap.map { issue($0.reason) } ?? "No account usage available" : nil))
        }
        return MenuPresentation(title: summaries.isEmpty ? "Switchback ◇" : summaries.joined(separator: "  ·  "),
                                compactTitle: compactSummaries.isEmpty ? "Switchback ◇" : compactSummaries.joined(separator: "  "),
                                status: refreshing || snapshot.refresh?.state == "in_progress" ? "Refreshing…" : failure ?? "Account limits · remaining capacity",
                                groups: groups)
    }
    public static func text(_ menu: MenuPresentation) -> String {
        ([menu.title, menu.status] + menu.groups.flatMap { group in
            [group.title] + (group.unavailable.map { [$0] } ?? []) + group.accounts.flatMap { [$0.title] + $0.lines }
        }).joined(separator: "\n")
    }
}
