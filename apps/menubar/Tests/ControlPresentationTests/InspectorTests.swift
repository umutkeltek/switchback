import Foundation
import Testing
@testable import ControlPresentation

@Test func badgesSayWhereEachValueComesFrom() {
    #expect(SourceBadge.of(source: "engine", locked: false) == .engine)
    #expect(SourceBadge.of(source: "defaults", locked: false) == .saved)
    #expect(SourceBadge.of(source: "profile:night", locked: false) == .profile("night"))
    #expect(SourceBadge.of(source: "route_profile:fast", locked: false) == .profile("fast"))
    #expect(SourceBadge.of(source: "defaults", locked: true) == .locked)
    #expect(SourceBadge.locked.symbol == "lock.fill")
    #expect(SourceBadge.saved.word == "saved for this model")
    #expect(SourceBadge.engine.word == "engine default")
}

@Test func answerSettingsShowValuesBadgesAndWhatMayChange() throws {
    let target = try #require(try Fixture.synthetic.target("model-a"))
    let rows = Dictionary(uniqueKeysWithValues: Inspector.answerSettings(target).map { ($0.key, $0) })
    #expect(rows["temperature"]?.value == .number(0.6))
    #expect(rows["temperature"]?.badge == .locked)
    #expect(rows["top_p"]?.badge == .saved)
    #expect(rows["max_tokens"]?.value == nil && rows["max_tokens"]?.badge == .engine)
    #expect(rows["reasoning.enabled"]?.badge == .engine)
    #expect(rows["reasoning_guard.enabled"]?.value == nil)
    #expect(rows.values.allSatisfy { $0.gate.isAllowed })
    // A model marked as thinking with the guard on shows the guard on, saved.
    let vision = try #require(try Fixture.synthetic.target("vision-model"))
    let guardRow = Inspector.answerSettings(vision).first { $0.key == "reasoning_guard.enabled" }
    #expect(guardRow?.value == .bool(true) && guardRow?.badge == .saved)
}

@Test func aValueSetByAProfileCannotBeChangedHereAndSaysWhy() throws {
    let worst = try Fixture.worst
    let name = try #require(worst.selected)
    let target = try #require(worst.target(name))
    let topP = Inspector.answerSettings(target).first { $0.key == "top_p" }
    #expect(topP?.badge == .profile("a-very-long-profile-name"))
    #expect(topP?.gate == .disabled("Set by the profile a-very-long-profile-name"))
}

@Test func theMacNeverGetsLoadSettingsLoadOrUnload() throws {
    let fixture = try Fixture.synthetic
    let vision = try #require(fixture.target("vision-model"))
    #expect(Inspector.loadSettings(vision, engines: fixture.engines, machines: fixture.machines) == nil)
    let actions = Inspector.loadActions(vision, engines: fixture.engines, machines: fixture.machines)
    #expect(actions.load == nil && actions.unload == nil)
    // Even if the Mac's engine claimed to accept load control, the Mac stays show-only.
    let claiming = EnginesReport(lanes: (fixture.engines?.lanes ?? []).map { lane in
        EngineLane(lane: lane.lane, kind: lane.kind, state: lane.state, error: lane.error, models: lane.models, acceptsControl: true)
    })
    #expect(Inspector.loadSettings(vision, engines: claiming, machines: fixture.machines) == nil)
    let claimed = Inspector.loadActions(vision, engines: claiming, machines: fixture.machines)
    #expect(claimed.load == nil && claimed.unload == nil)
    #expect(Inspector.loadControl(vision, engines: claiming, machines: fixture.machines) == nil)
}

@Test func aListingOnlyEngineOnTheGPUBoxGetsNoLoadControl() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    let listing = EnginesReport(lanes: (fixture.engines?.lanes ?? []).map { lane in
        EngineLane(lane: lane.lane, kind: lane.kind, state: lane.state, error: lane.error, models: lane.models, acceptsControl: false)
    })
    #expect(Inspector.loadSettings(target, engines: listing, machines: fixture.machines) == nil)
    #expect(Inspector.loadActions(target, engines: listing, machines: fixture.machines).unload == nil)
}

@Test func loadSettingsReadSavedValuesAndTheEnginesOwn() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    let rows = try #require(Inspector.loadSettings(target, engines: fixture.engines, machines: fixture.machines))
    #expect(rows.map(\.key) == ["load.ctx_size", "load.cache_type_k", "load.parallel"])
    #expect(rows[0].value == .number(32768) && rows[0].badge == .saved)
    #expect(rows[1].value == .string("q8_0"))
    // Parallel slots: nothing saved, so the engine's own value (from its start arguments) shows.
    #expect(rows[2].value == nil && rows[2].shown == .number(2) && rows[2].badge == .engine)
    #expect(Inspector.range("load.ctx_size", target: target) == 2048...131072)
}

@Test func loadingOnASleepingBoxIsFlaggedAsAWake() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-b"))
    let awake = Inspector.loadActions(target, engines: fixture.engines, machines: fixture.machines)
    #expect(awake.load?.title == "Load now")
    #expect(awake.load?.wakes == false)
    #expect(awake.load?.command.arguments == ["ai", "load", "text/model-b"])
    let asleep = fixture.machines.map { card in
        MachineCard(machine: card.machine, kind: card.kind, title: card.title, power: card.kind == .gpuBox ? .asleep : card.power,
                    powerWord: card.powerWord, powerOffIn: nil, uptime: nil, gpuUser: nil, busy: 0, guardBusy: false,
                    holds: [], loaded: [], problems: [], wakeSeconds: 68, lanes: card.lanes)
    }
    let sleeping = Inspector.loadActions(target, engines: fixture.engines, machines: asleep)
    #expect(sleeping.load?.title == "Load now · wakes the GPU box")
    #expect(sleeping.load?.wakes == true)
    #expect(sleeping.load?.command.arguments == ["ai", "load", "text/model-b", "--wake"])
    #expect(sleeping.load?.confirmation != nil)
    // Trying a model on a sleeping box wakes it too, and says so.
    let send = Inspector.tryIt("model-b", targets: ["text/model-b"], prompt: "hi", machines: asleep)
    #expect(send.wakes && send.title == "Send · wakes the GPU box")
    #expect(send.command.arguments == ["ai", "chat", "--json", "--", "model-b", "hi"])
    #expect(Inspector.tryIt("model-b", targets: ["text/model-b"], prompt: "  ", machines: asleep).gate.isAllowed == false)
    // A loaded model offers Unload, never while it answers.
    let loaded = Inspector.loadActions(try #require(fixture.target("model-a")), engines: fixture.engines, machines: fixture.machines)
    #expect(loaded.unload?.command.arguments == ["ai", "unload", "text/model-a"])
    #expect(loaded.load == nil)
}

@Test func thePreviewSaysWhatChangesAndWhetherTheModelReloads() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    var draft = SettingsDraft()
    draft.values["top_p"] = .set(.number(0.9))
    draft.values["max_tokens"] = .set(.number(4096))
    draft.values["temperature"] = .set(.number(0.6))  // the same value: no change
    draft.values["load.ctx_size"] = .set(.number(65536))
    let later = Inspector.plan(target: target, draft: draft, engines: fixture.engines, machines: fixture.machines)
    #expect(later.lines.map(\.title) == ["Context size", "Max tokens", "Top P"])
    #expect(later.lines.first?.from == "32768" && later.lines.first?.to == "65536")
    #expect(later.lines[1].from == "engine default")
    #expect(later.actions.map(\.command.arguments) == [
        ["ai", "set", "text/model-a", "max_tokens=4096", "top_p=0.9"],
        ["ai", "set", "text/model-a", "ctx_size=65536", "--load", "--apply", "next-load"],
    ])
    #expect(!later.reloads)
    #expect(later.notes.contains("Takes effect on next load · the loaded model keeps its old settings"))
    draft.apply = .now
    let now = Inspector.plan(target: target, draft: draft, engines: fixture.engines, machines: fixture.machines)
    #expect(now.reloads)
    #expect(now.notes.contains("Reloads the model now · it pauses while it reloads"))
    #expect(now.actions.last?.command.arguments.suffix(2) == ["--apply", "now"])
    // A model that is not loaded takes load settings when it loads.
    let idle = try #require(fixture.target("model-b"))
    let idlePlan = Inspector.plan(target: idle, draft: SettingsDraft(values: ["load.ctx_size": .set(.number(8192))], apply: .now),
                                  engines: fixture.engines, machines: fixture.machines)
    #expect(idlePlan.notes == ["Takes effect when it loads"])
    #expect(idlePlan.actions.first?.command.arguments.suffix(2) == ["--apply", "next-load"])
}

@Test func resettingRemovesTheSavedValue() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    let plan = Inspector.plan(target: target, draft: SettingsDraft(values: ["top_p": .reset, "load.cache_type_k": .reset,
                                                                            "max_tokens": .reset]),
                              engines: fixture.engines, machines: fixture.machines)
    // max_tokens has nothing saved: nothing to reset.
    #expect(plan.lines.map(\.key) == ["load.cache_type_k", "top_p"])
    #expect(plan.actions.map(\.command.arguments) == [["ai", "unset", "text/model-a", "top_p"],
                                                      ["ai", "unset", "text/model-a", "cache_type_k", "--load"]])
}

@Test func turningTheGuardOnAlsoMarksTheModelAsThinking() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-b"))
    let plan = Inspector.plan(target: target, draft: SettingsDraft(values: ["reasoning_guard.enabled": .set(.bool(true)),
                                                                            "reasoning_guard.min_tokens": .set(.number(2048))]),
                              engines: fixture.engines, machines: fixture.machines)
    #expect(plan.actions.first?.command.arguments == ["ai", "set", "text/model-b", "reasoning_guard.enabled=true",
                                                      "reasoning_guard.min_tokens=2048", "reasoning=true"])
    #expect(plan.lines.contains { $0.title == "Thinking model" })
    #expect(plan.notes == ["Answer settings take effect on the next answer"])
}

@Test func historyOffersUndoToEveryEarlierVersion() throws {
    let history = try #require(try Fixture.synthetic.histories["text/model-a"])
    let rows = Inspector.versions(history, now: Fixture.now, machines: try Fixture.synthetic.machines)
    #expect(rows.map(\.title) == ["Version 3", "Version 2", "Version 1"])
    #expect(rows[0].isCurrent && rows[0].undo == nil)
    #expect(rows[1].what == "Temperature, Top P")
    #expect(rows[0].what == "KV cache")
    #expect(rows[1].when == "3 h ago")
    #expect(rows[2].undo?.command.arguments == ["ai", "revert", "text/model-a", "1"])
    #expect(rows[2].undo?.title == "Undo to here")
}

@Test func findingsReadInPlainWordsWithTheirCommand() throws {
    let fixture = try Fixture.synthetic
    let rows = Inspector.findings(fixture.doctor, targets: ["text/model-b"])
    #expect(rows.count == 1)
    #expect(rows[0].advice == "Turn on the reasoning guard")
    #expect(rows[0].command == "sb ai set text/model-b reasoning=true reasoning_guard.min_tokens=1024")
    #expect(rows[0].text.hasPrefix("4 answers came back empty"))
    #expect(rows[0].draft?.values["reasoning_guard.enabled"] == .set(.bool(true)))
    #expect(rows[0].draft?.values["reasoning_guard.min_tokens"] == .set(.number(1024)))
    #expect(Inspector.findings(fixture.doctor, targets: ["text/model-a"]).isEmpty)
}

@Test func theFitHintComparesWithTheMemoryBudget() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    let model = fixture.engines?.model("text/model-a")?.model
    let small = Inspector.fit(context: 32768, cacheK: "q8_0", target: target, engineModel: model)
    #expect(small.verdict == .fits)
    #expect(small.text.hasPrefix("Fits · "))
    let huge = Inspector.fit(context: 262_144, cacheK: "f16", target: target, engineModel: model)
    #expect(huge.verdict == .wontFit)
    #expect(huge.text.hasPrefix("Won’t fit · needs "))
    // Smaller cache types need less memory.
    let quarter = Inspector.fit(context: 131_072, cacheK: "q4_0", target: target, engineModel: model)
    let full = Inspector.fit(context: 131_072, cacheK: "f16", target: target, engineModel: model)
    #expect((quarter.neededBytes ?? 0) < (full.neededBytes ?? 0))
    // Without an estimate the engine's own measurement gives a size but no verdict; with neither, unknown.
    let idle = try #require(fixture.target("model-b"))
    #expect(Inspector.fit(context: 8192, cacheK: "f16", target: idle, engineModel: nil).verdict == .unknown)
    #expect(Inspector.fit(context: 8192, cacheK: "f16", target: idle, engineModel: nil).text == "Memory need unknown · no measurement yet")
    let measured = Inspector.fit(context: 65536, cacheK: "f16", target: idle,
                                 engineModel: EngineModel(id: "x", target: "text/x", status: "loaded",
                                                          memory: MemorySummary(minimum: MemoryPart(context: 4096, bytes: 10 * 1_073_741_824),
                                                                                maximum: MemoryPart(context: 131_072, bytes: 26 * 1_073_741_824),
                                                                                effective: nil)))
    #expect(measured.verdict == .unknown && measured.text.hasPrefix("About "))
}
