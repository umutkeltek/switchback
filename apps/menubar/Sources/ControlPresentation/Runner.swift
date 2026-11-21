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

/// A command that could not run to its end (no `sb`, a timeout, too much output, or it was stopped).
public struct CommandFailure: Error, Sendable, Equatable {
    public let message: String
    /// The command was stopped because what asked for it went away (the window hid, another model was picked):
    /// it says nothing about the gateway.
    public let cancelled: Bool
    public init(_ message: String, cancelled: Bool = false) { self.message = message; self.cancelled = cancelled }
    public static let stopped = CommandFailure("Stopped", cancelled: true)
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

    /// Starts `sb` and waits for it without polling: the pipes report output as it comes and the process reports
    /// its end. Cancelling the calling task stops the process at once.
    @concurrent public func run(_ command: ControlCommand) async throws -> CommandResult {
        guard command.isAllowed else { throw CommandFailure("The window does not run this command") }
        guard !Task.isCancelled else { throw CommandFailure.stopped }
        let process = Process()
        process.executableURL = executable
        process.arguments = command.arguments
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        var environment = ProcessInfo.processInfo.environment
        environment["PATH"] = [home + "/.local/bin", home + "/.switchback/bin", "/opt/homebrew/bin",
                               "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].joined(separator: ":")
        process.environment = environment
        process.standardInput = FileHandle.nullDevice
        let running = RunningCommand(process: process)
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                running.start(continuation, timeout: command.timeout)
            }
        } onCancel: {
            running.stop(CommandFailure.stopped)
        }
    }
}

/// One running `sb`. The wait ends when the process exited and both pipes closed, or at once when it is stopped
/// (cancelled, too slow, too much output); a stopped process gets SIGTERM, then SIGKILL 0.3 s later.
///
/// Each pipe is read only by its own handler, and only that handler closes it, after it read the end: stopping
/// never closes a pipe a handler may still be reading. A stopped process's pipes close when it is gone; until
/// then what it still writes is read and dropped.
final class RunningCommand: @unchecked Sendable {
    static let stdoutCeiling = 8_388_608
    static let stderrCeiling = 262_144
    static let chunk = 65_536
    private let lock = NSLock()
    private let process: Process
    private let out = Pipe(), err = Pipe()
    private var stdout = Data(), stderr = Data()
    private var outOpen = true, errOpen = true, exited = false, launched = false
    private var failure: CommandFailure?
    private var continuation: CheckedContinuation<CommandResult, any Error>?
    private var deadline: DispatchWorkItem?

    init(process: Process) {
        self.process = process
        process.standardOutput = out
        process.standardError = err
    }

    func start(_ continuation: CheckedContinuation<CommandResult, any Error>, timeout: TimeInterval) {
        lock.lock()
        if let failure {
            // Stopped before it started: it never runs.
            lock.unlock()
            closeAll()
            continuation.resume(throwing: failure)
            return
        }
        self.continuation = continuation
        lock.unlock()
        out.fileHandleForReading.readabilityHandler = { [self] handle in collect(handle, stdout: true) }
        err.fileHandleForReading.readabilityHandler = { [self] handle in collect(handle, stdout: false) }
        process.terminationHandler = { [self] process in
            process.terminationHandler = nil
            lock.withLock { exited = true }
            finishIfDone()
        }
        do {
            try process.run()
        } catch {
            // Nothing was written, so no handler is reading: the pipes can close here.
            out.fileHandleForReading.readabilityHandler = nil
            err.fileHandleForReading.readabilityHandler = nil
            process.terminationHandler = nil
            closeAll()
            let waiting = lock.withLock { () -> CheckedContinuation<CommandResult, any Error>? in
                defer { self.continuation = nil }
                return self.continuation
            }
            waiting?.resume(throwing: CommandFailure("Could not start sb"))
            return
        }
        // The process holds the write ends now; the pipes close when it (and anything it started) is done.
        out.fileHandleForWriting.closeFile()
        err.fileHandleForWriting.closeFile()
        let pending = lock.withLock { () -> CommandFailure? in
            launched = true
            return failure
        }
        if let pending { stop(pending); return }
        let item = DispatchWorkItem { [weak self] in self?.stop(CommandFailure("sb took too long to answer")) }
        lock.withLock { deadline = item }
        DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: item)
    }

    /// Stops the process and ends the wait with `reason`. Before the process started it only records the
    /// reason, and `start` stops it (or never starts it). The pipes stay with their handlers, which drain and
    /// close them once the stopped process is gone.
    func stop(_ reason: CommandFailure) {
        lock.lock()
        if failure == nil { failure = reason }
        guard launched else { lock.unlock(); return }
        let waiting = continuation
        continuation = nil
        let item = deadline
        lock.unlock()
        item?.cancel()
        if process.isRunning { process.terminate() }
        let process = self.process
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) {
            if process.isRunning { _ = kill(process.processIdentifier, SIGKILL) }
        }
        waiting?.resume(throwing: reason)
    }

    /// One read from a pipe, in its own handler. A read error counts as the end, never a trap.
    private func collect(_ handle: FileHandle, stdout isOut: Bool) {
        // A late call after this pipe's end never touches the closed handle.
        guard lock.withLock({ isOut ? outOpen : errOpen }) else { return }
        var buffer = [UInt8](repeating: 0, count: Self.chunk)
        let count = buffer.withUnsafeMutableBytes { Darwin.read(handle.fileDescriptor, $0.baseAddress, Self.chunk) }
        if count < 0 && (errno == EAGAIN || errno == EINTR) { return }
        guard count > 0 else {
            // The end (or a read error): this handler is the pipe's only reader, so it closes it.
            handle.readabilityHandler = nil
            handle.closeFile()
            lock.withLock { if isOut { outOpen = false } else { errOpen = false } }
            finishIfDone()
            return
        }
        let tooMuch = lock.withLock { () -> Bool in
            // Stopped: what the process still writes is dropped.
            guard failure == nil else { return false }
            if isOut { stdout.append(contentsOf: buffer[0..<count]) } else { stderr.append(contentsOf: buffer[0..<count]) }
            return stdout.count > Self.stdoutCeiling || stderr.count > Self.stderrCeiling
        }
        if tooMuch { stop(CommandFailure("sb answered with too much output")) } else { finishIfDone() }
    }

    private func finishIfDone() {
        lock.lock()
        guard exited, !outOpen, !errOpen, let waiting = continuation else { lock.unlock(); return }
        continuation = nil
        let item = deadline
        let result = CommandResult(status: process.terminationStatus, stdout: stdout,
                                   stderr: String(decoding: stderr, as: UTF8.self))
        lock.unlock()
        item?.cancel()
        waiting.resume(returning: result)
    }

    /// Closes every end of both pipes; only where no handler reads them.
    private func closeAll() {
        for pipe in [out, err] {
            pipe.fileHandleForReading.closeFile()
            pipe.fileHandleForWriting.closeFile()
        }
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
