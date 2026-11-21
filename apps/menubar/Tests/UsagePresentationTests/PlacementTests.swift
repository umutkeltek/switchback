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
    let installed = StatusItemPlacement.autosaveName
    #expect(StatusItemPlacement.autosaveName(info: nil) == installed)
    #expect(StatusItemPlacement.autosaveName(info: "") == installed)
    #expect(StatusItemPlacement.autosaveName(info: "SwitchbackBarPlacementTest") == "SwitchbackBarPlacementTest")
    #expect(StatusItemPlacement.autosaveName(info: "bad name with spaces") == installed)
}

@Test func theInstalledItemNoLongerUsesTheNameMacOSKeepsHidden() {
    // macOS keeps an item named "SwitchbackBar" hidden at the left end whatever place the app writes.
    #expect(StatusItemPlacement.autosaveName != StatusItemPlacement.retiredAutosaveName)
    #expect(StatusItemPlacement.retiredAutosaveName == "SwitchbackBar")
    #expect(StatusItemPlacement.autosaveName(info: StatusItemPlacement.autosaveName) == StatusItemPlacement.autosaveName)
}

@Test func onlyAnOwnersPlaceRightOfTheAppsIsCarriedToTheNewName() {
    // No old place, or the app's own: the app's place.
    #expect(StatusItemPlacement.carriedPlace(legacyStored: nil, legacyPlaced: nil) == 280)
    #expect(StatusItemPlacement.carriedPlace(legacyStored: 280, legacyPlaced: 280) == 280)
    // The owner dragged it right of the app's place: kept.
    #expect(StatusItemPlacement.carriedPlace(legacyStored: 190, legacyPlaced: 280) == 190)
    // From before the app wrote places, right of the app's place: the owner's, kept.
    #expect(StatusItemPlacement.carriedPlace(legacyStored: 150, legacyPlaced: nil) == 150)
    // An old app place right of today's is the app's, not the owner's.
    #expect(StatusItemPlacement.carriedPlace(legacyStored: 200, legacyPlaced: 200) == 280)
    // Further left is where macOS hides first: the app's place.
    #expect(StatusItemPlacement.carriedPlace(legacyStored: 612, legacyPlaced: 280) == 280)
    #expect(StatusItemPlacement.carriedPlace(legacyStored: 1307, legacyPlaced: nil) == 280)
    #expect(StatusItemPlacement.carriedPlace(legacyStored: .nan, legacyPlaced: nil) == 280)
    #expect(StatusItemPlacement.carriedPlace(legacyStored: -5, legacyPlaced: nil) == 280)
}

@Test func anInstallUnderTheOldNameMovesToTheNewNameOnceAndKeepsAnOwnersPlace() throws {
    let suite = "placement-test-\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let old = StatusItemPlacement.positionKey("OldName")
    let new = StatusItemPlacement.positionKey("NewName")

    // Today's install: the app's place under the old name.
    defaults.set(280.0, forKey: old)
    defaults.set(280.0, forKey: StatusItemPlacement.placedKey)
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "NewName", retiredName: "OldName") == .place(280))
    #expect(defaults.double(forKey: new) == 280)
    #expect(defaults.double(forKey: old) == 280)
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "NewName", retiredName: "OldName") == .keep)

    // The owner had dragged the old item right of the app's place: carried, and kept after.
    defaults.removeObject(forKey: new)
    defaults.set(175.0, forKey: old)
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "NewName", retiredName: "OldName") == .place(175))
    #expect(defaults.double(forKey: new) == 175)
    #expect(defaults.double(forKey: StatusItemPlacement.placedKey) == 280)
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "NewName", retiredName: "OldName") == .keep)
    #expect(defaults.double(forKey: new) == 175)

    // Dragged left of the app's place under the old name: the app's place, where macOS shows it.
    defaults.removeObject(forKey: new)
    defaults.set(905.0, forKey: old)
    #expect(StatusItemPlacement.apply(defaults, autosaveName: "NewName", retiredName: "OldName") == .place(280))
}

@Test func aFreshInstallUnderTheNewNameGetsTheAppsPlace() throws {
    let suite = "placement-test-\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    #expect(StatusItemPlacement.apply(defaults, autosaveName: StatusItemPlacement.autosaveName) == .place(280))
    #expect(defaults.double(forKey: StatusItemPlacement.positionKey(StatusItemPlacement.autosaveName)) == 280)
    #expect(defaults.double(forKey: StatusItemPlacement.placedKey) == 280)
    #expect(defaults.object(forKey: StatusItemPlacement.positionKey(StatusItemPlacement.retiredAutosaveName)) == nil)
}
