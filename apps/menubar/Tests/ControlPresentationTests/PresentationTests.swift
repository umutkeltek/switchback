import Foundation
import Testing
@testable import ControlPresentation

// What the window shows: the Mac stays show-only, "nothing saved" is shown as the engine deciding (never a made-up
// value), loaded models are counted once, and buttons read well with VoiceOver.

@Test func theMacNeverOffersUndoToHere() throws {
    let fixture = try Fixture.synthetic
    let json = try JSONValue.parse(Data(#"""
    {"target":"mac/vision-model","revision":3,"history":[
      {"revision":3,"at_ms":1.7684784e12,"action":"put","changed":["load.ctx_size"]},
      {"revision":2,"at_ms":1.7684712e12,"action":"put","changed":["temperature"]},
      {"revision":1,"at_ms":1.768464e12,"action":"put","changed":[]}]}
    """#.utf8))
    let mac = Inspector.versions(SettingsHistory(json), now: Fixture.now, machines: fixture.machines)
    #expect(mac.map(\.title) == ["Version 3", "Version 2", "Version 1"])
    #expect(mac.allSatisfy { $0.undo == nil }, "the Mac offered Undo to here: \(mac.compactMap(\.undo).map(\.command.display))")
    // Before the machines are read the window cannot tell, so it offers no undo either.
    let gpu = try #require(fixture.histories["text/model-a"])
    #expect(Inspector.versions(gpu, now: Fixture.now, machines: []).allSatisfy { $0.undo == nil })
    // The GPU box keeps it, and VoiceOver hears which version.
    let rows = Inspector.versions(gpu, now: Fixture.now, machines: fixture.machines)
    #expect(rows[2].undo?.command.arguments == ["ai", "revert", "text/model-a", "1"])
    #expect(rows[2].undo?.title == "Undo to here" && rows[2].undo?.spoken == "Undo to version 1")
}

@Test func releaseSaysWhichHoldItLets() throws {
    let hold = try #require(try Fixture.synthetic.machines.first?.holds.first)
    #expect(hold.release.title == "Release")
    #expect(hold.release.spoken == "Release the hold: nightly check")
    // Without a spoken name, VoiceOver reads the title.
    #expect(Machines.powerOff(box())?.spoken == "Power off")
}

@Test func nothingSavedShowsTheEngineDecidingNeverAMadeUpValue() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-b"))
    let rows = Dictionary(uniqueKeysWithValues: Inspector.answerSettings(target).map { ($0.key, $0) })
    // A slider with nothing saved and the engine's value unknown: no handle, no position.
    let temperature = try #require(rows["temperature"])
    let neutral = Inspector.face(temperature, draft: SettingsDraft())
    #expect(neutral.value == nil && neutral.neutral && neutral.source == .unknown && neutral.text == "Engine decides")
    // Dragging starts a value: an edit waiting for Apply.
    let dragged = Inspector.face(temperature, draft: SettingsDraft(values: ["temperature": .set(.number(0.4))]))
    #expect(dragged.value == .number(0.4) && dragged.source == .draft && !dragged.neutral)
    // With the engine's start arguments the window knows its own values and shows them as the engine's.
    let engine = EngineModel(id: "model-b", target: "text/model-b", status: "unloaded",
                             arguments: ["--temperature", "0.8", "--top-p", "0.9", "--n-predict", "-1"])
    let known = Dictionary(uniqueKeysWithValues: Inspector.answerSettings(target, engineModel: engine).map { ($0.key, $0) })
    #expect(known["temperature"]?.value == nil && known["temperature"]?.shown == .number(0.8))
    #expect(Inspector.face(try #require(known["temperature"]), draft: SettingsDraft())
            == SettingFace(value: .number(0.8), source: .engine, text: "0.8"))
    #expect(known["top_p"]?.shown == .number(0.9))
    // -1 tokens means "until the context is full": no number to show.
    #expect(known["max_tokens"]?.shown == nil)
    // The preview names the engine's value it replaces.
    let plan = Inspector.plan(target: target, draft: SettingsDraft(values: ["temperature": .set(.number(0.4))]),
                              engines: EnginesReport(lanes: [EngineLane(lane: "text", models: [engine], acceptsControl: true)]),
                              machines: fixture.machines)
    #expect(plan.lines.first?.from == "engine default (0.8)")
}

@Test func thinkingIsEngineOnOrOffAndNeverOffForEngineDecides() throws {
    let fixture = try Fixture.synthetic
    // The Mac model is marked as one that thinks, but nothing decides `reasoning.enabled`: the engine does.
    let vision = try #require(fixture.target("vision-model"))
    let thinking = try #require(Inspector.answerSettings(vision).first { $0.key == "reasoning.enabled" })
    #expect(thinking.value == nil && thinking.shown == nil)
    #expect(Inspector.thinking(thinking, draft: SettingsDraft()) == .engine)
    #expect(ThinkingChoice.allCases.map(\.title) == ["Engine", "On", "Off"])
    var draft = SettingsDraft()
    Inspector.choose(.off, for: thinking, in: &draft)
    #expect(draft.values["reasoning.enabled"] == .set(.bool(false)))
    #expect(Inspector.thinking(thinking, draft: draft) == .off)
    Inspector.choose(.engine, for: thinking, in: &draft)
    #expect(draft.isEmpty)
    // Saved on: "Engine" removes it; "On" again is no change.
    let saved = SettingState(key: "reasoning.enabled", title: "Thinking", value: .bool(true), shown: nil, badge: .saved, gate: .allowed)
    #expect(Inspector.thinking(saved, draft: SettingsDraft()) == .on)
    Inspector.choose(.engine, for: saved, in: &draft)
    #expect(draft.values["reasoning.enabled"] == .reset)
    #expect(Inspector.thinking(saved, draft: draft) == .engine)
    Inspector.choose(.on, for: saved, in: &draft)
    #expect(draft.isEmpty)
}

@Test func steppersShowAutoUntilAValueAndStartSensibly() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-b"))
    let maxTokens = try #require(Inspector.answerSettings(target).first { $0.key == "max_tokens" })
    let steps: [Double] = [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072]
    #expect(Inspector.face(maxTokens, draft: SettingsDraft(), neutral: "auto").text == "auto")
    // The first press shows a sensible start, either way; it never steps from a hidden middle value.
    #expect(Inspector.step(maxTokens, draft: SettingsDraft(), steps: steps, range: Inspector.range("max_tokens"), up: true) == 4096)
    #expect(Inspector.step(maxTokens, draft: SettingsDraft(), steps: steps, range: Inspector.range("max_tokens"), up: false) == 4096)
    let started = SettingsDraft(values: ["max_tokens": .set(.number(4096))])
    #expect(Inspector.step(maxTokens, draft: started, steps: steps, range: Inspector.range("max_tokens"), up: true) == 8192)
    // Parallel slots: the engine's own value (from its start arguments) is where steps begin.
    let modelA = try #require(fixture.target("model-a"))
    let parallel = try #require(Inspector.loadSettings(modelA, engines: fixture.engines, machines: fixture.machines)?.last)
    #expect(parallel.key == "load.parallel" && parallel.value == nil && parallel.shown == .number(2))
    #expect(Inspector.face(parallel, draft: SettingsDraft(), neutral: "auto").source == .engine)
    #expect(Inspector.step(parallel, draft: SettingsDraft(), steps: (1...16).map(Double.init), range: Inspector.range("load.parallel"),
                           up: true) == 3)
    // Nothing known: one at once to start.
    let unknown = SettingState(key: "load.parallel", title: "Parallel slots", value: nil, shown: nil, badge: .engine, gate: .allowed)
    #expect(Inspector.step(unknown, draft: SettingsDraft(), steps: (1...16).map(Double.init), range: Inspector.range("load.parallel"),
                           up: true) == 1)
}

@Test func contextSizeHasNoHandleOrVerdictUntilASizeIsKnown() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-b"))
    let steps: [Double] = [2048, 4096, 8192, 16384, 32768, 65536, 131072]
    // An engine that does not say its context size: nothing to show yet.
    let silent = EnginesReport(lanes: (fixture.engines?.lanes ?? []).map { lane in
        EngineLane(lane: lane.lane, kind: lane.kind, state: lane.state, error: lane.error,
                   models: lane.models.map { EngineModel(id: $0.id, target: $0.target, status: $0.status) },
                   acceptsControl: lane.acceptsControl)
    })
    let rows = try #require(Inspector.loadSettings(target, engines: silent, machines: fixture.machines))
    let context = try #require(rows.first { $0.key == "load.ctx_size" })
    let empty = Inspector.contextFace(context, cache: rows[1], draft: SettingsDraft(), steps: steps, target: target, engineModel: nil)
    #expect(empty.size == nil && empty.index == nil && empty.fit == nil && empty.text == "Engine decides")
    // The engine's start arguments carry its size (`--ctx-size 16384`): the handle sits there, with a verdict.
    let known = try #require(Inspector.loadSettings(target, engines: fixture.engines, machines: fixture.machines))
    let face = Inspector.contextFace(known[0], cache: known[1], draft: SettingsDraft(), steps: steps, target: target,
                                     engineModel: fixture.engines?.model("text/model-b")?.model)
    #expect(face.size == 16384 && face.index == 3 && face.text == "16k")
    #expect(face.fit != nil)
    // A size chosen by the owner gets its handle and verdict too.
    let chosen = Inspector.contextFace(context, cache: rows[1], draft: SettingsDraft(values: ["load.ctx_size": .set(.number(65536))]),
                                       steps: steps, target: target, engineModel: nil)
    #expect(chosen.size == 65536 && chosen.index == 5 && chosen.fit != nil)
}

@Test func resettingALockedSettingSaysItAlsoRemovesTheLock() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    let plan = Inspector.plan(target: target, draft: SettingsDraft(values: ["temperature": .reset, "top_p": .reset]),
                              engines: fixture.engines, machines: fixture.machines)
    let lines = Dictionary(uniqueKeysWithValues: plan.lines.map { ($0.key, $0) })
    #expect(lines["temperature"]?.to == "engine default · also removes the lock")
    #expect(lines["top_p"]?.to == "engine default")
}

@Test func loadedModelsAreCountedOnceAndRestingIsNotLoaded() throws {
    // Real-shaped: two more names stand for the one loaded model, as aliases of a loaded model do.
    var json = try Fixture.json("window-synthetic.json")
    guard case .object(var root) = json, case .object(var models) = root["models"], case .array(var text) = models["text"],
          case .object(var settings) = root["settings"] else { Issue.record("fixture shape"); return }
    text.append(contentsOf: [.string("model-a-long"), .string("model-a-vision")])
    models["text"] = .array(text)
    root["models"] = .object(models)
    settings["model-a-long"] = settings["model-a"]
    settings["model-a-vision"] = settings["model-a"]
    root["settings"] = .object(settings)
    json = .object(root)
    let fixture = ControlFixture(json)
    let rows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                              machines: fixture.machines, settings: fixture.settings)
    // Four names reach the loaded model (model-a, its two aliases and the route "fast") ...
    #expect(rows.filter { $0.load == .loaded }.count == 4)
    // ... but one model is in memory.
    #expect(ModelList.loadedCount(engines: fixture.engines) == 1)
    // The Mac lists a model that is not in memory: resting, not loaded.
    let mac = try #require(fixture.machines.first { $0.kind == .mac })
    #expect(mac.loaded.map(\.state) == [.resting])
    #expect(mac.inMemory == 0)
    #expect(LoadState.resting.word == "Resting")
    #expect(rows.first { $0.name == "vision-model" }?.load.word == "Resting")
    #expect(fixture.machines.first { $0.kind == .gpuBox }?.inMemory == 1)
}

@Test func theSubtitleIsOneShortLineOverEveryMachine() throws {
    #expect(Machines.summary(try Fixture.synthetic.machines) == "GPU box on · Mac on")
    #expect(Machines.summary(try Fixture.worst.machines) == "2 GPU boxes · 1 on · Mac not answering")
    #expect(Machines.summary([]) == "")
}
