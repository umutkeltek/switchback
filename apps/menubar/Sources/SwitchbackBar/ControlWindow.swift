import AppKit
import SwiftUI
import ControlPresentation
import UsagePresentation

/// The Switchback window: one native window over everything `sb` offers. It reads only while visible.
final class ControlWindowController: NSObject, NSWindowDelegate {
    let store: ControlStore
    let state = ControlWindowState()
    let accounts: UsagePanelState
    private var window: NSWindow?

    init(store: ControlStore, accounts: UsagePanelState) {
        self.store = store
        self.accounts = accounts
    }

    var isOpen: Bool { window?.isVisible == true }

    /// Shows the window, or brings it forward. While it is open the app has a Dock icon and menus, like any window.
    func show() {
        if window == nil { window = makeWindow() }
        guard let window else { return }
        if NSApp.activationPolicy() != .regular {
            NSApp.setActivationPolicy(.regular)
            installMenus()
        }
        if window.isMiniaturized { window.deminiaturize(nil) }
        window.makeKeyAndOrderFront(nil)
        NSApp.activate()
        updateVisibility()
    }

    private func makeWindow() -> NSWindow {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: WindowMetrics.width, height: WindowMetrics.height),
                              styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
                              backing: .buffered, defer: false)
        window.title = "Switchback"
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.isMovableByWindowBackground = false
        window.isReleasedWhenClosed = false
        window.minSize = NSSize(width: WindowMetrics.minimumWidth, height: WindowMetrics.minimumHeight)
        window.setFrameAutosaveName("SwitchbackWindow")
        if !window.setFrameUsingName("SwitchbackWindow") { window.center() }
        window.collectionBehavior.insert(.fullScreenPrimary)
        window.delegate = self
        window.contentViewController = NSHostingController(rootView: ControlWindowView(store: store, state: state, accounts: accounts))
        window.setContentSize(NSSize(width: max(window.frame.width, WindowMetrics.minimumWidth),
                                     height: max(window.frame.height, WindowMetrics.minimumHeight)))
        return window
    }

    /// Reading runs only while the window can be seen: not hidden, not minimized, not covered.
    private func updateVisibility() {
        guard let window else { store.setVisible(false); return }
        store.setVisible(window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible))
    }

    func windowDidChangeOcclusionState(_ notification: Notification) { updateVisibility() }
    func windowDidMiniaturize(_ notification: Notification) { updateVisibility() }
    func windowDidDeminiaturize(_ notification: Notification) { updateVisibility() }
    func windowDidBecomeKey(_ notification: Notification) { updateVisibility() }
    func windowWillClose(_ notification: Notification) {
        store.setVisible(false)
        // Back to a menu bar app: no Dock icon once the window is gone, even while the panel is open (the panel
        // never needs one).
        DispatchQueue.main.async { [weak self] in
            if self?.window?.isVisible != true { NSApp.setActivationPolicy(.accessory) }
        }
    }

    /// The app menu, Edit (so copy and paste work in the window's fields) and Window.
    private func installMenus() {
        let main = NSMenu()
        let app = NSMenuItem()
        let appMenu = NSMenu(title: "Switchback")
        appMenu.addItem(withTitle: "Close Window", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Hide Switchback", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit Switchback", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        app.submenu = appMenu
        main.addItem(app)
        let edit = NSMenuItem()
        let editMenu = NSMenu(title: "Edit")
        editMenu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = editMenu.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        editMenu.addItem(.separator())
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        edit.submenu = editMenu
        main.addItem(edit)
        let windowItem = NSMenuItem()
        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowItem.submenu = windowMenu
        main.addItem(windowItem)
        NSApp.mainMenu = main
        NSApp.windowsMenu = windowMenu
    }
}

/// A runner for renders: it runs nothing.
struct NoCommandRunner: CommandRunning {
    func run(_ command: ControlCommand) async throws -> CommandResult { throw CommandFailure("Renders run no commands") }
}

/// Draws one place of the window offscreen to a PNG, from a saved set of answers. No window shows on screen and
/// no command runs.
@MainActor func renderWindow(_ fixturePath: String, place: WindowPlace, theme: String, output: String, scale: Double = 2,
                             width: Double = WindowMetrics.width, height: Double = WindowMetrics.height, review: Bool = false) throws {
    let app = NSApplication.shared
    app.setActivationPolicy(.prohibited)
    guard scale == 1 || scale == 2 else { throw OwnerError.failed }
    let data = try Data(contentsOf: URL(fileURLWithPath: fixturePath))
    guard data.count <= 16_777_216 else { throw OwnerError.outputTooLarge }
    let json = try JSONValue.parse(data)
    let fixture = ControlFixture(json)
    let store = ControlStore(runner: NoCommandRunner(), clock: { fixture.now })
    store.load(fixture)
    let state = ControlWindowState()
    state.place = place
    // A saved draft shows the change preview: {"draft": {"target": TARGET, "values": {KEY: VALUE}, "apply": "now"}}.
    if let target = json["draft"]["target"].string {
        var draft = SettingsDraft(apply: json["draft"]["apply"].string == "now" ? .now : .nextLoad)
        for (key, value) in json["draft"]["values"].object ?? [:] { draft.values[key] = value.isNull ? .reset : .set(value) }
        state.drafts[target] = draft
        state.reviewing = review
    }
    if let prompt = json["prompt"].string, let model = fixture.selected { state.prompts[model] = prompt }
    let accounts = UsagePanelState()
    if let limits = fixture.limits, let snapshot = try? UsageSnapshot.decode(limits) {
        accounts.presentation = Presenter.panel(snapshot, now: fixture.now, pins: Set(snapshot.accounts.map(\.key)))
        accounts.checkedText = "Checked just now"
    }
    accounts.maximumHeight = height
    let view = ControlWindowView(store: store, state: state, accounts: accounts, interactive: false, solid: true)
        .frame(width: width, height: height)
        .environment(\.colorScheme, theme == "dark" ? .dark : .light)
        .environment(\.controlActiveState, .active)
        .environment(\.displayScale, scale)
    let hosting = NSHostingView(rootView: view)
    let rect = NSRect(x: 0, y: 0, width: width, height: height)
    let window = NSWindow(contentRect: rect, styleMask: .borderless, backing: .buffered, defer: false)
    window.isReleasedWhenClosed = false
    window.appearance = NSAppearance(named: theme == "dark" ? .darkAqua : .aqua)
    window.contentView = hosting
    hosting.frame = rect
    hosting.layoutSubtreeIfNeeded()
    // Let lazy stacks and scroll views lay out their first screen before drawing.
    RunLoop.current.run(until: Date().addingTimeInterval(0.25))
    hosting.layoutSubtreeIfNeeded()
    hosting.displayIfNeeded()
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int(rect.width * scale), pixelsHigh: Int(rect.height * scale),
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { throw OwnerError.failed }
    bitmap.size = rect.size
    hosting.cacheDisplay(in: hosting.bounds, to: bitmap)
    guard let png = bitmap.representation(using: .png, properties: [:]) else { throw OwnerError.failed }
    try png.write(to: URL(fileURLWithPath: output), options: .atomic)
    window.close()
}

/// Stands in when no `sb` is installed: every read and action says so in one line.
struct MissingCommandRunner: CommandRunning {
    func run(_ command: ControlCommand) async throws -> CommandResult { throw CommandFailure(OwnerError.missingExecutable.message) }
}
