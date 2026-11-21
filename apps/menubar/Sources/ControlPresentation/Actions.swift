import Foundation

/// Whether an action may run now, and why not.
public enum Gate: Sendable, Equatable {
    case allowed
    case disabled(String)
    public var isAllowed: Bool { self == .allowed }
    public var reason: String? { if case .disabled(let reason) = self { return reason }; return nil }
}

/// The one question asked before an action that wakes the GPU box or powers it off.
public struct Confirmation: Sendable, Equatable {
    public let title: String
    public let message: String
    public let confirm: String
    public let destructive: Bool
}

/// A button's whole meaning: its label, the exact command, whether it wakes the GPU box, and the question first.
public struct ActionPlan: Sendable, Equatable, Identifiable {
    /// Stable per button ("power-off:gpu-box"), so its spinner and result line stay with it.
    public let id: String
    public let title: String
    public let command: ControlCommand
    public let wakes: Bool
    public let confirmation: Confirmation?
    public let gate: Gate
    public init(id: String, title: String, command: ControlCommand, wakes: Bool = false,
                confirmation: Confirmation? = nil, gate: Gate = .allowed) {
        self.id = id; self.title = title; self.command = command; self.wakes = wakes
        self.confirmation = confirmation; self.gate = gate
    }
    /// What a write that wakes the GPU box says on its button.
    public static let wakeSuffix = " · wakes the GPU box"
}

public enum WakeWarning {
    /// The button label for an action that would wake a sleeping machine.
    public static func label(_ action: String, wakes: Bool, machine: String = "GPU box") -> String {
        wakes ? action + " · wakes the \(machine)" : action
    }
    /// The one question asked before the wake, with how long a wake took last time.
    public static func confirmation(_ action: String, machine: String = "GPU box", wakeSeconds: Double?) -> Confirmation {
        let time = wakeSeconds.map { " Waking takes about \(Words.duration($0))." } ?? ""
        return Confirmation(title: "Wake the \(machine)?",
                            message: "\(action) wakes the \(machine), which is asleep.\(time) It powers off again when it has been idle.",
                            confirm: "Wake and \(action.prefix(1).lowercased() + action.dropFirst())", destructive: false)
    }
}
