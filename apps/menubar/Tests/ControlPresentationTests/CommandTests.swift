import Foundation
import Testing
@testable import ControlPresentation

@Test func everyCommandHasExactArguments() {
    #expect(Commands.status.arguments == ["ai", "status", "--json"])
    #expect(Commands.models.arguments == ["ai", "models", "--json"])
    #expect(Commands.engines.arguments == ["ai", "engines", "--json"])
    #expect(Commands.doctor().arguments == ["ai", "doctor", "--json"])
    #expect(Commands.doctor("model-a").arguments == ["ai", "doctor", "model-a", "--json"])
    #expect(Commands.settings("fast").arguments == ["ai", "settings", "fast", "--json"])
    #expect(Commands.history("text/model-a").arguments == ["ai", "settings", "text/model-a", "--history", "--json"])
    #expect(Commands.limits.arguments == ["limits", "--json"])
    #expect(Commands.set("text/model-a", [("temperature", .number(0.6)), ("reasoning.enabled", .bool(false))]).arguments
            == ["ai", "set", "text/model-a", "temperature=0.6", "reasoning.enabled=false"])
    #expect(Commands.setLoad("text/model-a", [("cache_type_k", .string("q8_0"))], apply: .now).arguments
            == ["ai", "set", "text/model-a", "cache_type_k=q8_0", "--load", "--apply", "now"])
    #expect(Commands.unset("text/model-a", ["top_p"]).arguments == ["ai", "unset", "text/model-a", "top_p"])
    #expect(Commands.unset("text/model-a", ["ctx_size"], load: true).arguments == ["ai", "unset", "text/model-a", "ctx_size", "--load"])
    #expect(Commands.revert("text/model-a", to: 2).arguments == ["ai", "revert", "text/model-a", "2"])
    #expect(Commands.load("text/model-a", wake: false).arguments == ["ai", "load", "text/model-a"])
    #expect(Commands.load("text/model-a", wake: true).arguments == ["ai", "load", "text/model-a", "--wake"])
    #expect(Commands.unload("text/model-a").arguments == ["ai", "unload", "text/model-a"])
    #expect(Commands.hold("1h", reason: "x y", machine: "gpu-box").arguments == ["ai", "hold", "1h", "x y", "--host", "gpu-box"])
    #expect(Commands.release("hold_1", machine: "gpu-box").arguments == ["ai", "release", "hold_1", "--host", "gpu-box"])
    #expect(Commands.on("1h", reason: "x", machine: "gpu-box").arguments == ["ai", "on", "1h", "x", "--host", "gpu-box"])
    #expect(Commands.off("gpu-box").arguments == ["ai", "off", "--host", "gpu-box"])
    #expect(Commands.chat("fast", prompt: "Say hi; then stop").arguments == ["ai", "chat", "fast", "Say hi; then stop", "--json"])
}

@Test func readsAreReadsAndWritesAreWrites() {
    for read in [Commands.status, Commands.models, Commands.engines, Commands.doctor(), Commands.settings("x"),
                 Commands.history("x"), Commands.limits] {
        #expect(!read.writes && read.isAllowed, "\(read.display)")
    }
    for write in [Commands.set("t", [("top_p", .number(1))]), Commands.unset("t", ["top_p"]), Commands.revert("t", to: 1),
                  Commands.load("t", wake: false), Commands.unload("t"), Commands.hold("1h", reason: "r", machine: "m"),
                  Commands.release("h", machine: "m"), Commands.on("1h", reason: "r", machine: "m"), Commands.off("m"),
                  Commands.chat("m", prompt: "p")] {
        #expect(write.writes && write.isAllowed, "\(write.display)")
    }
}

@Test func onlyKnownCommandsMayRun() {
    #expect(!ControlCommand(["ai", "tune", "model-a"], writes: true).isAllowed)
    #expect(!ControlCommand(["account", "login"], writes: true).isAllowed)
    #expect(!ControlCommand(["ai", "status", "--json"], writes: true).isAllowed)
    #expect(!ControlCommand(["ai", "off", "--host", "m"], writes: false).isAllowed)
    #expect(!ControlCommand([], writes: false).isAllowed)
}

@Test func theCopyableCommandIsQuotedForAShell() {
    #expect(Commands.off("gpu-box").display == "sb ai off --host gpu-box")
    #expect(Commands.chat("fast", prompt: "it's fine").display == #"sb ai chat fast 'it'\''s fine' --json"#)
    #expect(Commands.hold("1h", reason: "kept on from the Switchback window", machine: "gpu-box").display
            == "sb ai hold 1h 'kept on from the Switchback window' --host gpu-box")
    #expect(Commands.set("t", [("stop", .array([.string("\n")]))]).display == #"sb ai set t 'stop=["\n"]'"#)
}

@Test func valuesAreWrittenTheWaySbReadsThem() {
    #expect(JSONValue.number(0.6).argument == "0.6")
    #expect(JSONValue.number(32768).argument == "32768")
    #expect(JSONValue.bool(true).argument == "true")
    #expect(JSONValue.string("q8_0").argument == "q8_0")
    // A string that would read as JSON stays a string.
    #expect(JSONValue.string("16").argument == "\"16\"")
    #expect(JSONValue.numberText(0.1 + 0.2) == "0.3")
}

@Test func errorsAreOnePlainLineWithTheReasonSbGave() {
    let refused = CommandResult(status: 1, stdout: Data(), stderr:
        "sb ai: the gateway refused to power gpu-box off (uptime_too_short): the machine has been up 2m and needs at least 240 s\n")
    #expect(Outcome.errorLine(refused)
            == "The gateway refused to power gpu-box off (uptime_too_short): the machine has been up 2m and needs at least 240 s")
    let usage = CommandResult(status: 2, stdout: Data(), stderr: "sb ai: fast reaches 2 targets (text/a, mac/b); name one\n(see sb ai help)\n")
    #expect(Outcome.errorLine(usage) == "Fast reaches 2 models (text/a, mac/b); name one")
    #expect(Outcome.errorLine(CommandResult(status: 3, stdout: Data(), stderr: "")) == "sb stopped with exit code 3")
    let changed = CommandResult(status: 1, stdout: Data(), stderr: "sb ai: the settings changed meanwhile (revision 4 is current); run the command again\n")
    #expect(Outcome.errorLine(changed) == "The settings changed meanwhile (version 4 is current); run the command again")
}

@Test func successSaysWhatHappenedInFewWords() {
    func line(_ command: ControlCommand, _ stdout: String) -> String { Outcome.successLine(command, stdout: Data(stdout.utf8)) }
    #expect(line(Commands.set("t", [("top_p", .number(1))]), "text/model-a: revision 4\n") == "Saved · version 4")
    #expect(line(Commands.setLoad("t", [("ctx_size", .number(1))], apply: .nextLoad),
                 "text/model-a: revision 5\ntakes effect on the next load\n") == "Saved · version 5 · takes effect on the next load")
    #expect(line(Commands.revert("t", to: 2), "text/model-a: revision 6 (the settings of revision 2)\n")
            == "Back to version 2’s settings · now version 6")
    #expect(line(Commands.load("t", wake: false), "text/model-a: loaded (19.7 GiB on GPU at ctx 32768)\n")
            == "Loaded (19.7 GB on GPU at ctx 32768)")
    #expect(line(Commands.unload("t"), "text/model-a: unloaded\n") == "Unloaded")
    #expect(line(Commands.hold("2h", reason: "r", machine: "m"), "holding m on for 2.0 h: hold h1\n") == "Kept on for 2 h")
    #expect(line(Commands.on("1h", reason: "r", machine: "m"), "m is waking; holding it on for 60 min: hold h2\n") == "Waking · kept on for 1 h")
    #expect(line(Commands.on("1h", reason: "r", machine: "m"), "m is already up; holding it on\n") == "Kept on for 1 h")
    #expect(line(Commands.off("m"), "m is powering off\n") == "Powering off")
    #expect(line(Commands.off("m"), "m is already off\n") == "Already asleep")
    #expect(line(Commands.release("h", machine: "m"), "released hold h on m\n") == "Released")
}
