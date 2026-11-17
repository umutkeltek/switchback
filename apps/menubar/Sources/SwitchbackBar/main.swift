import AppKit
import SwiftUI
import UsagePresentation

@MainActor final class BarDelegate: NSObject, NSApplicationDelegate, NSPopoverDelegate {
    private var item: NSStatusItem?
    private var snapshot: UsageSnapshot?
    private var timer: Timer?
    private var task: Task<Void, Never>?
    private var popover: NSPopover?
    private var outsideDismissals = 0
    private lazy var dismissal = PopoverDismissal { [weak self] in
        guard let self else { return }
        self.outsideDismissals += 1
        self.closePanel()
    }
    private var busy = false
    private var failure: String?
    private var lastChecked: Date?
    private let defaults = UserDefaults.standard
    private let panel = UsagePanelState()
    private var pins: Set<String>? {
        defaults.stringArray(forKey: "pinnedAccountKeys").map(Set.init)
    }
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        defaults.register(defaults: ["menuBarIconOnly": true])
        panel.selectedProvider = defaults.string(forKey: "selectedProvider") ?? "all"
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item?.autosaveName = "SwitchbackBar"
        item?.isVisible = true
        if let button = item?.button {
            let icon = NSImage(systemSymbolName: "arrow.triangle.swap", accessibilityDescription: "Switchback")?
                .withSymbolConfiguration(.init(pointSize: 13, weight: .medium))
            icon?.size = NSSize(width: 14, height: 14)
            icon?.isTemplate = true
            button.image = icon
            button.imagePosition = .imageLeading
            button.font = .monospacedDigitSystemFont(ofSize: 12, weight: .medium)
            button.target = self
            button.action = #selector(togglePanel(_:))
            _ = button.sendAction(on: [.leftMouseUp, .rightMouseUp])
            button.setAccessibilityLabel("Switchback account limits")
        }
        panel.onPin = { [weak self] in self?.togglePin($0) }
        panel.onRefresh = { [weak self] in self?.forceRefresh() }
        panel.onQuit = { NSApp.terminate(nil) }
        panel.onClose = { [weak self] in self?.closePanel() }
        panel.onResize = { [weak self] height in
            self?.popover?.contentSize = NSSize(width: PanelMetrics.width, height: height)
        }
        panel.onProviderChange = { [weak self] provider in
            self?.defaults.set(provider, forKey: "selectedProvider")
        }
        panel.onIconOnly = { [weak self] value in
            self?.defaults.set(value, forKey: "menuBarIconOnly")
            self?.rebuild()
        }
        panel.onLimitNotices = { [weak self] value in
            guard let self else { return }
            self.defaults.set(value, forKey: "limitNoticesEnabled")
            self.defaults.removeObject(forKey: "limitNoticeLevels")
            if value { LimitNotifier.requestPermission() }
            self.notifyLimits()
            self.rebuild()
        }
        rebuild()
        task = Task {
            await refresh(.saved)
            await refresh(.scheduled)
        }
        timer = Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { [weak self] _ in
            Task { @MainActor in
                guard let self else { return }
                self.rebuild()
                guard !self.busy else { return }
                self.task = Task { await self.refresh(.scheduled) }
            }
        }
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        dismissal.stop()
        timer?.invalidate()
        guard let task else { return .terminateNow }
        task.cancel()
        Task {
            await task.value
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
    func applicationWillTerminate(_ notification: Notification) {
        dismissal.stop()
        timer?.invalidate()
        task?.cancel()
    }
    private func rebuild() {
        panel.busy = busy
        panel.failure = failure
        panel.iconOnly = defaults.bool(forKey: "menuBarIconOnly")
        panel.limitNotices = defaults.bool(forKey: "limitNoticesEnabled")
        panel.style = PanelStyle(preference: defaults.string(forKey: PanelStyle.preferenceKey))
        var fullTitle = "Switchback account limits"
        var title = ""
        if let snapshot {
            let now = Date()
            let presentation = Presenter.make(snapshot, now: now, pins: pins, refreshing: busy, failure: failure)
            panel.presentation = Presenter.panel(snapshot, now: now, pins: pins, refreshing: busy, failure: failure)
            fullTitle = presentation.title
            if let advice = Presenter.bestSeat(snapshot, now: now, failure: failure) {
                fullTitle += "\n" + advice.tooltipLine
            }
            if !panel.iconOnly {
                let fitted = Presenter.statusTitle(presentation, maxWidth: PanelMetrics.statusTextWidth) { text in
                    (text as NSString).size(withAttributes: [.font: self.item?.button?.font ?? NSFont.menuBarFont(ofSize: 0)]).width
                }
                title = fitted.hasPrefix("SB ") ? String(fitted.dropFirst(3)) : ""
            }
        }
        if let lastChecked {
            let minutes = max(0, Int(Date().timeIntervalSince(lastChecked) / 60))
            panel.checkedText = minutes == 0 ? "Checked just now" : "Checked \(minutes)m ago"
        }
        item?.length = panel.iconOnly ? PanelMetrics.statusIconWidth : NSStatusItem.variableLength
        item?.button?.imagePosition = panel.iconOnly ? .imageOnly : .imageLeading
        item?.button?.title = title
        item?.button?.toolTip = fullTitle
        recordStatusItem()
    }
    private func recordStatusItem() {
        guard let item, let button = item.button else { return }
        let width = (button.title as NSString).size(withAttributes: [.font: button.font ?? NSFont.menuBarFont(ofSize: 0)]).width
        defaults.set(["pid": Int(ProcessInfo.processInfo.processIdentifier),
                      "sampled_at": ISO8601DateFormatter().string(from: Date()),
                      "item_created": true, "item_enabled": item.isVisible,
                      "title": button.title, "title_width": width, "title_width_limit": PanelMetrics.statusTextWidth,
                      "item_width": button.bounds.width, "menu_bar_mode": panel.iconOnly ? "icon" : "readings",
                      "backing_scale": button.window?.backingScaleFactor ?? 1,
                      "panel_width": PanelMetrics.width, "ui_revision": "compact-3-dismissal",
                      "panel_open": popover?.isShown ?? false,
                      "outside_click_monitor_active": dismissal.isMonitoring,
                      "outside_dismissals": outsideDismissals, "ui": "account-panel"],
                     forKey: "statusItemDiagnostics")
    }
    @objc private func togglePanel(_ sender: Any?) {
        guard let button = item?.button else { return }
        if let event = NSApp.currentEvent, event.type == .rightMouseUp {
            closePanel()
            let menu = NSMenu()
            let refresh = NSMenuItem(title: "Refresh usage", action: #selector(forceRefresh), keyEquivalent: "r")
            refresh.target = self; refresh.isEnabled = !busy
            menu.addItem(refresh); menu.addItem(.separator())
            let quit = NSMenuItem(title: "Quit Switchback", action: #selector(quit), keyEquivalent: "q")
            quit.target = self; menu.addItem(quit)
            NSMenu.popUpContextMenu(menu, with: event, for: button)
            return
        }
        if popover?.isShown == true {
            closePanel()
        } else {
            rebuild()
            panel.query = ""
            if popover == nil {
                let popup = NSPopover()
                popup.behavior = .transient
                popup.animates = false
                popup.delegate = self
                popup.contentSize = NSSize(width: PanelMetrics.width, height: panel.preferredHeight)
                popup.contentViewController = NSHostingController(rootView: UsagePanelView(state: panel))
                popover = popup
            }
            let screenHeight: CGFloat = (button.window?.screen ?? NSScreen.main)?.visibleFrame.height ?? 800
            panel.maximumHeight = min(PanelMetrics.maximumHeight, max(PanelMetrics.minimumHeight, Double(screenHeight) - 80))
            popover?.contentSize = NSSize(width: PanelMetrics.width, height: panel.preferredHeight)
            popover?.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            // This entry follows a user action; activate for native focus and transient dismissal.
            NSApp.activate()
            if popover?.isShown == true { dismissal.start() }
            popover?.contentViewController?.view.window?.makeKey()
            panel.focusSearchRequest += 1
        }
        recordStatusItem()
    }
    private func closePanel() {
        dismissal.stop()
        popover?.performClose(nil)
        recordStatusItem()
    }
    func popoverDidClose(_ notification: Notification) {
        dismissal.stop()
        recordStatusItem()
    }
    private func togglePin(_ key: String) {
        guard let snapshot, snapshot.accounts.contains(where: { $0.key == key }) else { return }
        var updated = pins ?? Presenter.defaults(snapshot)
        if updated.contains(key) { updated.remove(key) } else { updated.insert(key) }
        defaults.set(updated.sorted(), forKey: "pinnedAccountKeys")
        rebuild()
    }
    private func refresh(_ request: RefreshRequest) async {
        guard !busy else { return }
        busy = true; rebuild()
        defer { busy = false; rebuild() }
        guard let executable = OwnerRunner.executable() else {
            failure = OwnerError.missingExecutable.message
            return
        }
        do {
            let data = try await OwnerRunner.run(request, executable: executable)
            snapshot = try UsageSnapshot.decode(data)
            lastChecked = Date()
            failure = nil
            notifyLimits()
        } catch let error as OwnerError {
            failure = error.message
        } catch {
            failure = "Switchback usage response is unreadable"
        }
    }
    /// Remembers the last notified level per account window so each crossing notifies once.
    private func notifyLimits() {
        guard let snapshot else { return }
        let previous = defaults.dictionary(forKey: "limitNoticeLevels") as? [String: Int] ?? [:]
        let result = Presenter.limitNotices(snapshot, pins: pins, enabled: defaults.bool(forKey: "limitNoticesEnabled"),
                                            previous: previous, failure: failure)
        defaults.set(result.levels, forKey: "limitNoticeLevels")
        LimitNotifier.post(result.notices)
    }
    @objc private func forceRefresh() {
        guard !busy else { return }
        task = Task { await refresh(.forced) }
    }
    @objc private func quit() { NSApp.terminate(nil) }
}

@MainActor func renderPanel(_ snapshotPath: String, output: String, theme: String, provider: String, scale: Double = 2, reduceTransparency: Bool = false, style: PanelStyle = .ledger) throws {
    let app = NSApplication.shared
    app.setActivationPolicy(.prohibited)
    guard scale == 1 || scale == 2 else { throw OwnerError.failed }
    let data = try Data(contentsOf: URL(fileURLWithPath: snapshotPath))
    guard data.count <= 1_048_576 else { throw OwnerError.outputTooLarge }
    let snapshot = try UsageSnapshot.decode(data)
    let state = UsagePanelState()
    state.presentation = Presenter.panel(snapshot, pins: Set(snapshot.accounts.map(\.key)))
    state.checkedText = "Checked just now"
    state.selectedProvider = provider
    state.style = style
    let view = UsagePanelView(state: state, interactive: false, solidBackground: reduceTransparency)
        .environment(\.colorScheme, theme == "dark" ? .dark : .light)
        .environment(\.controlActiveState, .active)
        .environment(\.displayScale, scale)
    let hosting = NSHostingView(rootView: view)
    let rect = NSRect(x: 0, y: 0, width: PanelMetrics.width, height: state.preferredHeight)
    let window = NSWindow(contentRect: rect, styleMask: .borderless, backing: .buffered, defer: false)
    window.isReleasedWhenClosed = false
    window.appearance = NSAppearance(named: theme == "dark" ? .darkAqua : .aqua)
    window.contentView = hosting
    hosting.frame = rect
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

let arguments = CommandLine.arguments
if let index = arguments.firstIndex(of: "--render-panel"), arguments.indices.contains(index + 1) {
    func value(_ key: String, fallback: String) -> String {
        guard let i = arguments.firstIndex(of: key), arguments.indices.contains(i + 1) else { return fallback }
        return arguments[i + 1]
    }
    do {
        let output = value("--output", fallback: "")
        guard !output.isEmpty else { throw OwnerError.failed }
        let styleFlag = value("--style", fallback: "")
        guard styleFlag.isEmpty || PanelStyle(rawValue: styleFlag) != nil else { throw OwnerError.failed }
        let style = PanelStyle(preference: styleFlag)
        try renderPanel(arguments[index + 1], output: output,
                        theme: value("--theme", fallback: "light"), provider: value("--provider", fallback: "all"),
                        scale: Double(value("--scale", fallback: "2")) ?? -1,
                        reduceTransparency: arguments.contains("--reduce-transparency"), style: style)
    } catch {
        FileHandle.standardError.write(Data("Could not render account panel\n".utf8)); exit(1)
    }
} else if arguments.contains("--version") {
    print("SwitchbackBar 1.0.0")
} else if let index = arguments.firstIndex(of: "--print-menu"), arguments.indices.contains(index + 1) {
    do {
        let data = try Data(contentsOf: URL(fileURLWithPath: arguments[index + 1]), options: .mappedIfSafe)
        guard data.count <= 1_048_576 else { throw OwnerError.outputTooLarge }
        let snapshot = try UsageSnapshot.decode(data)
        let menu = Presenter.make(snapshot)
        if arguments.contains("--text") {
            print(Presenter.text(menu))
        } else {
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            FileHandle.standardOutput.write(try encoder.encode(menu))
            print("")
        }
    } catch {
        FileHandle.standardError.write(Data("Could not present usage snapshot\n".utf8))
        exit(1)
    }
} else if arguments.count != 1 {
    FileHandle.standardError.write(Data("Usage: SwitchbackBar [--version | --print-menu snapshot.json [--text] | --render-panel snapshot.json --output panel.png [--theme light|dark] [--provider PROVIDER] [--scale 1|2] [--reduce-transparency] [--style a|cards]]\n".utf8))
    exit(2)
} else {
    let application = NSApplication.shared
    let delegate = BarDelegate()
    application.delegate = delegate
    withExtendedLifetime(delegate) { application.run() }
}
