#!/usr/bin/env python3
"""Native dismissal regression without posting input or showing windows on the desktop."""
import os
from pathlib import Path
import subprocess
import tempfile

HARNESS = r"""
import AppKit

@MainActor final class MouseEvents {
    struct Entry { let mask: NSEvent.EventTypeMask; let handler: PopoverDismissal.MouseHandler }
    var entries: [UUID: Entry] = [:]
    var removals = 0
    func install(_ mask: NSEvent.EventTypeMask, _ handler: @escaping PopoverDismissal.MouseHandler) -> Any? {
        let id = UUID(); entries[id] = Entry(mask: mask, handler: handler); return id
    }
    func remove(_ token: Any) { entries.removeValue(forKey: token as! UUID); removals += 1 }
    func emit(_ type: NSEvent.EventType) {
        let mask = NSEvent.EventTypeMask(rawValue: 1 << type.rawValue)
        let event = NSEvent.mouseEvent(with: type, location: .zero, modifierFlags: [],
                                      timestamp: 0, windowNumber: 0, context: nil,
                                      eventNumber: 1, clickCount: 1, pressure: 1)!
        for entry in Array(entries.values) where !entry.mask.intersection(mask).isEmpty { entry.handler(event) }
    }
}
@main struct DismissalTests {
    @MainActor static var failures = 0
    @MainActor static func check(_ value: Bool, _ name: String) {
        print("\(value ? "PASS" : "FAIL") \(name)")
        if !value { failures += 1 }
    }
    @MainActor static func pump() {
        let until = Date().addingTimeInterval(0.06)
        while Date() < until { RunLoop.current.run(mode: .default, before: until) }
    }
    @MainActor static func main() {
        NSApplication.shared.setActivationPolicy(.prohibited)
        let mouse = MouseEvents()
        let notifications = NotificationCenter()
        var closes = 0
        let dismissal = PopoverDismissal(installMouseMonitor: mouse.install,
            removeMouseMonitor: mouse.remove, notifications: notifications) { closes += 1 }
        dismissal.start()
        check(dismissal.isMonitoring && mouse.entries.count == 1, "outside mouse observation starts when opened")
        mouse.emit(.leftMouseDown); pump()
        check(closes == 1, "outside left click dismisses an inactive panel")
        check(!dismissal.isMonitoring && mouse.entries.isEmpty, "dismissal removes its mouse observation")
        for type: NSEvent.EventType in [.rightMouseDown, .otherMouseDown] {
            let before = closes
            dismissal.start(); mouse.emit(type); pump()
            check(closes == before + 1, "\(type) dismisses after reopening")
        }
        dismissal.start()
        let beforeOwnActivation = closes
        notifications.post(name: NSWorkspace.didActivateApplicationNotification, object: nil,
                           userInfo: [NSWorkspace.applicationUserInfoKey: NSRunningApplication.current])
        pump()
        check(closes == beforeOwnActivation && dismissal.isMonitoring, "the app's own activation preserves inside interaction")
        mouse.emit(.leftMouseDown)
        dismissal.stop()
        dismissal.start()
        pump()
        check(closes == beforeOwnActivation && dismissal.isMonitoring, "queued input from an old opening cannot close the new opening")
        dismissal.start()
        check(mouse.entries.count == 1, "repeated start keeps exactly one mouse observer")
        dismissal.stop()
        check(mouse.entries.isEmpty && !dismissal.isMonitoring, "explicit close removes observers")
        let otherNotifications = NotificationCenter()
        let switcher = PopoverDismissal(installMouseMonitor: mouse.install,
            removeMouseMonitor: mouse.remove, notifications: otherNotifications,
            processIdentifier: -1) { closes += 1 }
        switcher.start()
        let beforeSwitch = closes
        otherNotifications.post(name: NSWorkspace.didActivateApplicationNotification, object: nil,
                               userInfo: [NSWorkspace.applicationUserInfoKey: NSRunningApplication.current])
        pump()
        check(closes == beforeSwitch + 1 && !switcher.isMonitoring, "switching to another application dismisses")
        switcher.start(); switcher.stop()
        let beforeStopped = closes
        otherNotifications.post(name: NSWorkspace.didActivateApplicationNotification, object: nil,
                               userInfo: [NSWorkspace.applicationUserInfoKey: NSRunningApplication.current])
        pump()
        check(closes == beforeStopped, "stopped application observation cannot dismiss again")
        let live = PopoverDismissal {}
        live.start()
        check(live.isMonitoring, "native global mouse registration succeeds without activation")
        live.stop()
        check(!live.isMonitoring, "native global mouse registration stops")
        exit(failures == 0 ? 0 : 1)
    }
}
"""
def main():
    scratch = os.environ.get("SWITCHBACK_MENUBAR_TEST_SCRATCH")
    if not scratch or not Path(scratch).is_absolute():
        raise SystemExit("Set SWITCHBACK_MENUBAR_TEST_SCRATCH to an external test directory")
    Path(scratch).mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="popover-dismissal-", dir=scratch))
    harness = work / "DismissalTests.swift"
    harness.write_text(HARNESS)
    source = Path(__file__).parents[1] / "Sources/SwitchbackBar/PopoverDismissal.swift"
    executable = work / "dismissal-tests"
    subprocess.run(["swiftc", "-swift-version", "6", "-default-isolation", "MainActor", "-parse-as-library", str(source), str(harness), "-o", str(executable)], check=True)
    raise SystemExit(subprocess.run([str(executable)], timeout=15).returncode)

if __name__ == "__main__":
    main()
