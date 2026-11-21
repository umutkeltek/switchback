import Foundation
import Testing
@testable import ControlPresentation

/// A GPU box card with the given state, for the power rules.
func box(power: Power = .on, uptime: Double? = 3600, busy: Int = 0, holds: Int = 0, guardBusy: Bool = false) -> MachineCard {
    let release = ActionPlan(id: "r", title: "Release", command: Commands.release("h", machine: "gpu-box"))
    return MachineCard(machine: "gpu-box", kind: .gpuBox, title: "GPU box", power: power, powerWord: "", powerOffIn: nil,
                       uptime: uptime, gpuUser: nil, busy: busy, guardBusy: guardBusy,
                       holds: (0..<holds).map { HoldCard(id: "h\($0)", machine: "gpu-box", reason: "x", owner: nil, secondsLeft: 60,
                                                          untilText: "", waking: false, wakeFailed: nil, release: release) },
                       loaded: [], problems: [], wakeSeconds: 68, lanes: ["text"])
}

@Test func machinesAreNamedByWhatTheGatewayDoesWithThem() throws {
    let cards = try Fixture.synthetic.machines
    #expect(cards.map(\.title) == ["GPU box", "Mac"])
    #expect(cards.map(\.kind) == [.gpuBox, .mac])
    let gpu = cards[0]
    #expect(gpu.power == .on && gpu.powerWord == "On")
    #expect(gpu.gpuUser == "Text models")
    #expect(gpu.holds.map(\.reason) == ["nightly check"])
    #expect(gpu.holds.first?.untilText.hasSuffix("1 h 12 min left") == true)
    // Held: no countdown.
    #expect(gpu.powerOffIn == nil)
    // Up since the last wake (38 min ago) plus how long that wake took (71 s).
    #expect(gpu.uptime == Double(38 * 60 - 71))
    #expect(gpu.loaded.map(\.name) == ["model-a"])
    #expect(gpu.loaded.first?.memoryBytes ?? 0 > 19 * 1_073_741_824)
    #expect(gpu.problems.isEmpty)
    let mac = cards[1]
    #expect(mac.loaded.map(\.state) == [.resting])
    #expect(mac.uptime == nil)
}

@Test func theMacNeverGetsPowerOrHoldActions() throws {
    let mac = try #require(try Fixture.synthetic.machines.first { $0.kind == .mac })
    #expect(Machines.powerOff(mac) == nil)
    #expect(Machines.keepOn(mac) == nil)
    #expect(mac.holds.isEmpty)
    // Even a hold reported on the Mac's lanes is not offered for release there.
    var json = try Fixture.json("window-synthetic.json")
    if case .object(var root) = json, case .object(var status) = root["status"], case .array(var lanes) = status["lanes"],
       case .object(var first) = lanes[0] {
        first["holds"] = .array([.object(["id": .string("hold_mac"), "reason": .string("x"), "state": .string("active")])])
        lanes[0] = .object(first); status["lanes"] = .array(lanes); root["status"] = .object(status); json = .object(root)
    }
    let held = ControlFixture(json).machines.first { $0.kind == .mac }
    #expect(held?.holds.isEmpty == true)
}

@Test func powerOffIsDisabledUnderFourMinutes() {
    let young = Machines.powerOff(box(uptime: 239))
    #expect(young?.gate == .disabled("Up 3 min · it can power off after 4 min"))
    #expect(Machines.powerOff(box(uptime: 240))?.gate == .allowed)
    #expect(Machines.powerOff(box(uptime: 30))?.gate.reason?.hasPrefix("Up 30 s") == true)
}

@Test func powerOffIsDisabledWhileWorkOrAHoldIsActive() {
    #expect(Machines.powerOff(box(busy: 1))?.gate == .disabled("Working on 1 request"))
    #expect(Machines.powerOff(box(busy: 3))?.gate == .disabled("Working on 3 requests"))
    #expect(Machines.powerOff(box(holds: 1))?.gate == .disabled("A hold keeps it on · release it first"))
    #expect(Machines.powerOff(box(holds: 2))?.gate == .disabled("2 holds keep it on · release them first"))
    // Asleep or already powering off: no button at all (TruthTests).
    #expect(Machines.powerOff(box(power: .asleep)) == nil)
    #expect(Machines.powerOff(box(power: .poweringOff)) == nil)
    #expect(Machines.powerOff(box(power: .waking))?.gate == .disabled("Waking up"))
    #expect(Machines.powerOff(box(guardBusy: true))?.gate == .disabled("Someone else is using it"))
    // Unknown uptime: the window cannot judge it, and the gateway still checks.
    #expect(Machines.powerOff(box(uptime: nil))?.gate == .allowed)
    let plan = Machines.powerOff(box())
    #expect(plan?.command.arguments == ["ai", "off", "--host", "gpu-box"])
    #expect(plan?.confirmation?.destructive == true)
    #expect(plan?.wakes == false)
}

@Test func keepOnHoldsAnAwakeBoxAndWakesASleepingOneWithAQuestion() {
    let awake = Machines.keepOn(box())
    #expect(awake?.title == "Keep on 1 h")
    #expect(awake?.command.arguments == ["ai", "hold", "1h", "kept on from the Switchback window", "--host", "gpu-box"])
    #expect(awake?.wakes == false && awake?.confirmation == nil)
    let asleep = Machines.keepOn(box(power: .asleep), duration: "2h")
    #expect(asleep?.title == "Keep on 2 h · wakes the GPU box")
    #expect(asleep?.command.arguments == ["ai", "on", "2h", "kept on from the Switchback window", "--host", "gpu-box"])
    #expect(asleep?.wakes == true)
    #expect(asleep?.confirmation?.title == "Wake the GPU box?")
    #expect(asleep?.confirmation?.message.contains("about 1 min") == true)
}

@Test func releaseNamesTheHoldAndItsMachine() throws {
    let hold = try #require(try Fixture.synthetic.machines.first?.holds.first)
    #expect(hold.release.command.arguments == ["ai", "release", "hold_0001", "--host", "gpu-box"])
    #expect(hold.release.wakes == false)
}

@Test func theWorstCaseHasAPlainProblemOnEveryMachine() throws {
    let cards = try Fixture.worst.machines
    #expect(cards.map(\.title) == ["GPU box · gpu-box", "GPU box · gpu-box-2", "Mac"])
    for card in cards { #expect(!card.problems.isEmpty, "\(card.title) shows no problem") }
    let first = cards[0]
    #expect(first.problems.contains { $0.text.contains("did not wake") })
    #expect(first.problems.contains { $0.text.contains("failed to load") })
    #expect(first.problems.contains { $0.text.contains("did not reach the engine") })
    #expect(first.busy == 8)
    #expect(Machines.powerOff(first)?.gate.isAllowed == false)
    #expect(cards[1].power == .asleep && cards[1].powerWord == "Asleep")
    #expect(cards[2].powerWord == "Not answering")
    // No problem line uses the gateway's internal words.
    for problem in cards.flatMap(\.problems) {
        for word in ["lane", "target", "revision", "overlay", "pending_apply"] {
            #expect(problem.text.range(of: "\\b\(word)\\b", options: .regularExpression) == nil, "\(problem.text)")
        }
    }
}

@Test func anIdleBoxShowsItsCountdown() throws {
    let json = try JSONValue.parse(Data(#"""
    {"machines":[{"machine":"gpu-box","power":"on"}],
     "lanes":[{"name":"text","host":"gpu-box","state":"idle_countdown","queue_depth":0,"in_flight":0,"holds":[],
               "poweroff_in_secs":540,"wake_configured":true,"poweroff_configured":true}]}
    """#.utf8))
    let card = try #require(Machines.cards(status: AIStatus(json), engines: nil, doctor: nil, now: Fixture.now).first)
    #expect(card.powerOffIn == 540)
    #expect(card.uptime == nil)
}

@Test func uptimeComesFromTheGatewayOrFromTheNewestWake() throws {
    func status(_ lanes: String) throws -> AIStatus {
        AIStatus(try JSONValue.parse(Data(("{\"machines\":[{\"machine\":\"gpu-box\",\"power\":\"on\"}],\"lanes\":[" + lanes + "]}").utf8)))
    }
    let now = Fixture.now.timeIntervalSince1970 * 1000
    func lane(_ name: String, wokeMinutesAgo: Double, upSince: Double? = nil) -> String {
        let up = upSince.map { ",\"up_since_ms\":\(now - $0 * 60_000)" } ?? ""
        return "{\"name\":\"\(name)\",\"host\":\"gpu-box\",\"state\":\"healthy\",\"holds\":[],\"wake_configured\":true," +
            "\"poweroff_configured\":true,\"last_wake_result\":\"ok after 1 attempt(s)\",\"wake_duration_ms\":60000," +
            "\"last_wake_ms\":\(now - wokeMinutesAgo * 60_000)\(up)}"
    }
    func uptime(_ lanes: String) throws -> Double? {
        Machines.cards(status: try status(lanes), engines: nil, doctor: nil, now: Fixture.now).first?.uptime
    }
    // Two lanes woken together: up since the newer wake ended.
    #expect(try uptime(lane("a", wokeMinutesAgo: 30) + "," + lane("b", wokeMinutesAgo: 28)) == 27 * 60)
    // One lane started long after the other: the newest wake stands in, marked approximate. It may be later than
    // the machine's start, never earlier, so Power off waits at least as long as it must.
    #expect(try uptime(lane("a", wokeMinutesAgo: 600) + "," + lane("b", wokeMinutesAgo: 2)) == 60)
    let woken = try #require(Machines.cards(status: try status(lane("a", wokeMinutesAgo: 600) + "," + lane("b", wokeMinutesAgo: 2)),
                                            engines: nil, doctor: nil, now: Fixture.now).first)
    #expect(woken.uptimeApproximate)
    #expect(Machines.uptimeText(woken) == "~1 min")
    #expect(Machines.powerOff(woken)?.gate == .disabled("Up ~1 min · it can power off after 4 min"))
    // Four minutes after the newest wake, Power off is offered again.
    let later = try #require(Machines.cards(status: try status(lane("a", wokeMinutesAgo: 600) + "," + lane("b", wokeMinutesAgo: 6)),
                                            engines: nil, doctor: nil, now: Fixture.now).first)
    #expect(Machines.uptimeText(later) == "~5 min")
    #expect(Machines.powerOff(later)?.gate == .allowed)
    // A gateway that reports when the machine came up is believed first, and is exact.
    #expect(try uptime(lane("a", wokeMinutesAgo: 600, upSince: 3) + "," + lane("b", wokeMinutesAgo: 2)) == 180)
    let young = try #require(Machines.cards(status: try status(lane("a", wokeMinutesAgo: 600, upSince: 3)), engines: nil,
                                            doctor: nil, now: Fixture.now).first)
    #expect(!young.uptimeApproximate && Machines.uptimeText(young) == "3 min")
    #expect(Machines.powerOff(young)?.gate == .disabled("Up 3 min · it can power off after 4 min"))
    // No successful wake and no time from the gateway: unknown, and the gateway's own check decides.
    let unknown = try #require(Machines.cards(status: try status(#"{"name":"a","host":"gpu-box","state":"healthy","holds":[],"wake_configured":true,"last_wake_result":"timeout"}"#),
                                              engines: nil, doctor: nil, now: Fixture.now).first)
    #expect(unknown.uptime == nil && Machines.uptimeText(unknown) == nil)
}

@Test func gpuBoxesComeBeforeTheMac() throws {
    let json = try JSONValue.parse(Data(#"""
    {"machines":[{"machine":"mac","power":"on"},{"machine":"gpu-box","power":"off"}],
     "lanes":[{"name":"mac","host":"mac","state":"healthy","holds":[],"wake_configured":false,"poweroff_configured":false},
              {"name":"text","host":"gpu-box","state":"offline","holds":[],"wake_configured":true,"poweroff_configured":true}]}
    """#.utf8))
    #expect(Machines.cards(status: AIStatus(json), engines: nil, doctor: nil, now: Fixture.now).map(\.title) == ["GPU box", "Mac"])
}
