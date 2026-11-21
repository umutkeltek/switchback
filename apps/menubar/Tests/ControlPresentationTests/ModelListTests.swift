import Foundation
import Testing
@testable import ControlPresentation

@Test func modelRowsShowWhereEachRunsAndWhatIsLoaded() throws {
    let fixture = try Fixture.synthetic
    let rows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                              machines: fixture.machines, settings: fixture.settings)
    func row(_ name: String) -> ModelRow? { rows.first { $0.name == name } }
    #expect(rows.map(\.job).first == .text && rows.map(\.job).last == .segment)
    #expect(row("model-a")?.location == .machine(.gpuBox, title: "GPU box"))
    #expect(row("model-a")?.load == .loaded)
    #expect(row("model-a")?.saved == true)
    #expect(row("model-b")?.saved == false && row("model-b")?.load == .unloaded)
    #expect(row("vision-model")?.location == .machine(.mac, title: "Mac"))
    #expect(row("vision-model")?.load == .resting)
    // Routes pick one of several models; they are marked apart from real models.
    #expect(row("fast")?.isRoute == true && row("fast")?.targets.count == 2)
    #expect(row("fast")?.elsewhere == 1)
    #expect(row("fast")?.load == .loaded)
    #expect(row("model-a")?.isRoute == false)
    // A name that stands for one model with another id says which.
    #expect(row("transcribe")?.standsFor == "speech-model")
    #expect(row("transcribe")?.isRoute == false)
    // Image, video and segment names are matched to the speed report's models by id.
    #expect(row("image/model-e")?.location == .machine(.gpuBox, title: "GPU box"))
    #expect(row("image/model-e")?.load == LoadState.none)
}

@Test func speedBarsCompareWithTheCeilingOrTheFastestModel() throws {
    let fixture = try Fixture.synthetic
    let rows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                              machines: fixture.machines, settings: fixture.settings)
    let a = try #require(rows.first { $0.name == "model-a" }?.speed)
    #expect(a.againstCeiling)
    #expect(abs(a.fraction - 142.5 / 168) < 0.0001)
    #expect(a.detail == "142 tok/s · 85% of its 168 tok/s ceiling")
    let b = try #require(rows.first { $0.name == "model-b" }?.speed)
    #expect(!b.againstCeiling)
    #expect(abs(b.fraction - 61.0 / 142.5) < 0.0001)
    #expect(rows.first { $0.name == "model-c" }?.speed == nil)
}

@Test func namesNotReadYetAreMatchedById() throws {
    let fixture = try Fixture.synthetic
    let rows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                              machines: fixture.machines, settings: [:])
    let a = try #require(rows.first { $0.name == "model-a" })
    #expect(a.targets == ["text/model-a"])
    #expect(!a.known && !a.isRoute)
    // A route's name matches no engine model before its settings are read: where it runs is not known yet.
    #expect(rows.first { $0.name == "fast" }?.location == .unknown)
}

@Test func theWorstCaseListsFortyTextModels() throws {
    let fixture = try Fixture.worst
    let rows = ModelList.rows(catalog: fixture.catalog, engines: fixture.engines, doctor: fixture.doctor,
                              machines: fixture.machines, settings: fixture.settings)
    #expect(rows.filter { $0.job == .text }.count == 40)
    #expect(rows.contains { $0.load == .failed })
    #expect(rows.first { $0.name == "default" }?.isRoute == true)
}
