import Foundation

/// One local notice for a pinned account window. The app only posts it; the crossing rule lives here.
public struct LimitNotice: Codable, Sendable, Equatable {
    public enum Kind: String, Codable, Sendable { case used80, used95, reset }
    public let id: String
    public let kind: Kind
    public let accountKey: String
    public let title: String
    public let body: String
}
public struct LimitNoticeResult: Sendable, Equatable {
    public let notices: [LimitNotice]
    /// Last notified level (0, 80 or 95) per account window; store it and pass it back next time.
    public let levels: [String: Int]
}

extension Presenter {
    /// Compares trusted pinned readings with the last notified level of each window.
    /// A window seen for the first time only records its level, so turning the option on never floods.
    /// Levels only rise until the window drops below 80% used; a drop from 95% is a reset notice.
    public static func limitNotices(_ snapshot: UsageSnapshot, now: Date = Date(), pins: Set<String>? = nil,
                                    enabled: Bool, previous: [String: Int], failure: String? = nil) -> LimitNoticeResult {
        guard enabled else { return LimitNoticeResult(notices: [], levels: [:]) }
        let pinned = snapshot.accounts.filter { (pins ?? defaults(snapshot)).contains($0.key) }
        let fresh = snapshotIsFresh(snapshot, failure: failure, now: now)
        var levels = previous.filter { entry in pinned.contains { entry.key.hasPrefix($0.key + "|") } }
        var notices: [LimitNotice] = []
        for account in pinned where !account.isEstimated {
            var occurrences: [String: Int] = [:]
            for window in account.windows {
                let base = [account.key, window.name, window.model ?? "", window.scope ?? ""].joined(separator: "|")
                let occurrence = occurrences[base, default: 0]
                occurrences[base] = occurrence + 1
                let id = base + "|\(occurrence)"
                guard isGeneration(window), !window.isEstimated,
                      valid(window, account: account, snapshotFresh: fresh, now: now),
                      let used = window.usedPercent ?? window.remainingPercent.map({ 100 - $0 }) else { continue }
                let level = used >= 95 ? 95 : used >= 80 ? 80 : 0
                guard let last = previous[id] else { levels[id] = level; continue }
                let kind: LimitNotice.Kind
                if level > last {
                    levels[id] = level
                    kind = level == 95 ? .used95 : .used80
                } else if level == 0 && last > 0 {
                    levels[id] = 0
                    guard last >= 95 else { continue }
                    kind = .reset
                } else {
                    continue
                }
                let model = window.model.map(clean).flatMap { $0.isEmpty ? nil : " (" + $0 + ")" } ?? ""
                let subject = name(account.provider) + " " + clean(account.slot) + ": " + windowTitle(window) + model
                let left = 100 - used
                let title: String
                switch kind {
                case .used80: title = subject + " over 80% used"
                case .used95: title = subject + " over 95% used"
                case .reset: title = subject + " reset"
                }
                let body = (percent(used) ?? "?") + " used · " + (percent(left) ?? "?") + " left" +
                    (kind == .reset ? "" : " · " + reset(window, now: now))
                notices.append(LimitNotice(id: "switchback.limit." + id + "." + kind.rawValue, kind: kind,
                                           accountKey: account.key, title: title, body: body))
            }
        }
        return LimitNoticeResult(notices: notices, levels: levels)
    }
}
