import Foundation
import Darwin

public enum RefreshRequest: Sendable {
    case saved, scheduled, forced
    public var timeout: TimeInterval {
        switch self {
        case .saved: 30
        case .scheduled, .forced: 600
        }
    }
    public var arguments: [String] {
        switch self {
        case .saved: ["limits", "--json"]
        case .scheduled: ["limits", "--refresh-if-due", "--json"]
        case .forced: ["limits", "--refresh", "--json"]
        }
    }
}
public enum OwnerError: Error, Sendable, Equatable {
    case missingExecutable, launchFailed, timedOut, cancelled, outputTooLarge, failed
    public var message: String {
        switch self {
        case .missingExecutable: "Switchback command unavailable"
        case .launchFailed: "Could not start Switchback"
        case .timedOut: "Switchback refresh timed out"
        case .cancelled: "Refresh cancelled"
        case .outputTooLarge: "Switchback usage response is too large"
        case .failed: "Switchback could not read account usage"
        }
    }
}
public enum OwnerRunner {
    public static func executable() -> URL? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let paths = [home + "/.local/bin/sb", home + "/.switchback/bin/sb",
                     "/opt/homebrew/bin/sb", "/usr/local/bin/sb"]
        return paths.first { FileManager.default.isExecutableFile(atPath: $0) }.map { URL(fileURLWithPath: $0) }
    }
    // The app sends fixed owner arguments only. It never invokes a shell or touches credentials.
    @concurrent public static func run(_ request: RefreshRequest, executable: URL,
                                      timeout: TimeInterval? = nil, maximumBytes: Int = 1_048_576) async throws -> Data {
        guard !Task.isCancelled else { throw OwnerError.cancelled }
        let process = Process()
        let pipe = Pipe()
        process.executableURL = executable
        process.arguments = request.arguments
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        var environment = ProcessInfo.processInfo.environment
        environment["PATH"] = [home + "/.local/bin", home + "/.switchback/bin", "/opt/homebrew/bin",
                               "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].joined(separator: ":")
        process.environment = environment
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        do { try process.run() } catch { throw OwnerError.launchFailed }
        pipe.fileHandleForWriting.closeFile()
        let descriptor = pipe.fileHandleForReading.fileDescriptor
        _ = fcntl(descriptor, F_SETFL, O_NONBLOCK)
        defer { pipe.fileHandleForReading.closeFile() }
        let deadline = ProcessInfo.processInfo.systemUptime + max(0.01, timeout ?? request.timeout)
        var output = Data()
        var buffer = [UInt8](repeating: 0, count: 8192)
        func stopOwnProcess() {
            if process.isRunning { process.terminate() }
            let grace = Date().addingTimeInterval(0.3)
            while process.isRunning && Date() < grace { usleep(10_000) }
            if process.isRunning { _ = kill(process.processIdentifier, SIGKILL) }
            process.waitUntilExit()
        }
        while true {
            if Task.isCancelled { stopOwnProcess(); throw OwnerError.cancelled }
            if ProcessInfo.processInfo.systemUptime >= deadline { stopOwnProcess(); throw OwnerError.timedOut }
            let count = read(descriptor, &buffer, buffer.count)
            if count > 0 {
                guard output.count + count <= maximumBytes else {
                    stopOwnProcess(); throw OwnerError.outputTooLarge
                }
                output.append(contentsOf: buffer.prefix(count))
            } else if count == 0 {
                if !process.isRunning { break }
                usleep(10_000)
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                usleep(10_000)
            } else {
                stopOwnProcess(); throw OwnerError.failed
            }
        }
        process.waitUntilExit()
        guard process.terminationStatus == 0 else { throw OwnerError.failed }
        return output
    }
}
