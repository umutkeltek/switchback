import Foundation
import Testing
@testable import ControlPresentation

// How the window reads: one debounced read per model pick, a pick that replaces a running read stops it (and its
// process), and a read stopped because the window hid is no gateway problem.

@MainActor @Test func pickingModelsQuicklyReadsOnlyTheLastPick() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now }, sleep: { _ in try await Task.sleep(for: .seconds(3600)) },
                             debounce: 0.3)
    store.setVisible(true)
    try await eventually { store.lastRead != nil && store.sweepTask == nil }
    let before = runner.calls.count
    store.selectedModel = "model-a"
    store.selectedModel = "model-b"
    store.selectedModel = "fast"
    try await Task.sleep(for: .milliseconds(20))
    #expect(runner.calls.count == before, "a pick was read before it held for the debounce")
    await store.selectionTask?.value
    let reads = Array(runner.calls.dropFirst(before))
    #expect(reads.first == Commands.settings("fast"))
    #expect(!reads.contains(Commands.settings("model-a")) && !reads.contains(Commands.settings("model-b")))
    // Then the history of each model behind the name, once.
    #expect(reads.contains(Commands.history("text/model-a")))
    #expect(reads.filter { $0 == Commands.history("text/model-a") }.count == 1)
    #expect(store.settings["fast"] != nil && store.histories["text/model-a"] != nil)
    store.setVisible(false)
}

@MainActor @Test func aNewPickStopsTheReadStillRunningForTheLastOne() async throws {
    let first = Commands.settings("image/model-e"), second = Commands.settings("segment/model-g")
    let runner = try GatedRunner(holding: [first, second])
    let store = ControlStore(runner: runner, clock: { Fixture.now }, sleep: { _ in try await Task.sleep(for: .seconds(3600)) },
                             debounce: 0)
    store.setVisible(true)
    try await eventually { store.lastRead != nil && store.sweepTask == nil }
    store.selectedModel = "image/model-e"
    try await runner.waitUntilStarted(first)
    store.selectedModel = "segment/model-g"
    try await runner.waitUntilCancelled(first)
    try await runner.waitUntilStarted(second)
    // While the pick's own read runs, a round of reads does not start a second read of the same model.
    let before = runner.started.count
    await store.tick()
    #expect(runner.started.dropFirst(before).contains(Commands.status.arguments))
    #expect(!runner.started.dropFirst(before).contains(second.arguments))
    #expect(runner.started.filter { $0 == second.arguments }.count == 1)
    runner.release(second)
    await store.selectionTask?.value
    #expect(store.settings["image/model-e"] == nil)
    #expect(store.selectionTask == nil)
    store.setVisible(false)
}

@MainActor @Test func theRoundOfReadsNeverReReadsTheOpenModelsHistory() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now }, sleep: { _ in try await Task.sleep(for: .seconds(3600)) },
                             debounce: 0)
    store.setVisible(true)
    try await eventually { store.lastRead != nil && store.sweepTask == nil }
    store.selectedModel = "model-a"
    try await eventually { store.histories["text/model-a"] != nil }
    await store.selectionTask?.value
    let before = runner.calls.count
    clock.advance(10)
    await store.tick()
    let round = Array(runner.calls.dropFirst(before))
    #expect(round.contains(Commands.settings("model-a")))
    #expect(!round.contains(Commands.history("text/model-a")))
    store.setVisible(false)
}

@MainActor @Test func hidingTheWindowMidReadRecordsNoGatewayError() async throws {
    let runner = try GatedRunner(holding: [Commands.status])
    let store = ControlStore(runner: runner, clock: { Fixture.now }, sleep: { _ in try await Task.sleep(for: .seconds(3600)) })
    store.setVisible(true)
    try await runner.waitUntilStarted(Commands.status)
    store.setVisible(false)
    try await runner.waitUntilCancelled(Commands.status)
    try await Task.sleep(for: .milliseconds(50))
    #expect(store.readError == nil)
    #expect(!store.statusFailed)
    #expect(store.problems.isEmpty)
}
