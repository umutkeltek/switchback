import Foundation

/// One `sb` command with exact arguments. The window runs nothing else: no shell, no string to split.
public struct ControlCommand: Sendable, Equatable, Hashable {
    /// The arguments after `sb`.
    public let arguments: [String]
    /// True for a command that changes something (a setting, a hold, the power, a loaded model).
    public let writes: Bool
    public let timeout: TimeInterval

    init(_ arguments: [String], writes: Bool, timeout: TimeInterval = 60) {
        self.arguments = arguments; self.writes = writes; self.timeout = timeout
    }
    /// The command as the owner would type it, for the "copy command" affordance.
    public var display: String { (["sb"] + arguments).map(ControlCommand.quoted).joined(separator: " ") }

    /// Shell-safe spelling of one argument: plain when it has no special characters, else in single quotes.
    public static func quoted(_ argument: String) -> String {
        let plain = CharacterSet(charactersIn:
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_./:=,@%+")
        if !argument.isEmpty && argument.unicodeScalars.allSatisfy(plain.contains) { return argument }
        return "'" + argument.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// The `sb ai` subcommands the window may run; anything else is refused before it starts.
    static let reads: Set<String> = ["status", "models", "engines", "settings", "doctor"]
    static let writeCommands: Set<String> = ["set", "unset", "revert", "load", "unload", "hold", "release", "on", "off", "chat"]
    /// True when the arguments are one of the commands the window knows, with a write flag that matches.
    public var isAllowed: Bool {
        if arguments == ["limits", "--json"] { return !writes }
        guard arguments.count >= 2, arguments[0] == "ai" else { return false }
        if ControlCommand.reads.contains(arguments[1]) { return !writes }
        return ControlCommand.writeCommands.contains(arguments[1]) && writes
    }
}

/// How a load setting change reaches a loaded model.
public enum ApplyMode: String, Sendable {
    /// The loaded model keeps its old settings until it loads again.
    case nextLoad = "next-load"
    /// The model reloads once nothing is in flight on it.
    case now
}

/// Every command the window can run, built with exact arguments.
public enum Commands {
    // Reads: none of them wakes a machine. The gateway answers most from what it already knows; the engines read
    // may check an engine's health, which reaches a machine that is on.
    public static let status = ControlCommand(["ai", "status", "--json"], writes: false, timeout: 30)
    public static let models = ControlCommand(["ai", "models", "--json"], writes: false, timeout: 30)
    public static let engines = ControlCommand(["ai", "engines", "--json"], writes: false, timeout: 30)
    public static let limits = ControlCommand(["limits", "--json"], writes: false, timeout: 30)
    public static func doctor(_ model: String? = nil) -> ControlCommand {
        ControlCommand(["ai", "doctor"] + (model.map { [$0] } ?? []) + ["--json"], writes: false, timeout: 30)
    }
    public static func settings(_ model: String) -> ControlCommand {
        ControlCommand(["ai", "settings", model, "--json"], writes: false, timeout: 30)
    }
    public static func history(_ target: String) -> ControlCommand {
        ControlCommand(["ai", "settings", target, "--history", "--json"], writes: false, timeout: 30)
    }

    // Writes.
    /// Saves request settings and target-wide keys (`reasoning_guard.*`) for one target.
    public static func set(_ target: String, _ pairs: [(String, JSONValue)]) -> ControlCommand {
        ControlCommand(["ai", "set", target] + pairs.map { "\($0.0)=\($0.1.argument)" }, writes: true, timeout: 150)
    }
    /// Saves load settings; `apply` says when a loaded model takes them. Never wakes a sleeping machine.
    public static func setLoad(_ target: String, _ pairs: [(String, JSONValue)], apply: ApplyMode) -> ControlCommand {
        ControlCommand(["ai", "set", target] + pairs.map { "\($0.0)=\($0.1.argument)" } + ["--load", "--apply", apply.rawValue],
                       writes: true, timeout: 150)
    }
    /// Removes saved keys: the engine's own value applies again.
    public static func unset(_ target: String, _ keys: [String], load: Bool = false) -> ControlCommand {
        ControlCommand(["ai", "unset", target] + keys + (load ? ["--load"] : []), writes: true, timeout: 150)
    }
    /// Puts back the settings of an earlier version.
    public static func revert(_ target: String, to revision: Int) -> ControlCommand {
        ControlCommand(["ai", "revert", target, String(revision)], writes: true, timeout: 150)
    }
    public static func load(_ target: String, wake: Bool) -> ControlCommand {
        ControlCommand(["ai", "load", target] + (wake ? ["--wake"] : []), writes: true, timeout: 920)
    }
    public static func unload(_ target: String) -> ControlCommand {
        ControlCommand(["ai", "unload", target], writes: true, timeout: 120)
    }
    /// Keeps a machine that is on, on: never wakes it.
    public static func hold(_ duration: String, reason: String, machine: String) -> ControlCommand {
        ControlCommand(["ai", "hold", duration, reason, "--host", machine], writes: true)
    }
    public static func release(_ hold: String, machine: String) -> ControlCommand {
        ControlCommand(["ai", "release", hold, "--host", machine], writes: true)
    }
    /// Keeps a machine on and wakes it when it sleeps.
    public static func on(_ duration: String, reason: String, machine: String) -> ControlCommand {
        ControlCommand(["ai", "on", duration, reason, "--host", machine], writes: true)
    }
    public static func off(_ machine: String) -> ControlCommand {
        ControlCommand(["ai", "off", "--host", machine], writes: true, timeout: 120)
    }
    /// One quick request to try a model. A request to a sleeping machine's model wakes it. `--json` comes first and
    /// `--` ends the options, so a prompt that starts with "-" (even "-h") is sent as text.
    public static func chat(_ model: String, prompt: String) -> ControlCommand {
        ControlCommand(["ai", "chat", "--json", "--", model, prompt], writes: true, timeout: 920)
    }
}
