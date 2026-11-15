import AppKit
import UsagePresentation

@MainActor final class BarDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private var item: NSStatusItem?
    private var snapshot: UsageSnapshot?
    private var timer: Timer?
    private var task: Task<Void, Never>?
    private var busy = false
    private var failure: String?
    private let defaults = UserDefaults.standard
    private var pins: Set<String>? {
        defaults.array(forKey: "pinnedAccountKeys").map { Set($0.compactMap { $0 as? String }) }
    }
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
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
        timer?.invalidate()
        task?.cancel()
    }
    func menuWillOpen(_ menu: NSMenu) { rebuild(existingMenu: menu) }
    private func row(_ text: String, action: Selector? = nil) -> NSMenuItem {
        let display = text.count > 110 ? String(text.prefix(107)) + "…" : text
        let row = NSMenuItem(title: display, action: action, keyEquivalent: "")
        row.toolTip = text
        if action != nil { row.target = self }
        return row
    }
    private func rebuild(existingMenu: NSMenu? = nil) {
        let menu = existingMenu ?? NSMenu()
        menu.removeAllItems()
        menu.delegate = self
        var fullTitle = "Switchback ?"
        var visibleTitle = fullTitle
        if let snapshot {
            let presentation = Presenter.make(snapshot, pins: pins, refreshing: busy, failure: failure)
            fullTitle = presentation.title
            visibleTitle = presentation.compactTitle
            menu.addItem(row(presentation.status))
            for group in presentation.groups {
                let provider = row(group.title)
                let submenu = NSMenu()
                if let unavailable = group.unavailable { submenu.addItem(row(unavailable)) }
                for account in group.accounts {
                    let accountRow = row(account.title + " · " + account.summary)
                    let accountMenu = NSMenu()
                    let toggle = row("Pin in menu bar", action: #selector(togglePin(_:)))
                    toggle.representedObject = account.key
                    toggle.state = account.pinned ? .on : .off
                    accountMenu.addItem(toggle)
                    accountMenu.addItem(.separator())
                    for line in account.lines { accountMenu.addItem(row(line)) }
                    let details = row("Details")
                    let detailMenu = NSMenu()
                    for line in account.details { detailMenu.addItem(row(line)) }
                    details.submenu = detailMenu
                    accountMenu.addItem(details)
                    accountRow.submenu = accountMenu
                    submenu.addItem(accountRow)
                }
                provider.submenu = submenu
                menu.addItem(provider)
            }
        } else {
            menu.addItem(row(busy ? "Reading account limits…" : failure ?? "Account limits unavailable"))
        }
        menu.addItem(.separator())
        let refresh = row(busy ? "Refresh in progress…" : "Refresh now", action: #selector(forceRefresh))
        refresh.isEnabled = !busy
        menu.addItem(refresh)
        menu.addItem(row("Quit Switchback Bar", action: #selector(quit)))
        item?.button?.title = visibleTitle
        item?.button?.toolTip = fullTitle
        item?.menu = menu
    }
    private func refresh(_ request: RefreshRequest) async {
        guard !busy else { return }
        busy = true
        rebuild()
        defer { busy = false; rebuild() }
        guard let executable = OwnerRunner.executable() else {
            failure = OwnerError.missingExecutable.message
            return
        }
        do {
            let data = try await OwnerRunner.run(request, executable: executable)
            snapshot = try UsageSnapshot.decode(data)
            failure = nil
        } catch let error as OwnerError {
            failure = error.message
        } catch {
            failure = "Switchback usage response is unreadable"
        }
    }
    @objc private func togglePin(_ sender: NSMenuItem) {
        guard let key = sender.representedObject as? String, let snapshot else { return }
        var updated = pins ?? Presenter.defaults(snapshot)
        if updated.contains(key) { updated.remove(key) } else { updated.insert(key) }
        defaults.set(updated.sorted(), forKey: "pinnedAccountKeys")
        rebuild()
    }
    @objc private func forceRefresh() {
        guard !busy else { return }
        task = Task { await refresh(.forced) }
    }
    @objc private func quit() { NSApp.terminate(nil) }
}
let arguments = CommandLine.arguments
if arguments.contains("--version") {
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
    FileHandle.standardError.write(Data("Usage: SwitchbackBar [--version | --print-menu snapshot.json [--text]]\n".utf8))
    exit(2)
} else {
    let application = NSApplication.shared
    let delegate = BarDelegate()
    application.delegate = delegate
    application.run()
}
