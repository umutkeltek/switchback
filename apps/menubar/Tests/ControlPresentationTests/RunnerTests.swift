import Foundation
import Testing
@testable import ControlPresentation

// The process runner, with a stand-in program instead of `sb`: it waits on the process without polling, and a
// cancel, a timeout or too much output stops the process.

/// A directory for stand-in programs, on the test scratch volume when one is set.
func scratchDirectory() throws -> URL {
    let base = ProcessInfo.processInfo.environment["SWITCHBACK_MENUBAR_TEST_SCRATCH"].map { URL(fileURLWithPath: $0) }
        ?? FileManager.default.temporaryDirectory
    let directory = base.appendingPathComponent("control-runner-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    return directory
}

/// A stand-in `sb`: a shell script with `body`.
func standIn(_ body: String) throws -> (runner: ProcessCommandRunner, directory: URL) {
    let directory = try scratchDirectory()
    let script = directory.appendingPathComponent("sb")
    try ("#!/bin/sh\n" + body + "\n").write(to: script, atomically: true, encoding: .utf8)
    try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
    return (ProcessCommandRunner(executable: script), directory)
}

@Test func aRunCollectsBothOutputsAndTheExitCode() async throws {
    let (runner, directory) = try standIn(#"printf '{"ok":true}'; echo "sb ai: no gateway" >&2; exit 3"#)
    defer { try? FileManager.default.removeItem(at: directory) }
    let result = try await runner.run(Commands.status)
    #expect(result.status == 3)
    #expect(String(decoding: result.stdout, as: UTF8.self) == #"{"ok":true}"#)
    #expect(result.stderr == "sb ai: no gateway\n")
    // The arguments arrive exactly, and stdin is closed.
    let (echo, echoDirectory) = try standIn(#"for a in "$@"; do printf '%s|' "$a"; done; cat"#)
    defer { try? FileManager.default.removeItem(at: echoDirectory) }
    let echoed = try await echo.run(Commands.chat("fast", prompt: "-h"))
    #expect(String(decoding: echoed.stdout, as: UTF8.self) == "ai|chat|--json|--|fast|-h|")
}

@Test func cancellingARunStopsItsProcess() async throws {
    let (runner, directory) = try standIn("echo $$ > \"$(dirname \"$0\")/pid\"; exec sleep 30")
    defer { try? FileManager.default.removeItem(at: directory) }
    let pidFile = directory.appendingPathComponent("pid")
    let started = Date()
    let task = Task { try await runner.run(Commands.status) }
    for _ in 0..<400 where !FileManager.default.fileExists(atPath: pidFile.path) { try await Task.sleep(for: .milliseconds(5)) }
    let text = try String(contentsOf: pidFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
    let pid = try #require(Int32(text))
    task.cancel()
    do {
        _ = try await task.value
        Issue.record("a cancelled run finished")
    } catch let failure as CommandFailure {
        #expect(failure == .stopped && failure.cancelled)
    }
    #expect(Date().timeIntervalSince(started) < 5)
    // The process is gone, not left running in the background.
    for _ in 0..<200 where kill(pid, 0) == 0 { try await Task.sleep(for: .milliseconds(10)) }
    #expect(kill(pid, 0) == -1 && errno == ESRCH)
}

@Test func aSlowRunStopsAtItsTimeout() async throws {
    let (runner, directory) = try standIn("exec sleep 30")
    defer { try? FileManager.default.removeItem(at: directory) }
    let quick = ControlCommand(["ai", "status", "--json"], writes: false, timeout: 0.3)
    let started = Date()
    do {
        _ = try await runner.run(quick)
        Issue.record("a slow run finished")
    } catch let failure as CommandFailure {
        #expect(failure.message == "sb took too long to answer" && !failure.cancelled)
    }
    #expect(Date().timeIntervalSince(started) < 5)
}

@Test func tooMuchOutputStopsTheRun() async throws {
    let (runner, directory) = try standIn("head -c 9000000 /dev/zero; sleep 30")
    defer { try? FileManager.default.removeItem(at: directory) }
    do {
        _ = try await runner.run(Commands.status)
        Issue.record("an oversized answer was accepted")
    } catch let failure as CommandFailure {
        #expect(failure.message == "sb answered with too much output")
    }
}

@Test func aCommandTheWindowDoesNotKnowNeverStarts() async throws {
    let (runner, directory) = try standIn("touch \"$(dirname \"$0\")/ran\"")
    defer { try? FileManager.default.removeItem(at: directory) }
    do {
        _ = try await runner.run(ControlCommand(["ai", "tune", "x"], writes: true))
        Issue.record("an unknown command ran")
    } catch let failure as CommandFailure {
        #expect(failure.message == "The window does not run this command")
    }
    #expect(!FileManager.default.fileExists(atPath: directory.appendingPathComponent("ran").path))
}

/// The file descriptors open in this process now.
func openFileCount() -> Int {
    (0..<Int32(min(getdtablesize(), 65_536))).filter { fcntl($0, F_GETFD) != -1 }.count
}

@Test func stoppingARunWhileItWritesNeverCrashesOrLeaksFiles() async throws {
    // The stand-in keeps writing 64 KB at a time, so a stop lands while a pipe is being read.
    let (runner, directory) = try standIn("while :; do head -c 65536 /dev/zero; done")
    defer { try? FileManager.default.removeItem(at: directory) }
    let before = openFileCount()
    for round in 0..<20 {
        let task = Task { try await runner.run(Commands.status) }
        try await Task.sleep(for: .milliseconds(2 + (round % 5) * 3))
        task.cancel()
        do {
            _ = try await task.value
            Issue.record("a stopped run finished")
        } catch let failure as CommandFailure {
            #expect(failure.cancelled)
        }
    }
    // Each stopped stand-in closes its pipes when it is gone; then every pipe end the runs opened is closed.
    // Two leaked ends a run would leave 40 open; other tests running beside this one hold a few at most.
    for _ in 0..<400 where openFileCount() > before + 10 { try await Task.sleep(for: .milliseconds(10)) }
    #expect(openFileCount() <= before + 10, "open files: \(before) before, \(openFileCount()) after")
}
