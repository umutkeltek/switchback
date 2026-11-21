import Foundation
import Testing
@testable import ControlPresentation

// "Try it" always reaches the gateway, which wakes a sleeping GPU box for it. These rules decide when the window
// asks first: any model behind the name on a GPU box not known to be on, unknown models, or a power reading that
// is old, failed, not re-read after the window was hidden, or about to change with the idle power-off.

@MainActor @Test func tryItThatMayWakeSendsNothingUntilTheOwnerSaysYes() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    let plan = Inspector.tryIt("model-b", targets: ["text/model-b"], prompt: "hi", machines: asleep(try Fixture.synthetic.machines))
    #expect(plan.wakes && plan.confirmation != nil)
    runner.answer(plan.command, stdout: #"{"answer":"Hi.","completion_tokens":2}"#)
    await store.send(plan, model: "model-b")
    #expect(runner.calls.isEmpty, "Try it ran without the question: \(runner.calls.map(\.display))")
    #expect(store.chats["model-b"] == nil)
    await store.send(plan, model: "model-b", confirmed: true)
    #expect(runner.calls == [plan.command])
    guard case .answered(let answer) = store.chats["model-b"] else { Issue.record("no answer"); return }
    #expect(answer.answer == "Hi.")
}

@Test func tryItAsksWhenAnyModelBehindTheNameIsOnAGPUBoxNotKnownToBeOn() throws {
    let fixture = try Fixture.synthetic
    let sleeping = asleep(fixture.machines)
    // A route whose first model is on the Mac and whose second is on the sleeping GPU box may wake it.
    let route = Inspector.tryIt("vision", targets: ["mac/vision-model", "text/model-a"], prompt: "hi", machines: sleeping)
    #expect(route.wakes)
    #expect(route.title == "Send · may wake the GPU box")
    #expect(route.confirmation?.title == "This may wake the GPU box")
    #expect(route.confirmation?.message.hasPrefix("vision can answer from a model on the GPU box, which is asleep.") == true)
    #expect(route.confirmation?.message.contains("Waking takes about 1 min.") == true)
    #expect(route.confirmation?.confirm == "Send")
    // Every model behind the name on the sleeping box: it surely wakes it.
    let sure = Inspector.tryIt("model-a", targets: ["text/model-a"], prompt: "hi", machines: sleeping)
    #expect(sure.title == "Send · wakes the GPU box" && sure.confirmation?.title == "Wake the GPU box?")
    // A name whose models are not known yet (a route before its settings are read) asks too, even with the box on.
    let unknown = Inspector.tryIt("default", targets: [], targetsKnown: false, prompt: "hi", machines: fixture.machines)
    #expect(unknown.title == "Send · may wake the GPU box")
    #expect(unknown.confirmation?.message.hasPrefix("The window does not know yet which models default uses.") == true)
    #expect(Inspector.tryIt("model-a", targets: ["text/model-a"], targetsKnown: false, prompt: "hi",
                            machines: fixture.machines).confirmation != nil)
    // Nothing read yet: no machines, so nothing is known.
    #expect(Inspector.tryIt("model-a", targets: ["text/model-a"], prompt: "hi", machines: []).confirmation != nil)
    // Everything on the Mac, or on a box known to be on: no question.
    #expect(Inspector.tryIt("vision-model", targets: ["mac/vision-model"], prompt: "hi", machines: fixture.machines).confirmation == nil)
    #expect(Inspector.tryIt("vision-model", targets: ["mac/vision-model"], prompt: "hi", machines: sleeping).confirmation == nil)
    let on = Inspector.tryIt("fast", targets: ["text/model-a", "mac/vision-model"], prompt: "hi", machines: fixture.machines)
    #expect(on.confirmation == nil && on.title == "Send" && !on.wakes)
}

@MainActor @Test func tryItForANameNotReadYetAsksThroughTheStore() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    store.load(try Fixture.synthetic)
    let fresh = ControlStore(runner: runner, clock: { Fixture.now })
    fresh.load(ControlFixture(try Fixture.json("window-synthetic.json").dropping("settings")))
    // Read: "default" picks between a model on the GPU box (on) and one on the Mac.
    let known = try #require(store.rows.first { $0.name == "default" })
    #expect(store.tryIt(known, prompt: "hi").confirmation == nil)
    // Not read: the window cannot know where "default" runs.
    let guessed = try #require(fresh.rows.first { $0.name == "default" })
    #expect(!guessed.known)
    #expect(fresh.tryIt(guessed, prompt: "hi").confirmation != nil)
}

@Test func tryItOnTheMacSaysItLoadsARestingModel() throws {
    let fixture = try Fixture.synthetic
    #expect(Inspector.tryItNote(targets: ["mac/vision-model"], machines: fixture.machines) == "Loads it on the Mac if it is resting")
    #expect(Inspector.tryItNote(targets: ["text/model-a", "mac/vision-model"], machines: fixture.machines) != nil)
    #expect(Inspector.tryItNote(targets: ["text/model-a"], machines: fixture.machines) == nil)
}

@MainActor @Test func powerReadMoreThanFifteenSecondsAgoIsUnknownForAWake() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    await store.tick()
    await store.sweepTask?.value
    let row = try #require(store.rows.first { $0.name == "model-a" })
    #expect(store.machines.first?.knownOn == true)
    #expect(store.tryIt(row, prompt: "hi").confirmation == nil)
    #expect(store.keepOn("gpu-box")?.command.arguments.prefix(2) == ["ai", "hold"])
    clock.advance(15)
    #expect(store.machines.first?.powerDoubt == nil)
    clock.advance(1)
    let box = try #require(store.machines.first { $0.kind == .gpuBox })
    #expect(box.powerDoubt == "it was last read 16 s ago")
    #expect(!box.knownOn)
    let stale = store.tryIt(row, prompt: "hi")
    #expect(stale.title == "Send · may wake the GPU box")
    #expect(stale.confirmation?.message.hasPrefix("The window can’t be sure the GPU box is on: it was last read 16 s ago.") == true)
    // Keep on can no longer trust "on": it asks, and keeps the box on with a wake in case it went to sleep.
    let keep = try #require(store.keepOn("gpu-box"))
    #expect(keep.title == "Keep on 1 h · may wake the GPU box")
    #expect(keep.command.arguments.prefix(2) == ["ai", "on"])
    #expect(keep.confirmation?.title == "This may wake the GPU box")
    #expect(await store.perform(keep) == nil, "a Keep on that may wake ran without the question")
    // A fresh read makes it known again.
    await store.tick()
    #expect(store.machines.first?.knownOn == true)
    #expect(store.tryIt(row, prompt: "hi").confirmation == nil)
}

@MainActor @Test func aFailedReadMakesThePowerUnknownUntilAReadSucceeds() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    await store.tick()
    await store.sweepTask?.value
    let row = try #require(store.rows.first { $0.name == "model-a" })
    runner.answer(Commands.status, status: 3, stderr: "sb ai: the gateway does not answer (connection refused)\n")
    await store.tick()
    #expect(store.machines.first?.powerDoubt == "the last reading failed")
    #expect(store.tryIt(row, prompt: "hi").confirmation != nil)
    runner.answer(Commands.status, stdout: try syntheticStatus(power: "on"))
    await store.tick()
    #expect(store.machines.first?.powerDoubt == nil)
    #expect(store.tryIt(row, prompt: "hi").confirmation == nil)
}

@MainActor @Test func aWindowShownAgainDoesNotTrustThePowerUntilItReadsIt() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now }, sleep: { _ in try await Task.sleep(for: .seconds(3600)) })
    await store.tick()
    await store.sweepTask?.value
    let row = try #require(store.rows.first { $0.name == "model-a" })
    #expect(store.tryIt(row, prompt: "hi").confirmation == nil)
    // Shown again three seconds later: the next read waits its turn, and until it lands the power is unknown.
    clock.advance(3)
    store.setVisible(true)
    #expect(store.awaitingFresh)
    #expect(store.machines.first?.powerDoubt == "the window is reading it again")
    #expect(store.tryIt(row, prompt: "hi").confirmation != nil)
    await store.tick()
    #expect(!store.awaitingFresh)
    #expect(store.tryIt(row, prompt: "hi").confirmation == nil)
    store.setVisible(false)
}

@Test func anIdlePowerOffUnderThirtySecondsAwayMakesThePowerUnknown() throws {
    func card(countdown: Double, readSecondsAgo: Double = 0) throws -> MachineCard {
        let json = try JSONValue.parse(Data("""
        {"machines":[{"machine":"gpu-box","power":"on"}],
         "lanes":[{"name":"text","host":"gpu-box","state":"idle_countdown","queue_depth":0,"in_flight":0,"holds":[],
                   "poweroff_in_secs":\(countdown),"wake_configured":true,"poweroff_configured":true}]}
        """.utf8))
        let evidence = PowerEvidence(readAt: Fixture.now.addingTimeInterval(-readSecondsAgo))
        return try #require(Machines.cards(status: AIStatus(json), engines: nil, doctor: nil, now: Fixture.now, evidence: evidence).first)
    }
    #expect(try card(countdown: 40).powerDoubt == nil)
    let soon = try card(countdown: 25)
    #expect(soon.powerDoubt == "it powers off in 25 s" && !soon.knownOn)
    #expect(Inspector.tryIt("model-a", targets: ["text/model-a"], prompt: "hi", machines: [soon]).confirmation != nil)
    #expect(Machines.keepOn(soon)?.command.arguments.prefix(2) == ["ai", "on"])
    // The countdown keeps running after it was read: 40 s read 12 s ago leaves 28 s.
    let aged = try card(countdown: 40, readSecondsAgo: 12)
    #expect(aged.powerOffIn == 28)
    #expect(aged.powerDoubt == "it powers off in 28 s")
}

@MainActor @Test func theSettingsSweepNeverHoldsUpTheStatusReads() async throws {
    let runner = try GatedRunner(holding: [Commands.settings("fast")])
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    // The first round returns although the five-minute sweep is still waiting on one name.
    await store.tick()
    #expect(store.status != nil)
    try await runner.waitUntilStarted(Commands.settings("fast"))
    #expect(store.sweepTask != nil)
    clock.advance(10)
    let before = runner.started.count
    await store.tick()
    #expect(runner.started.dropFirst(before).contains(Commands.status.arguments))
    #expect(store.statusReadAt == clock.now)
    #expect(store.machines.first?.knownOn == true)
    runner.release(Commands.settings("fast"))
    await store.sweepTask?.value
    #expect(store.settings["fast"] != nil)
    #expect(store.sweepTask == nil)
}

@MainActor @Test func enginesAreReadOnceAMinuteWhileEveryGPUBoxSleeps() async throws {
    let runner = try FakeRunner()
    runner.answer(Commands.status, stdout: try syntheticStatus(power: "off"))
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    func round() async -> Bool {
        let before = runner.calls.count
        await store.tick()
        await store.sweepTask?.value
        return runner.calls.dropFirst(before).contains(Commands.engines)
    }
    #expect(await round())
    #expect(store.machines.first { $0.kind == .gpuBox }?.power == .asleep)
    clock.advance(10)
    #expect(!(await round()), "the engines were read again 10 s later while every GPU box sleeps")
    clock.advance(50)
    #expect(await round())
    // A box on again: every round.
    runner.answer(Commands.status, stdout: try syntheticStatus(power: "on"))
    clock.advance(10)
    _ = await round()
    clock.advance(10)
    #expect(await round())
}

extension JSONValue {
    /// The object without one key.
    func dropping(_ key: String) -> JSONValue {
        guard case .object(var fields) = self else { return self }
        fields[key] = nil
        return .object(fields)
    }
}
