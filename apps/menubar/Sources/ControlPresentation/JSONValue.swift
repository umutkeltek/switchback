import Foundation

/// Any JSON value. `sb` answers are read through this, so a field of an unexpected type is skipped
/// instead of failing the whole answer.
public enum JSONValue: Sendable, Equatable, Hashable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    /// Parses `data`; throws when it is not JSON.
    public static func parse(_ data: Data) throws -> JSONValue {
        let object = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        return JSONValue(any: object)
    }
    public init(any: Any) {
        switch any {
        case let number as NSNumber:
            // JSONSerialization gives booleans as NSNumber too; tell them apart by their type.
            if CFGetTypeID(number) == CFBooleanGetTypeID() { self = .bool(number.boolValue) }
            else { self = .number(number.doubleValue) }
        case let text as String: self = .string(text)
        case let list as [Any]: self = .array(list.map(JSONValue.init(any:)))
        case let map as [String: Any]: self = .object(map.mapValues(JSONValue.init(any:)))
        default: self = .null
        }
    }

    public subscript(key: String) -> JSONValue {
        if case .object(let map) = self { return map[key] ?? .null }
        return .null
    }
    public var object: [String: JSONValue]? { if case .object(let map) = self { return map }; return nil }
    public var array: [JSONValue] { if case .array(let list) = self { return list }; return [] }
    public var string: String? { if case .string(let text) = self { return text }; return nil }
    public var bool: Bool? { if case .bool(let value) = self { return value }; return nil }
    public var double: Double? {
        if case .number(let value) = self, value.isFinite { return value }
        return nil
    }
    public var int: Int? {
        guard let value = double, value.rounded() == value, abs(value) < 9e15 else { return nil }
        return Int(value)
    }
    public var isNull: Bool { self == .null }
    /// An empty object, an empty list or null: nothing is saved there.
    public var isEmpty: Bool {
        switch self {
        case .null: true
        case .object(let map): map.isEmpty
        case .array(let list): list.isEmpty
        default: false
        }
    }
    /// The value as `sb ai set` reads it after `key=`: JSON when it parses as JSON, else a plain string.
    public var argument: String {
        switch self {
        case .string(let text):
            // A string that would parse as JSON (a number, true) is quoted, so it stays a string.
            if (try? JSONValue.parse(Data(text.utf8))) != nil { return JSONValue.quoted(text) }
            return text
        default: return compactText
        }
    }
    /// Short JSON text with sorted keys; whole numbers without a decimal point.
    public var compactText: String {
        switch self {
        case .null: return "null"
        case .bool(let value): return value ? "true" : "false"
        case .number(let value): return JSONValue.numberText(value)
        case .string(let text): return JSONValue.quoted(text)
        case .array(let list): return "[" + list.map(\.compactText).joined(separator: ",") + "]"
        case .object(let map):
            return "{" + map.keys.sorted().map { JSONValue.quoted($0) + ":" + map[$0]!.compactText }.joined(separator: ",") + "}"
        }
    }
    static func quoted(_ text: String) -> String {
        let data = (try? JSONSerialization.data(withJSONObject: [text], options: [.withoutEscapingSlashes])) ?? Data()
        let list = String(decoding: data, as: UTF8.self)
        return String(list.dropFirst().dropLast())
    }
    /// 16384, 0.6, 0.95: at most four decimals, no trailing zeros.
    public static func numberText(_ value: Double) -> String {
        if value.rounded() == value && abs(value) < 1e15 { return String(Int(value)) }
        var text = String(format: "%.4f", value)
        while text.hasSuffix("0") { text.removeLast() }
        if text.hasSuffix(".") { text.removeLast() }
        return text
    }
}
