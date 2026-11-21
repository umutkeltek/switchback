import Foundation
import CoreGraphics
import Testing
@testable import UsagePresentation

@Test func statusItemCountsAsShownOnlyWhenMacOSShowsIt() {
    let screens = [CGRect(x: 0, y: 0, width: 1600, height: 900)]
    let item = CGRect(x: 1400, y: 876, width: 24, height: 24)
    #expect(StatusItemVisibility.onScreen(frame: item, screens: screens, occlusionVisible: true))
    // Hidden by a crowded or auto-hidden menu bar: the window exists but is not visible.
    #expect(!StatusItemVisibility.onScreen(frame: item, screens: screens, occlusionVisible: false))
    // Parked off every screen, or never laid out.
    #expect(!StatusItemVisibility.onScreen(frame: CGRect(x: -40, y: 876, width: 24, height: 24), screens: screens, occlusionVisible: true))
    #expect(!StatusItemVisibility.onScreen(frame: .zero, screens: screens, occlusionVisible: true))
    #expect(!StatusItemVisibility.onScreen(frame: nil, screens: screens, occlusionVisible: true))
}

@Test func visibilityHistoryKeepsOnlyChangesAndIsBounded() {
    var history: [String] = []
    for (index, visible) in [true, true, false, false, true].enumerated() {
        history = StatusItemVisibility.record(history, visible: visible, at: instant.addingTimeInterval(Double(index)))
    }
    #expect(history.map { $0.split(separator: " ").last.map(String.init) ?? "" } == ["visible", "hidden", "visible"])
    for index in 0..<40 {
        history = StatusItemVisibility.record(history, visible: index % 2 == 0, at: instant)
    }
    #expect(history.count == 12)
}

@Test func openPanelShortcutDefaultsOnAndCanBeTurnedOff() {
    #expect(ShortcutChoice.selected(nil)?.title == "⌃⌥⌘L")
    #expect(ShortcutChoice.selected("unknown-choice") == ShortcutChoice.all[0])
    #expect(ShortcutChoice.selected("control-option-command-b")?.keyCode == 0x0B)
    #expect(ShortcutChoice.selected(ShortcutChoice.off) == nil)
    #expect(Set(ShortcutChoice.all.map(\.id)).count == ShortcutChoice.all.count)
    // Every choice needs three modifiers so it cannot collide with ordinary typing.
    #expect(ShortcutChoice.all.allSatisfy { $0.modifiers.nonzeroBitCount == 3 })
}

@Test func windowShortcutDefaultsOnAndCanBeTurnedOffTheSameWay() {
    #expect(ShortcutChoice.window(nil) == ShortcutChoice.window)
    #expect(ShortcutChoice.window("on")?.title == "⌃⌥⌘W")
    #expect(ShortcutChoice.window(ShortcutChoice.off) == nil)
    // Its own preference, apart from the panel's, and a key no panel choice uses.
    #expect(ShortcutChoice.windowPreferenceKey != ShortcutChoice.preferenceKey)
    #expect(!ShortcutChoice.all.contains { $0.keyCode == ShortcutChoice.window.keyCode })
    #expect(ShortcutChoice.window.modifiers.nonzeroBitCount == 3)
}
