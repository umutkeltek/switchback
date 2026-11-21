import Foundation
import Testing
@testable import ControlPresentation

/// A clock the test moves by hand.
final class TestClock: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Date
    init(_ start: Date) { value = start }
    var now: Date { lock.withLock { value } }
    func advance(_ seconds: TimeInterval) { lock.withLock { value = value.addingTimeInterval(seconds) } }
}

@MainActor @Test func aRefreshOnlyReadsAndNeverWakes() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    store.selectedModel = "model-a"
    await store.tick()
    await store.sweepTask?.value
    let calls = runner.calls
    #expect(!calls.isEmpty)
    #expect(calls.allSatisfy { !$0.writes }, "a refresh ran a write: \(calls.filter(\.writes).map(\.display))")
    #expect(!calls.contains { $0.arguments.contains("--wake") })
    #expect(calls.contains(Commands.status) && calls.contains(Commands.engines) && calls.contains(Commands.doctor()))
    #expect(calls.contains(Commands.settings("model-a")))
    // The open model's history is read when it is picked and after a write, never every round.
    #expect(!calls.contains(Commands.history("text/model-a")))
    // The first refresh also starts the read of every text and audio name's settings, beside the status reads.
    #expect(calls.contains(Commands.settings("fast")) && calls.contains(Commands.settings("transcribe")))
    #expect(store.machines.map(\.title) == ["GPU box", "Mac"])
    #expect(store.rows.first { $0.name == "fast" }?.isRoute == true)
    #expect(store.readError == nil)
}

@MainActor @Test func slowerReadsWaitTheirTurn() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    await store.tick()
    await store.sweepTask?.value
    let first = runner.calls.count
    clock.advance(10)
    await store.tick()
    let second = Array(runner.calls.dropFirst(first))
    // Ten seconds later: status and engines again, but not the speed report, the model list or every name's settings.
    #expect(second.contains(Commands.status) && second.contains(Commands.engines))
    #expect(!second.contains(Commands.doctor()) && !second.contains(Commands.models) && !second.contains(Commands.settings("fast")))
    clock.advance(30)
    let before = runner.calls.count
    await store.tick()
    #expect(runner.calls.dropFirst(before).contains(Commands.doctor()))
}

@MainActor @Test func readsNeverComeSoonerThanTenSeconds() async throws {
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: try FakeRunner(), clock: { clock.now })
    #expect(store.nextReadDelay() == 0)
    await store.tick()
    #expect(store.nextReadDelay() == 10)
    // Hiding and showing the window again three seconds later does not read early.
    clock.advance(3)
    store.setVisible(true)
    store.setVisible(false)
    #expect(store.nextReadDelay() == 7)
    clock.advance(8)
    #expect(store.nextReadDelay() == 0)
}

@MainActor @Test func hidingTheWindowStopsReading() async throws {
    let runner = try FakeRunner()
    let slept = Slept()
    let store = ControlStore(runner: runner, clock: { Fixture.now }, sleep: { seconds in
        await slept.add(seconds)
        try await Task.sleep(for: .seconds(3600))
    })
    store.setVisible(true)
    // The first read starts at once; then the loop waits its 10 seconds.
    for _ in 0..<200 where await slept.values.isEmpty { try await Task.sleep(for: .milliseconds(5)) }
    #expect(await slept.values == [10])
    await store.sweepTask?.value
    let count = runner.calls.count
    store.setVisible(false)
    try await Task.sleep(for: .milliseconds(50))
    #expect(runner.calls.count == count)
    #expect(!store.visible)
}
actor Slept {
    var values: [TimeInterval] = []
    func add(_ value: TimeInterval) { values.append(value) }
}

@MainActor @Test func anActionWithAQuestionRunsOnlyAfterYes() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    let card = box(power: .asleep)
    let plan = try #require(Machines.keepOn(card))
    #expect(plan.wakes && plan.confirmation != nil)
    #expect(await store.perform(plan) == nil)
    #expect(runner.calls.isEmpty, "a waking action ran without the question")
    runner.answer(plan.command, stdout: "gpu-box is waking; holding it on for 60 min: hold h9\n")
    #expect(await store.perform(plan, confirmed: true) == .done("Waking · kept on for 1 h"))
    #expect(runner.calls.first == plan.command)
    #expect(store.feedback[plan.id] == .done("Waking · kept on for 1 h"))
}

@MainActor @Test func aDisabledActionNeverRuns() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    let plan = try #require(Machines.powerOff(box(uptime: 60)))
    #expect(await store.perform(plan, confirmed: true) == nil)
    #expect(runner.calls.isEmpty)
}

@MainActor @Test func aFailedActionShowsTheReasonSbGave() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    let plan = try #require(Machines.powerOff(box()))
    runner.answer(plan.command, status: 1, stderr:
        "sb ai: the gateway refused to power gpu-box off (guard_busy): the power-off guard reports the machine busy\n")
    let result = await store.perform(plan, confirmed: true)
    #expect(result == .failed("The gateway refused to power gpu-box off (guard_busy): the power-off guard reports the machine busy"))
    // After a power action the status is read again at once.
    #expect(runner.calls.dropFirst().contains(Commands.status))
}

@MainActor @Test func applyRunsEachCommandInOrderAndStopsAtAFailure() async throws {
    let fixture = try Fixture.synthetic
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    store.load(fixture)
    let target = try #require(fixture.target("model-a"))
    let plan = Inspector.plan(target: target, draft: SettingsDraft(values: ["top_p": .set(.number(0.9)),
                                                                            "load.ctx_size": .set(.number(65536))]),
                              engines: fixture.engines, machines: fixture.machines)
    runner.answer(plan.actions[0].command, stdout: "text/model-a: revision 4\n")
    runner.answer(plan.actions[1].command, status: 1, stderr: "sb ai: the engine of lane `text` does not list `model-a`\n")
    let result = await store.apply(plan, id: "apply")
    #expect(result == .failed("The engine of `text` does not list `model-a`"))
    #expect(runner.calls.prefix(2).map(\.arguments) == plan.actions.map(\.command.arguments))
}

@MainActor @Test func tryItShowsTheAnswerOrTheReason() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    let plan = Inspector.tryIt("fast", targets: ["text/model-a"], prompt: "Say hi", machines: try Fixture.synthetic.machines)
    #expect(!plan.wakes)
    runner.answer(plan.command, stdout: #"{"answer":"Hi.","completion_tokens":2,"decode_tok_s":140.0,"route":"text/model-a"}"#)
    await store.send(plan, model: "fast")
    guard case .answered(let answer) = store.chats["fast"] else { Issue.record("no answer"); return }
    #expect(answer.answer == "Hi." && answer.route == "text/model-a" && answer.tokensPerSecond == 140)
    runner.answer(plan.command, status: 1, stderr: "sb ai: asking fast: the gateway said 503: model paused\n")
    await store.send(plan, model: "fast")
    #expect(store.chats["fast"] == .failed("Asking fast: the gateway said 503: model paused"))
}

@MainActor @Test func aGatewayThatDoesNotAnswerKeepsTheLastDataAndSaysWhy() async throws {
    let runner = try FakeRunner()
    let store = ControlStore(runner: runner, clock: { Fixture.now })
    await store.tick()
    runner.answer(Commands.status, status: 3, stderr: "sb ai: the gateway does not answer (connection refused)\n")
    await store.tick()
    #expect(store.readError == "Can’t read the gateway · The gateway does not answer (connection refused)")
    #expect(store.status != nil)
    #expect(store.problems.first?.serious == true)
}

@MainActor @Test func readingNowStillWaitsTenSeconds() async throws {
    let runner = try FakeRunner()
    let clock = TestClock(Fixture.now)
    let store = ControlStore(runner: runner, clock: { clock.now })
    await store.readNow()
    let first = runner.calls.count
    #expect(first > 0)
    clock.advance(4)
    await store.readNow()
    #expect(runner.calls.count == first)
    clock.advance(6)
    await store.readNow()
    #expect(runner.calls.count > first)
}
