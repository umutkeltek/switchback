import Foundation
import Testing
@testable import ControlPresentation

// The last review round: a short status never makes a machine a Mac, wake questions default to Cancel, nothing
// reads while hidden, the KV cache says "Engine decides" when it is not known, the footer dot ages like the
// machine line, and the clicks decide from the power as it is at the click.

// MARK: Missing wake fields mean unknown

private var shortView: ControlFixture { get throws { ControlFixture(try Fixture.json("window-short-view.json")) } }

@Test func aShortStatusMakesEveryMachineUnknownNeverAMac() throws {
    let fixture = try shortView
    let machines = fixture.machines
    #expect(fixture.status?.lanes.allSatisfy { $0.wakeConfigured == nil && $0.poweroffConfigured == nil } == true)
    #expect(machines.map(\.kind) == [.unknown, .unknown])
    #expect(machines.map(\.title) == ["Machine · gpu-box", "Machine · mac"])
    #expect(Machines.summary(machines) == "2 machines of unknown kind")
    for machine in machines {
        // The card says it cannot tell, and offers no power, hold or load action.
        #expect(machine.capabilityNote?.hasPrefix("The window can’t tell what this machine can do") == true)
        #expect(Machines.powerOff(machine) == nil)
        #expect(Machines.keepOn(machine) == nil)
        #expect(machine.holds.isEmpty, "a hold to release on a machine the window cannot tell")
    }
    let target = try #require(fixture.target("model-a"))
    #expect(Inspector.loadSettings(target, engines: fixture.engines, machines: machines) == nil)
    let load = Inspector.loadActions(target, engines: fixture.engines, machines: machines)
    #expect(load.load == nil && load.unload == nil)
    let history = try #require(fixture.histories["text/model-a"])
    #expect(Inspector.versions(history, now: fixture.now, machines: machines).allSatisfy { $0.undo == nil })
    // The full status still knows its GPU box and its Mac.
    #expect(try Fixture.synthetic.machines.map(\.kind) == [.gpuBox, .mac])
}

@Test func tryItOnAMachineOfUnknownKindAsksFirst() throws {
    let machines = try shortView.machines
    // The model sits on what is really the GPU box, read "on" a moment ago: the window still cannot tell it sleeps.
    let plan = Inspector.tryIt("model-a", targets: ["text/model-a"], prompt: "hi", machines: machines)
    #expect(plan.wakes && plan.title == "Send · may wake" && plan.spoken == "Send · may wake a machine")
    #expect(plan.confirmation?.title == "This may wake a machine")
    #expect(plan.confirmation?.message.hasPrefix("The window can’t tell what the machine gpu-box can do") == true)
    // Every machine is unknown in the short view, the Mac too.
    #expect(Inspector.tryIt("vision-model", targets: ["mac/vision-model"], prompt: "hi", machines: machines).confirmation != nil)
    #expect(Inspector.tryItNote(targets: ["mac/vision-model"], machines: machines) == nil)
}

@MainActor @Test func aShortStatusSendsNothingFromTryItUntilTheOwnerSaysYes() async throws {
    let runner = try FakeRunner(fixture: "window-short-view.json")
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    store.visible = true
    await store.tick()
    await store.sweepTask?.value
    let row = try #require(store.rows.first { $0.name == "model-a" })
    guard case .ask(let question) = await store.clickSend(row, prompt: "hi") else {
        Issue.record("Try it on a machine of unknown kind sent without asking"); return
    }
    #expect(question.confirmation?.title == "This may wake a machine")
    #expect(!runner.calls.contains { $0.writes }, "a write ran: \(runner.calls.filter(\.writes).map(\.display))")
}

// MARK: Wake questions default to Cancel

@Test func everyQuestionBeforeAWakeDefaultsToCancel() throws {
    let fixture = try Fixture.synthetic
    let sleeping = asleep(fixture.machines)
    let box = try #require(sleeping.first { $0.kind == .gpuBox })
    let target = try #require(fixture.target("model-b"))
    var questions: [ControlPresentation.Confirmation?] = [
        Inspector.tryIt("model-a", targets: ["text/model-a"], prompt: "hi", machines: sleeping).confirmation,
        Inspector.tryIt("vision", targets: ["mac/vision-model", "text/model-a"], prompt: "hi", machines: sleeping).confirmation,
        Inspector.tryIt("default", targets: [], targetsKnown: false, prompt: "hi", machines: fixture.machines).confirmation,
        Inspector.tryIt("model-a", targets: ["text/model-a"], prompt: "hi", machines: try shortView.machines).confirmation,
        Machines.keepOn(box)?.confirmation,
        Inspector.loadActions(target, engines: fixture.engines, machines: sleeping).load?.confirmation,
    ]
    // Keep on with a doubt about the power.
    let on = try #require(fixture.machines.first { $0.kind == .gpuBox })
    let doubted = Machines.cards(status: fixture.status, engines: fixture.engines, doctor: fixture.doctor, now: fixture.now,
                                 evidence: PowerEvidence(readAt: fixture.now.addingTimeInterval(-40)))
    questions.append(Machines.keepOn(try #require(doubted.first { $0.machine == on.machine }))?.confirmation)
    #expect(questions.count == 7 && questions.allSatisfy { $0 != nil }, "a wake asked no question")
    #expect(questions.allSatisfy { $0?.cancelIsDefault == true }, "a wake question would say yes on Return")
    // Power off asks the same way.
    #expect(Machines.powerOff(on)?.confirmation?.cancelIsDefault == true)
}

// MARK: No hidden reads

@MainActor @Test func hidingAfterTheStatusReadRunsNoSettingsRead() async throws {
    // The speed report is held, so the round is still running when the window hides.
    let runner = try GatedRunner(holding: [Commands.doctor()])
    let store = ControlStore(runner: runner, clock: { Fixture.now }, sleep: { _ in try await Task.sleep(for: .seconds(3600)) })
    store.setVisible(true)
    try await runner.waitUntilStarted(Commands.status)
    try await runner.waitUntilStarted(Commands.doctor())
    store.setVisible(false)
    try await runner.waitUntilCancelled(Commands.doctor())
    try await eventually { !store.reading }
    await store.sweepTask?.value
    // The status that was read is kept; nothing more started.
    #expect(store.status != nil)
    #expect(store.sweepTask == nil)
    let settingsReads = runner.started.filter { $0.dropFirst().first == "settings" }
    #expect(settingsReads.isEmpty, "settings read while hidden: \(settingsReads)")
}

@MainActor @Test func aRoundRunByHandWhileHiddenStartsNoSweep() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    await store.tick()
    #expect(store.sweepTask == nil)
    #expect(!runner.calls.contains { $0.arguments.dropFirst().first == "settings" })
}

// MARK: KV cache

@Test func theKVCacheSaysEngineDecidesUnlessTheEngineNamesIt() throws {
    let fixture = try Fixture.synthetic
    // model-b's engine starts with no cache type: the engine decides, and the window shows no value.
    let plain = try #require(fixture.target("model-b"))
    let rows = try #require(Inspector.loadSettings(plain, engines: fixture.engines, machines: fixture.machines))
    let cache = try #require(rows.first { $0.key == "load.cache_type_k" })
    #expect(cache.value == nil && cache.shown == nil)
    let face = Inspector.face(cache, draft: SettingsDraft())
    #expect(face.text == "Engine decides" && face.value == nil)
    // model-a's engine names q8_0, and it is saved too: a concrete value.
    let named = try #require(fixture.target("model-a"))
    let namedRows = try #require(Inspector.loadSettings(named, engines: fixture.engines, machines: fixture.machines))
    let namedCache = try #require(namedRows.first { $0.key == "load.cache_type_k" })
    #expect(namedCache.shown == .string("q8_0"))
    #expect(Inspector.face(namedCache, draft: SettingsDraft()).text == "q8_0")
}

// MARK: Footer dot

@MainActor @Test func theFooterDotAgesLikeTheMachineLine() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    #expect(store.readingFreshness == .old, "nothing read yet")
    await store.tick()
    #expect(store.readingFreshness == .fresh)
    clock.advance(15)
    #expect(store.readingFreshness == .fresh)
    #expect(!Machines.statusLine(try #require(store.machines.first)).muted)
    clock.advance(1)
    #expect(store.readingFreshness == .old)
    #expect(Machines.statusLine(try #require(store.machines.first)).muted, "the dot and the machine line disagree")
    runner.answer(Commands.status, status: 3, stderr: "sb ai: gateway does not answer\n")
    clock.advance(10)
    await store.tick()
    #expect(store.readingFreshness == .failed)
}

// MARK: The check made at the click

@MainActor @Test func tryItDecidesAtTheClickNotFromThePlanItWasDrawnWith() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    store.visible = true
    await store.tick()
    await store.sweepTask?.value
    let row = try #require(store.rows.first { $0.name == "model-a" })
    // Drawn while the power is fresh: a plain Send.
    let drawn = store.tryIt(row, prompt: "hi")
    #expect(drawn.confirmation == nil && !drawn.wakes)
    // Clicked 20 s later, with no read since: the box may have gone to sleep, so the click asks.
    clock.advance(20)
    guard case .ask(let question) = await store.clickSend(row, prompt: "hi") else {
        Issue.record("the click sent with the plan drawn before the power went stale"); return
    }
    #expect(question.wakes && question.title == "Send · may wake")
    #expect(!runner.calls.contains { $0.arguments.dropFirst().first == "chat" })
    // The other way round: drawn stale, clicked after a fresh read, it sends at once.
    let staleDrawn = store.tryIt(row, prompt: "hi")
    #expect(staleDrawn.confirmation != nil)
    await store.tick()
    runner.answer(staleDrawn.command, stdout: #"{"answer":"Hi.","completion_tokens":2}"#)
    #expect(await store.clickSend(row, prompt: "hi") == .ran)
    #expect(runner.calls.contains(staleDrawn.command))
    // A click while the answer runs, or with no prompt, does nothing.
    #expect(await store.clickSend(row, prompt: "  ") == .refused)
}

@MainActor @Test func keepOnDecidesAtTheClickNotFromThePlanItWasDrawnWith() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    await store.tick()
    // Drawn fresh: a plain hold.
    let drawn = try #require(store.keepOn("gpu-box"))
    #expect(drawn.command.arguments.prefix(2) == ["ai", "hold"] && drawn.confirmation == nil)
    // Clicked 20 s later: it asks, and keeps the box on with a wake, not a hold.
    clock.advance(20)
    guard case .ask(let question) = await store.clickKeepOn("gpu-box") else {
        Issue.record("the click held the box with the plan drawn before the power went stale"); return
    }
    #expect(question.wakes && question.command.arguments.prefix(2) == ["ai", "on"])
    #expect(!runner.calls.contains { $0.writes }, "a write ran: \(runner.calls.filter(\.writes).map(\.display))")
    // Drawn stale, clicked after a fresh read: the plain hold runs without a question.
    let staleDrawn = try #require(store.keepOn("gpu-box"))
    #expect(staleDrawn.confirmation != nil)
    await store.tick()
    #expect(await store.clickKeepOn("gpu-box") == .ran)
    #expect(runner.calls.contains { $0.arguments.prefix(2) == ["ai", "hold"] })
    #expect(!runner.calls.contains { $0.arguments.prefix(2) == ["ai", "on"] })
}
