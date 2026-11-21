import Foundation
import Testing
@testable import ControlPresentation

// The window shows only what is true now: an asleep box has nothing in memory and no Power off, every place
// says "In memory" or "Resting" the same way, Unload is only for a model in memory, a waiting reset can be undone
// on its own row, and a power reading the window cannot vouch for is not shown as a confident "On".

/// One GPU box with one text engine. `seenSecondsAgo` is how old the engine listing is; nil leaves its time out.
private func gpuBox(power: String, models: [(String, String)], seenSecondsAgo: Double? = 3600, countdown: Double? = nil,
                    evidence: PowerEvidence? = nil) throws -> MachineCard {
    let countdownText = countdown.map { "\(Int($0))" } ?? "null"
    let status = try JSONValue.parse(Data("""
    {"machines":[{"machine":"gpu-box","power":"\(power)"}],
     "lanes":[{"name":"text","host":"gpu-box","state":"offline","queue_depth":0,"in_flight":0,"holds":[],
               "poweroff_in_secs":\(countdownText),"wake_configured":true,"poweroff_configured":true,
               "wake_duration_ewma_ms":67490}]}
    """.utf8))
    let nowMs = Fixture.now.timeIntervalSince1970 * 1000
    let engines = EnginesReport(lanes: [EngineLane(
        lane: "text", state: power == "on" ? "up" : "asleep",
        models: models.map { EngineModel(id: $0.0, target: "text/" + $0.0, status: $0.1) }, acceptsControl: true,
        seenAtMs: seenSecondsAgo.map { nowMs - $0 * 1000 })])
    return try #require(Machines.cards(status: AIStatus(status), engines: engines, doctor: nil, now: Fixture.now,
                                       evidence: evidence).first)
}

@Test func anAsleepBoxListsNothingInMemoryAndOffersNoPowerOff() throws {
    let listing = [("big-model", "sleeping"), ("small-model", "loaded"), ("other", "unloaded")]
    for power in ["off", "powering off"] {
        let box = try gpuBox(power: power, models: listing)
        #expect(box.loaded.isEmpty, "\(power): \(box.loaded.map(\.name)) listed as in memory")
        #expect(box.inMemory == 0)
        #expect(Machines.powerOff(box) == nil, "\(power) still offers Power off")
        // Only the tiles that mean something while it sleeps.
        #expect(Machines.stats(box).map(\.title) == ["Wake takes", "GPU"])
        #expect(Machines.stats(box).first?.value == "~1 min")
    }
    #expect(ModelList.inMemoryText([try gpuBox(power: "off", models: listing)]) == nil)
    // Waking: still nothing in memory; Power off stays visible but waits.
    let waking = try gpuBox(power: "waking", models: listing)
    #expect(waking.loaded.isEmpty)
    #expect(Machines.powerOff(waking)?.gate == .disabled("Waking up"))
    #expect(Machines.stats(waking).map(\.title) == ["Wake takes", "GPU", "Working"])
    // On: the listing is now, and every tile shows.
    let on = try gpuBox(power: "on", models: listing)
    #expect(on.loaded.map(\.name) == ["big-model", "small-model"])
    #expect(on.inMemory == 1)
    #expect(on.lastLoaded.isEmpty)
    #expect(Machines.powerOff(on) != nil)
    #expect(Machines.stats(on).map(\.title) == ["Up", "GPU", "Working", "Powers off"])
}

@Test func lastLoadedShowsOnlyAListingFromBeforeThePowerOff() throws {
    let listing = [("big-model", "sleeping"), ("small-model", "loaded"), ("other", "unloaded"), ("big-model", "sleeping")]
    // Read while the box was up, an hour ago; the box is asleep now, so the listing is from before it powered off.
    #expect(try gpuBox(power: "off", models: listing).lastLoaded == ["big-model", "small-model"])
    // A listing whose time is not given (or is later than now) proves nothing: no "Last loaded" either.
    #expect(try gpuBox(power: "off", models: listing, seenSecondsAgo: nil).lastLoaded.isEmpty)
    #expect(try gpuBox(power: "off", models: listing, seenSecondsAgo: -60).lastLoaded.isEmpty)
    // Nothing was in memory or resting: nothing to name.
    #expect(try gpuBox(power: "off", models: [("other", "unloaded")]).lastLoaded.isEmpty)
    // As in the real capture: the box is off and its engine listed one resting model before it powered off.
    let real = try gpuBox(power: "off", models: [("model-x", "unloaded"), ("model-y", "sleeping")])
    #expect(real.loaded.isEmpty && real.lastLoaded == ["model-y"])
}

@Test func everyPlaceSaysInMemoryOrResting() throws {
    #expect(LoadState.loaded.word == "In memory")
    #expect(LoadState.resting.word == "Resting")
    let fixture = try Fixture.synthetic
    let machines = fixture.machines
    let box = try #require(machines.first { $0.kind == .gpuBox })
    let mac = try #require(machines.first { $0.kind == .mac })
    // The machine cards (Now and Machines) and the model list use the same words and dots.
    #expect(box.loaded.map(\.word) == ["In memory"])
    #expect(box.loaded.map(\.loadState) == [.loaded])
    #expect(mac.loaded.map(\.word) == ["Resting"])
    #expect(mac.loaded.map(\.loadState) == [.resting])
    #expect(mac.inMemory == 0)
    let rows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                              machines: machines, settings: fixture.settings)
    #expect(rows.first { $0.name == "model-a" }?.load.word == "In memory")
    #expect(rows.first { $0.name == "vision-model" }?.load.word == "Resting")
    // The sidebar counts what the cards call in memory: the resting Mac model is not.
    #expect(ModelList.inMemoryText(machines) == "1 in memory")
    // A box that is asleep: its listing still says loaded, but the list says what is true now.
    let asleep = machines.map { card in
        MachineCard(machine: card.machine, kind: card.kind, title: card.title, power: card.kind == .gpuBox ? .asleep : card.power,
                    powerWord: card.powerWord, powerOffIn: nil, uptime: nil, gpuUser: nil, busy: 0, guardBusy: false,
                    holds: [], loaded: [], problems: [], wakeSeconds: 68, lanes: card.lanes)
    }
    let sleepingRows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                                      machines: asleep, settings: fixture.settings)
    #expect(sleepingRows.first { $0.name == "model-a" }?.load == .unloaded)
    #expect(ModelList.inMemoryText(asleep) == nil)
}

/// The synthetic listing with one model's status changed.
private func engines(_ fixture: ControlFixture, _ target: String, status: String) -> EnginesReport {
    EnginesReport(lanes: (fixture.engines?.lanes ?? []).map { lane in
        EngineLane(lane: lane.lane, kind: lane.kind, state: lane.state,
                   models: lane.models.map { model in
                       EngineModel(id: model.id, target: model.target, status: model.target == target ? status : model.status,
                                   arguments: model.arguments, memory: model.memory)
                   }, acceptsControl: lane.acceptsControl, seenAtMs: lane.seenAtMs)
    })
}

@Test func unloadShowsOnlyForAModelInMemory() throws {
    let fixture = try Fixture.synthetic
    let modelA = try #require(fixture.target("model-a"))
    let loaded = Inspector.loadActions(modelA, engines: fixture.engines, machines: fixture.machines)
    #expect(loaded.unload?.title == "Unload" && loaded.load == nil)
    // Resting: not in memory. No Unload; Load now instead, where loading is allowed.
    let resting = engines(fixture, "text/model-a", status: "sleeping")
    let restingActions = Inspector.loadActions(modelA, engines: resting, machines: fixture.machines)
    #expect(restingActions.unload == nil)
    #expect(restingActions.load?.title == "Load now")
    // A route: the model chosen in its picker decides. "default" picks a model that is not loaded; "fast" one that is.
    let routeIdle = try #require(fixture.target("default", 0))
    #expect(Inspector.loadActions(routeIdle, engines: fixture.engines, machines: fixture.machines).unload == nil)
    let routeLoaded = try #require(fixture.target("fast", 0))
    #expect(Inspector.loadActions(routeLoaded, engines: fixture.engines, machines: fixture.machines).unload != nil)
    // A box that is asleep has nothing in memory, whatever its last listing says: Load now, which wakes it.
    let asleep = fixture.machines.map { card in
        MachineCard(machine: card.machine, kind: card.kind, title: card.title, power: card.kind == .gpuBox ? .asleep : card.power,
                    powerWord: card.powerWord, powerOffIn: nil, uptime: nil, gpuUser: nil, busy: 0, guardBusy: false,
                    holds: [], loaded: [], problems: [], wakeSeconds: 68, lanes: card.lanes)
    }
    let sleeping = Inspector.loadActions(modelA, engines: fixture.engines, machines: asleep)
    #expect(sleeping.unload == nil)
    #expect(sleeping.load?.wakes == true)
}

@Test func aPendingResetCanBeUndoneOnItsOwnRow() throws {
    let fixture = try Fixture.synthetic
    let target = try #require(fixture.target("model-a"))
    let rows = Dictionary(uniqueKeysWithValues: Inspector.answerSettings(target).map { ($0.key, $0) })
    let temperature = try #require(rows["temperature"])
    let topP = try #require(rows["top_p"])
    #expect(temperature.value != nil, "the fixture saves a temperature")
    // Nothing waiting: the arrow resets the saved value (after Apply).
    var draft = SettingsDraft()
    #expect(Inspector.rowUndo(temperature, draft: draft) == .reset)
    Inspector.press(.reset, for: temperature, in: &draft)
    #expect(draft.values["temperature"] == .reset)
    // The reset now waits for Apply, beside another change. Its row offers Undo, which takes back only that reset.
    draft.values["top_p"] = .set(.number(0.5))
    let undo = try #require(Inspector.rowUndo(temperature, draft: draft))
    #expect(undo == .undoReset)
    #expect(undo.help == "Undo the reset · keep the saved value")
    Inspector.press(undo, for: temperature, in: &draft)
    #expect(draft.values["temperature"] == nil)
    #expect(draft.values["top_p"] == .set(.number(0.5)), "undoing one row dropped another change")
    // A waiting edit is taken back the same way.
    #expect(Inspector.rowUndo(topP, draft: draft) == .undoChange)
    Inspector.press(.undoChange, for: topP, in: &draft)
    #expect(draft.isEmpty)
    // A row that may not change here has no undo.
    let locked = SettingState(key: "temperature", title: "Temperature", value: .number(0.6), shown: nil, badge: .profile("p"),
                              gate: .disabled("Set by the profile p"))
    #expect(Inspector.rowUndo(locked, draft: SettingsDraft(values: ["temperature": .reset])) == nil)
}

@Test func aPowerReadingTheWindowCannotVouchForIsNotAConfidentOn() throws {
    let on = [("model-a", "loaded")]
    func line(_ card: MachineCard) -> (String, Bool) { let l = Machines.statusLine(card); return (l.text, l.muted) }
    // Fresh: a confident line.
    let fresh = try gpuBox(power: "on", models: on, evidence: PowerEvidence(readAt: Fixture.now.addingTimeInterval(-3)))
    #expect(line(fresh).0.hasPrefix("On") && !line(fresh).1)
    #expect(Machines.shortStatus(fresh) == "On")
    // Older than 15 s.
    let old = try gpuBox(power: "on", models: on, evidence: PowerEvidence(readAt: Fixture.now.addingTimeInterval(-40)))
    #expect(line(old) == ("Last seen on 40 s ago", true))
    #expect(Machines.shortStatus(old) == "Last seen on")
    #expect(Machines.summary([old]) == "GPU box last seen on")
    // The read after it failed.
    let failed = try gpuBox(power: "on", models: on,
                            evidence: PowerEvidence(readAt: Fixture.now.addingTimeInterval(-5), failed: true))
    #expect(line(failed) == ("Last seen on 5 s ago", true))
    #expect(failed.powerDoubt == "the last reading failed")
    let failedNow = try gpuBox(power: "on", models: on, evidence: PowerEvidence(readAt: Fixture.now, failed: true))
    #expect(line(failedNow) == ("Last seen on just now", true))
    // Just shown again, not read since.
    let shown = try gpuBox(power: "on", models: on,
                           evidence: PowerEvidence(readAt: Fixture.now.addingTimeInterval(-2), awaitingFresh: true))
    #expect(line(shown) == ("Checking…", true))
    // The idle power-off under 30 s away.
    let soon = try gpuBox(power: "on", models: on, countdown: 20, evidence: PowerEvidence(readAt: Fixture.now))
    #expect(line(soon) == ("About to power off · in 20 s", true))
    // An old reading of a sleeping box is not a confident "Asleep" either.
    let asleep = try gpuBox(power: "off", models: on, evidence: PowerEvidence(readAt: Fixture.now.addingTimeInterval(-40)))
    #expect(line(asleep) == ("Last seen asleep 40 s ago", true))
    let asleepFresh = try gpuBox(power: "off", models: on, evidence: PowerEvidence(readAt: Fixture.now))
    #expect(line(asleepFresh) == ("Asleep · wakes when work arrives", false))
}
