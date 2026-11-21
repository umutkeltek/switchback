import Foundation
import Testing
@testable import UsagePresentation

@Test func anItemWithoutAPlaceGetsTheAppsPlaceTowardTheRight() {
    #expect(StatusItemPlacement.decide(stored: nil, placed: nil) == .place(StatusItemPlacement.position))
    // The app wrote a place, but macOS has none any more (preferences reset): written again.
    #expect(StatusItemPlacement.decide(stored: nil, placed: 280) == .place(StatusItemPlacement.position))
    #expect(StatusItemPlacement.decide(stored: .nan, placed: nil) == .place(StatusItemPlacement.position))
}

@Test func theOwnersCommandDragPlaceIsKept() {
    // The app wrote 280; the owner dragged the item, macOS stored 612.
    #expect(StatusItemPlacement.decide(stored: 612, placed: 280) == .keep)
    // Dragged further right too.
    #expect(StatusItemPlacement.decide(stored: 120, placed: 280) == .keep)
}

@Test func theAppsOwnPlaceStaysOrMovesWithTheApp() {
    #expect(StatusItemPlacement.decide(stored: 280, placed: 280, target: 280) == .keep)
    // A new app place moves an item still at the old app place, never a dragged one.
    #expect(StatusItemPlacement.decide(stored: 300, placed: 300, target: 280) == .place(280))
    #expect(StatusItemPlacement.decide(stored: 640, placed: 300, target: 280) == .keep)
}

@Test func aPlaceFromBeforeTheRuleMovesRightOnlyWhenItIsFurtherLeft() {
    // The new-item place at the left end: moved once.
    #expect(StatusItemPlacement.decide(stored: 1307, placed: nil, target: 280) == .place(280))
    // Already at or right of the app's place: kept.
    #expect(StatusItemPlacement.decide(stored: 280, placed: nil, target: 280) == .keep)
    #expect(StatusItemPlacement.decide(stored: 190, placed: nil, target: 280) == .keep)
}

@Test func applyWritesThePlaceAndItsRecordAndThenKeepsAnOwnersMove() throws {
    let suite = "placement-test-\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let key = StatusItemPlacement.positionKey("SwitchbackBarTest")
    #expect(key == "NSStatusItem Preferred Position SwitchbackBarTest")

    #expect(StatusItemPlacement.apply(defaults, autosaveName: "SwitchbackBarTest") == .place(280))
    #expect(defaults.double(forKey: key) == 280)
    #expect(defaults.double(forKey: StatusItemPlacement.placedKey) == 280)
    // Next launch: unchanged.
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "SwitchbackBarTest") == .keep)
    // The owner Command-drags it; macOS stores the new place.
    defaults.set(733.5, forKey: key)
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "SwitchbackBarTest") == .keep)
    #expect(defaults.double(forKey: key) == 733.5)
}

@Test func aTestCopyNamesItsOwnItem() {
    #expect(StatusItemPlacement.autosaveName(info: nil) == "SwitchbackBar")
    #expect(StatusItemPlacement.autosaveName(info: "") == "SwitchbackBar")
    #expect(StatusItemPlacement.autosaveName(info: "SwitchbackBarPlacementTest") == "SwitchbackBarPlacementTest")
    #expect(StatusItemPlacement.autosaveName(info: "bad name with spaces") == "SwitchbackBar")
}
