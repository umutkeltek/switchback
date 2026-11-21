import Foundation

/// Numbers in short, plain words.
public enum Words {
    /// 45 s, 12 min, 1 h 5 min, 2 d 3 h.
    public static func duration(_ seconds: Double) -> String {
        let total = max(0, Int(seconds.rounded()))
        if total < 60 { return "\(total) s" }
        let minutes = total / 60
        if minutes < 60 { return "\(minutes) min" }
        let hours = minutes / 60, rest = minutes % 60
        if hours < 24 { return rest == 0 ? "\(hours) h" : "\(hours) h \(rest) min" }
        let days = hours / 24, hoursLeft = hours % 24
        return hoursLeft == 0 ? "\(days) d" : "\(days) d \(hoursLeft) h"
    }
    /// "just now", "3 min ago", "2 h ago".
    public static func ago(_ seconds: Double) -> String {
        seconds < 45 ? "just now" : duration(seconds) + " ago"
    }
    /// 18.4 GB, 640 MB. Memory is shown in powers of 1024 like the CLI, named in everyday words.
    public static func bytes(_ value: Double) -> String {
        let gib = value / 1_073_741_824
        if gib >= 10 { return String(format: "%.0f GB", gib) }
        if gib >= 1 { return String(format: "%.1f GB", gib) }
        return String(format: "%.0f MB", max(0, value / 1_048_576))
    }
    /// 32k, 128k, 4,096: a context size in tokens.
    public static func tokens(_ value: Int) -> String {
        if value >= 1024 && value % 1024 == 0 { return "\(value / 1024)k" }
        if value >= 10_000 { return String(format: "%.0fk", Double(value) / 1000) }
        return value.formatted(.number.grouping(.automatic))
    }
    /// 142 tok/s; 1 decimal below 10.
    public static func speed(_ value: Double) -> String {
        value < 10 ? String(format: "%.1f tok/s", value) : String(format: "%.0f tok/s", value)
    }
    /// The local clock time, with the day when it is not today.
    public static func clock(_ date: Date, now: Date, calendar: Calendar = .current) -> String {
        let formatter = DateFormatter()
        formatter.calendar = calendar
        formatter.timeZone = calendar.timeZone
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = calendar.isDate(date, inSameDayAs: now) ? "HH:mm" : "MMM d HH:mm"
        return formatter.string(from: date)
    }
    /// A setting key in everyday words.
    public static func setting(_ key: String) -> String {
        let known: [String: String] = [
            "temperature": "Temperature", "top_p": "Top P", "top_k": "Top K", "min_p": "Min P",
            "max_tokens": "Max tokens", "reasoning": "Thinking model", "reasoning.enabled": "Thinking",
            "reasoning.effort": "Thinking effort", "reasoning.budget_tokens": "Thinking budget",
            "reasoning_guard": "Reasoning guard", "reasoning_guard.enabled": "Reasoning guard",
            "reasoning_guard.min_tokens": "Guard minimum", "ceiling": "Speed ceiling",
            "ctx_size": "Context size", "load.ctx_size": "Context size",
            "cache_type_k": "KV cache", "load.cache_type_k": "KV cache",
            "cache_type_v": "KV cache (values)", "load.cache_type_v": "KV cache (values)",
            "parallel": "Parallel slots", "load.parallel": "Parallel slots",
            "seed": "Seed", "stop": "Stop words", "system_prompt": "System prompt",
            "repeat_penalty": "Repeat penalty", "presence_penalty": "Presence penalty",
            "frequency_penalty": "Frequency penalty",
        ]
        if let word = known[key] { return word }
        let last = key.split(separator: ".").last.map(String.init) ?? key
        let spaced = last.replacingOccurrences(of: "_", with: " ")
        return spaced.prefix(1).uppercased() + spaced.dropFirst()
    }
    /// A value for a person: whole numbers plain, booleans as on/off, short text.
    public static func value(_ value: JSONValue?) -> String {
        guard let value, !value.isNull else { return "engine default" }
        switch value {
        case .bool(let flag): return flag ? "on" : "off"
        case .number(let number): return JSONValue.numberText(number)
        case .string(let text): return text.count <= 40 ? text : String(text.prefix(37)) + "…"
        default:
            let text = value.compactText
            return text.count <= 40 ? text : String(text.prefix(37)) + "…"
        }
    }
    /// A sentence from `sb` with the gateway's internal words swapped for everyday ones.
    public static func plain(_ text: String) -> String {
        var line = text.replacingOccurrences(of: "engine of lane ", with: "engine of ")
        for (word, plain) in [("revisions", "versions"), ("revision", "version"), ("lanes", "engines"), ("lane", "engine"),
                              ("targets", "models"), ("target", "model"), ("pending_apply", "waiting to apply"),
                              ("overlay", "settings file")] {
            line = line.replacingOccurrences(of: "\\b\(word)\\b", with: plain, options: .regularExpression)
        }
        return line
    }
}
