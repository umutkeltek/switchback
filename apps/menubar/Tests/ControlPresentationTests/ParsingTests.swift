import Foundation
import Testing
@testable import ControlPresentation

@Test func statusReadsMachinesLanesAndHolds() throws {
    let status = try #require(try Fixture.synthetic.status)
    #expect(status.power.map(\.machine) == ["gpu-box", "mac"])
    #expect(status.power.map(\.power) == ["on", "on"])
    #expect(status.lanes.map(\.name) == ["mac", "mac-audio", "media", "text"])
    let text = try #require(status.lanes.first { $0.name == "text" })
    #expect(text.machine == "gpu-box")
    #expect(text.holdsGPU == true)
    #expect(text.wakeConfigured == true && text.poweroffConfigured == true)
    #expect(text.lastWakeResult == "ok after 1 attempt(s)")
    #expect(text.holds.map(\.id) == ["hold_0001"])
    #expect(text.holds.first?.reason == "nightly check")
    #expect(text.holds.first?.expiresAtMs == Fixture.now.timeIntervalSince1970 * 1000 + 72 * 60_000)
    let mac = try #require(status.lanes.first { $0.name == "mac" })
    #expect(mac.wakeConfigured == false && mac.poweroffConfigured == false)
    #expect(mac.poweroffInSecs == nil)
}

@Test func clientViewWithoutPowerFieldsStillReads() throws {
    // A client key sees a shorter lane: no wake or power-off fields, no errors.
    let json = try JSONValue.parse(Data(#"""
    {"machines":[{"machine":"gpu-box","power":"off"}],
     "lanes":[{"name":"text","host":"gpu-box","state":"offline","level":"ok","queue_depth":0,"in_flight":0,
               "slots":{"in_use":0,"max_in_flight":1},"poweroff_in_secs":null,"holds":[]}]}
    """#.utf8))
    let status = AIStatus(json)
    #expect(status.lanes.first?.wakeConfigured == nil)
    #expect(Machines.cards(status: status, engines: nil, doctor: nil, now: Fixture.now).first?.kind == .mac)
}

@Test func modelsReadEveryJob() throws {
    let catalog = try #require(try Fixture.synthetic.catalog)
    #expect(catalog[.text] == ["default", "fast", "model-a", "model-b", "model-c", "model-d", "vision-model"])
    #expect(catalog[.image] == ["image/model-e"])
    #expect(catalog[.video] == ["video/model-f"])
    #expect(catalog[.segment] == ["segment/model-g"])
    #expect(catalog[.audio] == ["transcribe"])
}

@Test func enginesReadModelsMemoryAndArguments() throws {
    let engines = try #require(try Fixture.synthetic.engines)
    let text = try #require(engines.lane("text"))
    #expect(text.acceptsControl)
    #expect(engines.lane("mac")?.acceptsControl == false)
    let model = try #require(engines.model("text/model-a")?.model)
    #expect(model.status == "loaded")
    #expect(model.argument(["--ctx-size", "-c"]) == "32768")
    #expect(model.argument(["--cache-type-k", "-ctk"]) == "q8_0")
    #expect(model.argument(["--parallel", "-np"]) == "2")
    #expect(model.memory?.effective?.context == 32768)
    #expect((model.memory?.inUse?.bytes ?? 0) > 19 * 1_073_741_824)
    // `failed` is an object with a reason in the worst case: it still counts.
    let worst = try #require(try Fixture.worst.engines)
    #expect(worst.lanes.first?.models.filter(\.failed).count == 1)
    #expect(worst.lane("text")?.lastApplyError == "settings command exited 1: the preset file is read-only")
}

@Test func settingsReadSourcesLocksLoadAndEstimate() throws {
    let target = try #require(try Fixture.synthetic.target("model-a"))
    #expect(target.target == "text/model-a")
    #expect(target.revision == 3)
    #expect(target.effective["temperature"] == EffectiveSetting(value: .number(0.6), source: "defaults", locked: true))
    #expect(target.effective["max_tokens"] == EffectiveSetting(value: .null, source: "engine"))
    #expect(target.loadSettings["ctx_size"]?.value == .number(32768))
    #expect(target.loadSettings["cache_type_k"]?.value == .string("q8_0"))
    #expect(target.engineLane == "text" && target.engineStatus == "loaded")
    #expect(target.estimate?.budgetBytes == Double(Int(23.5 * 1_073_741_824)))
    #expect(target.bounds["ctx_size"] == 2048...131072)
    #expect(target.hasSaved)
    let fast = try #require(try Fixture.synthetic.settings["fast"])
    #expect(fast.isRoute)
    #expect(fast.targets.map(\.target) == ["text/model-a", "mac/vision-model"])
    #expect(try Fixture.synthetic.target("model-b")?.hasSaved == false)
}

@Test func historyReadsNewestFirst() throws {
    let history = try #require(try Fixture.synthetic.histories["text/model-a"])
    #expect(history.revision == 3)
    #expect(history.entries.map(\.revision) == [3, 2, 1])
    #expect(history.entries.first?.changed == ["load.cache_type_k"])
}

@Test func doctorReadsSpeedCeilingAndFindings() throws {
    let doctor = try #require(try Fixture.synthetic.doctor)
    let speed = try #require(doctor.speed("text/model-a"))
    #expect(speed.decodeTPS == 142.5)
    #expect(speed.ceilingTPS == 168)
    #expect(speed.percentOfCeiling == 84.8)
    #expect(doctor.speed("text/model-b")?.ceilingTPS == nil)
    #expect(doctor.findings.map(\.kind) == ["empty_after_thinking"])
}

@Test func chatAnswerReadsSpeedAndRoute() throws {
    // The shape `sb ai chat MODEL "prompt" --json` prints.
    let answer = ChatAnswer(try JSONValue.parse(Data(#"""
    {"model":"fast","answer":"Hello.","reasoning":null,"prompt_tokens":12,"completion_tokens":3,"ttft_ms":180,
     "decode_tok_s":141.2,"total_s":0.41,"route":"text/model-a","settings":"temperature=0.6"}
    """#.utf8)))
    #expect(answer.answer == "Hello.")
    #expect(answer.tokensPerSecond == 141.2)
    #expect(answer.route == "text/model-a")
    #expect(answer.thinking == nil)
}

@Test func emptyAndBrokenAnswersDoNotCrash() throws {
    let empty = ControlFixture(try Fixture.json("window-empty.json"))
    #expect(empty.status == nil && empty.engines == nil && empty.catalog == nil)
    #expect(Machines.cards(status: nil, engines: nil, doctor: nil, now: empty.now).isEmpty)
    #expect(ModelList.rows(catalog: nil, engines: nil, doctor: nil, machines: [], settings: [:]).isEmpty)
    // Wrong types are skipped, not fatal.
    let odd = AIStatus(try JSONValue.parse(Data(#"{"machines":"x","lanes":[{"name":7,"in_flight":"two"}]}"#.utf8)))
    #expect(odd.power.isEmpty)
    #expect(odd.lanes.first?.inFlight == 0)
    #expect((try? JSONValue.parse(Data("not json".utf8))) == nil)
}
