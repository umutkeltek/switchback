import Foundation

public struct UsageSnapshot: Decodable, Sendable {
    public let schema: String
    public let generatedAt: String
    public let purpose: String
    public let accounts: [UsageAccount]
    public let coverage: [Coverage]
    public let refresh: Refresh?
    enum CodingKeys: String, CodingKey {
        case schema, purpose, accounts, coverage, refresh
        case generatedAt = "generated_at"
    }
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        schema = try c.decode(String.self, forKey: .schema)
        generatedAt = try c.decode(String.self, forKey: .generatedAt)
        purpose = try c.decode(String.self, forKey: .purpose)
        accounts = try c.decode([UsageAccount].self, forKey: .accounts)
        coverage = try c.decodeIfPresent([Coverage].self, forKey: .coverage) ?? []
        refresh = try c.decodeIfPresent(Refresh.self, forKey: .refresh)
    }
    public static func decode(_ data: Data) throws -> UsageSnapshot {
        let snapshot = try JSONDecoder().decode(Self.self, from: data)
        guard snapshot.schema == "switchback/account-usage@1",
              snapshot.purpose == "usage_observation_not_admission_authority" else {
            throw SnapshotError.unsupportedSchema
        }
        return snapshot
    }
}
public enum SnapshotError: Error { case unsupportedSchema }
public struct Refresh: Decodable, Sendable { public let state: String }
public struct Coverage: Decodable, Sendable {
    public let provider: String
    public let state: String
    public let reason: String
}
public struct UsageAccount: Decodable, Sendable {
    public let provider: String
    public let slot: String
    public let label: String?
    public let email: String?
    public let enabled: Bool?
    public let source: String
    public let observedAt: String?
    public let state: String
    public let usageKind: String?
    public let estimated: Bool?
    public let refreshError: String?
    public let reloginRequired: Bool?
    public let windows: [UsageWindow]
    public let balances: [UsageBalance]
    public let notes: [String]
    public var key: String { provider + ":" + slot }
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        provider = try c.decode(String.self, forKey: .provider)
        slot = try c.decode(String.self, forKey: .slot)
        label = try c.decodeIfPresent(String.self, forKey: .label) ?? c.decodeIfPresent(String.self, forKey: .accountLabel)
        email = try c.decodeIfPresent(String.self, forKey: .email)
        enabled = try c.decodeIfPresent(Bool.self, forKey: .enabled)
        source = try c.decode(String.self, forKey: .source)
        observedAt = try c.decodeIfPresent(String.self, forKey: .observedAt)
        state = try c.decode(String.self, forKey: .state)
        usageKind = try c.decodeIfPresent(String.self, forKey: .usageKind)
        estimated = try c.decodeIfPresent(Bool.self, forKey: .estimated)
        refreshError = try c.decodeIfPresent(String.self, forKey: .refreshError)
        reloginRequired = try c.decodeIfPresent(Bool.self, forKey: .reloginRequired)
        windows = try c.decodeIfPresent([UsageWindow].self, forKey: .windows) ?? []
        balances = try c.decodeIfPresent([UsageBalance].self, forKey: .balances) ?? []
        notes = try c.decodeIfPresent([String].self, forKey: .notes) ?? []
    }
    enum CodingKeys: String, CodingKey {
        case provider, slot, label, email, enabled, source, state, windows, balances, notes, estimated
        case accountLabel = "account_label"
        case usageKind = "usage_kind"
        case observedAt = "observed_at", refreshError = "refresh_error", reloginRequired = "relogin_required"
    }
}
public struct UsageWindow: Decodable, Sendable {
    public let name: String
    public let usedPercent: Double?
    public let remainingPercent: Double?
    public let resetsAt: String?
    public let sampledAt: String?
    public let state: String
    public let windowMinutes: Double?
    public let model: String?
    public let scope: String?
    public let estimated: Bool?
    public let usageKind: String?
    enum CodingKeys: String, CodingKey {
        case name, state, model, scope, estimated
        case usageKind = "usage_kind"
        case usedPercent = "used_percent", remainingPercent = "remaining_percent"
        case resetsAt = "resets_at", sampledAt = "sampled_at", windowMinutes = "window_minutes"
    }
}
public struct UsageBalance: Decodable, Sendable {
    public let kind: String
    public let used: Double?
    public let limit: Double?
    public let balance: Double?
    public let currencyCode: String?
    public let period: String?
}
public enum UsageTime {
    public static func parse(_ value: String?) -> Date? {
        guard let value else { return nil }
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.date(from: value) ?? ISO8601DateFormatter().date(from: value)
    }
    public static func fresh(_ value: String?, now: Date) -> Bool {
        guard let date = parse(value) else { return false }
        let age = now.timeIntervalSince(date)
        return age >= -30 && age <= 900
    }
}
