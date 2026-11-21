import Foundation
import Darwin

/// What one `sb` command printed and how it ended.
public struct CommandResult: Sendable, Equatable {
    public let status: Int32
    public let stdout: Data
    public let stderr: String
    public init(status: Int32, stdout: Data, stderr: String = "") {
        self.status = status; self.stdout = stdout; self.stderr = stderr
    }
    public var succeeded: Bool { status == 0 }
}

/// A command that could not run to its end (no `sb`, a timeout, too much output).
public struct CommandFailure: Error, Sendable, Equatable {
    public let message: String
    public init(_ message: String) { self.message = message }
}

/// Runs `sb` commands. The window holds a fake one in tests and a real one in the app.
public protocol CommandRunning: Sendable {
    func run(_ command: ControlCommand) async throws -> CommandResult
}

/// Runs the installed `sb` with exact arguments: no shell, no keys, stdin closed.
public struct ProcessCommandRunner: CommandRunning {
    public let executable: URL
    public init(executable: URL) { self.executable = executable }

    /// The installed `sb`: the user's private bins first, then the standard Homebrew bins.
    public static func installed() -> ProcessCommandRunner? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let paths = [home + "/.local/bin/sb", home + "/.switchback/bin/sb", "/opt/homebrew/bin/sb", "/usr/local/bin/sb"]
        return paths.first { FileManager.default.isExecutableFile(atPath: $0) }
            .map { ProcessCommandRunner(executable: URL(fileURLWithPath: $0)) }
    }

    @concurrent public func run(_ command: ControlCommand) async throws -> CommandResult {
        guard command.isAllowed else { throw CommandFailure("The window does not run this command") }
        guard !Task.isCancelled else { throw CommandFailure("Stopped") }
        let process = Process()
        let out = Pipe(), err = Pipe()
        process.executableURL = executable
        process.arguments = command.arguments
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        var environment = ProcessInfo.processInfo.environment
        environment["PATH"] = [home + "/.local/bin", home + "/.switchback/bin", "/opt/homebrew/bin",
                               "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].joined(separator: ":")
        process.environment = environment
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = out
        process.standardError = err
        do { try process.run() } catch { throw CommandFailure("Could not start sb") }
        out.fileHandleForWriting.closeFile(); err.fileHandleForWriting.closeFile()
        let descriptors = [out.fileHandleForReading.fileDescriptor, err.fileHandleForReading.fileDescriptor]
        for descriptor in descriptors { _ = fcntl(descriptor, F_SETFL, O_NONBLOCK) }
        defer { out.fileHandleForReading.closeFile(); err.fileHandleForReading.closeFile() }
        let deadline = ProcessInfo.processInfo.systemUptime + command.timeout
        var stdout = Data(), stderr = Data()
        var open = [true, true]
        var buffer = [UInt8](repeating: 0, count: 16_384)
        func stop() {
            if process.isRunning { process.terminate() }
            let grace = Date().addingTimeInterval(0.3)
            while process.isRunning && Date() < grace { usleep(10_000) }
            if process.isRunning { _ = kill(process.processIdentifier, SIGKILL) }
            process.waitUntilExit()
        }
        while open.contains(true) {
            if Task.isCancelled { stop(); throw CommandFailure("Stopped") }
            if ProcessInfo.processInfo.systemUptime >= deadline { stop(); throw CommandFailure("sb took too long to answer") }
            var progressed = false
            for (index, descriptor) in descriptors.enumerated() where open[index] {
                let count = read(descriptor, &buffer, buffer.count)
                if count > 0 {
                    progressed = true
                    if index == 0 { stdout.append(contentsOf: buffer.prefix(count)) } else { stderr.append(contentsOf: buffer.prefix(count)) }
                    if stdout.count > 8_388_608 || stderr.count > 262_144 { stop(); throw CommandFailure("sb answered with too much output") }
                } else if count == 0 {
                    open[index] = false
                } else if errno != EAGAIN && errno != EWOULDBLOCK {
                    open[index] = false
                }
            }
            if !progressed { usleep(10_000) }
        }
        process.waitUntilExit()
        return CommandResult(status: process.terminationStatus, stdout: stdout, stderr: String(decoding: stderr, as: UTF8.self))
    }
}

/// The one line the window shows after an action: what happened, or the reason `sb` gave.
public enum Outcome {
    /// The reason `sb` gave on stderr, as one plain line.
    public static func errorLine(_ result: CommandResult) -> String {
        let lines = result.stderr.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty && !$0.hasPrefix("(see ") }
        guard var line = lines.last else { return "sb stopped with exit code \(result.status)" }
        for prefix in ["sb ai: ", "sb: "] where line.hasPrefix(prefix) { line = String(line.dropFirst(prefix.count)) }
        line = Words.plain(line)
        return line.prefix(1).uppercased() + line.dropFirst()
    }

    /// What a successful write did, in a few words.
    public static func successLine(_ command: ControlCommand, stdout: Data) -> String {
        let text = String(decoding: stdout, as: UTF8.self)
        let lines = text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        let verb = command.arguments.count > 1 ? command.arguments[1] : ""
        func version(_ line: String) -> String? {
            line.range(of: #"revision (\d+)"#, options: .regularExpression).map { String(line[$0].dropFirst("revision ".count)) }
        }
        func durationWords() -> String {
            command.arguments.count > 2 ? Machines.durationWords(command.arguments[2]) : ""
        }
        switch verb {
        case "set", "unset":
            var words = version(lines.first ?? "").map { "Saved · version \($0)" } ?? "Saved"
            if let note = lines.dropFirst().first { words += " · " + Words.plain(note) }
            return words
        case "revert":
            let back = command.arguments.last ?? ""
            return version(lines.first ?? "").map { "Back to version \(back)’s settings · now version \($0)" } ?? "Back to version \(back)’s settings"
        case "load":
            let status = lines.first.flatMap { $0.components(separatedBy: ": ").last } ?? "loaded"
            let word = status.prefix(1).uppercased() + status.dropFirst()
            return Words.plain(word.replacingOccurrences(of: "GiB", with: "GB"))
        case "unload":
            let message = lines.first.flatMap { $0.components(separatedBy: ": ").dropFirst().joined(separator: ": ") } ?? ""
            return message.isEmpty ? "Unloaded" : Words.plain(message.prefix(1).uppercased() + message.dropFirst())
        case "hold": return "Kept on for \(durationWords())"
        case "on": return text.contains("is waking") ? "Waking · kept on for \(durationWords())" : "Kept on for \(durationWords())"
        case "off": return text.contains("already off") ? "Already asleep" : "Powering off"
        case "release": return "Released"
        default: return "Done"
        }
    }
}
