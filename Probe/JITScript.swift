import DebugTrace
import Foundation
import JavaScriptCore

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "JITScript")

/// Runs a StikDebug JIT script (an `enable-jit` link's `script-data`, as LiveContainer sends for
/// an app given one) against a debugserver session from `apparition_jit_script_open`. The
/// script is the app's debugger from then on: it attaches, continues, and answers the app's
/// breakpoints itself, until it returns.
///
/// It sees StikDebug's functions: `get_pid()`, `send_command(packet)` (one gdb-remote packet,
/// unframed; returns the reply), `prepare_memory_region(address, length)`, `log(…)`, `hasTXM()`,
/// and `resume_app()` (the app is already back in front, so it only answers "OK").
/// `take_screenshot` throws: Apparition has no screenshot service.
enum JITScript {
    struct Outcome {
        /// The script's own `log` lines, at most `lineBudget` of them.
        var lines: [String] = []
        var droppedLines = 0
        /// What the script threw, if it threw.
        var exception: String?
    }

    private static let lineBudget = 200

    /// Runs `source` to the end on this thread. `session` stays the caller's to close.
    static func run(_ source: String, session: OpaquePointer, pid: Int) -> Outcome {
        guard let context = JSContext() else { return Outcome(exception: "JavaScriptCore didn't start") }
        var outcome = Outcome()
        let record = { (line: String) in
            if outcome.lines.count < lineBudget { outcome.lines.append(line) } else { outcome.droppedLines += 1 }
        }
        func raise(_ message: String) {
            JSContext.current()?.exception = JSValue(newErrorFromMessage: message, in: JSContext.current())
        }
        /// An address or a length: scripts pass BigInts (`0x1000n`) as often as Numbers.
        func unsigned(_ value: JSValue) -> UInt64? {
            if value.isBigInt { return value.toUInt64() }
            guard value.isNumber, let number = value.toNumber()?.doubleValue, number >= 0, number < 0x1p64 else { return nil }
            return UInt64(number)
        }

        let getPid: @convention(block) () -> Int = { pid }
        let sendCommand: @convention(block) (JSValue) -> String = { packet in
            guard packet.isString, let packet = packet.toString() else {
                raise("send_command needs a packet")
                return ""
            }
            let reply = AccountModel.call(apparition_jit_script_send(session, packet))
            guard reply["ok"] as? Bool == true else {
                raise(reply["error"] as? String ?? "send_command failed")
                return ""
            }
            return reply["reply"] as? String ?? ""
        }
        let prepareRegion: @convention(block) (JSValue, JSValue) -> String = { address, length in
            guard let address = unsigned(address), let length = unsigned(length) else {
                raise("prepare_memory_region needs an address and a length")
                return ""
            }
            let reply = AccountModel.call(apparition_jit_script_prepare(session, address, length))
            guard reply["ok"] as? Bool == true else {
                raise(reply["error"] as? String ?? "prepare_memory_region failed")
                return ""
            }
            return "OK"
        }
        // log('Script failed:', name, message) is common: keep every argument, as console.log does.
        let logLine: @convention(block) () -> Void = {
            let parts = (JSContext.currentArguments() as? [JSValue] ?? []).map { $0.toString() ?? "" }
            record(parts.joined(separator: " "))
        }
        // Only devices with TXM need a script at all, and the request is for one.
        let hasTXM: @convention(block) () -> Bool = { true }
        let resumeApp: @convention(block) () -> String = { "OK" }
        let takeScreenshot: @convention(block) () -> String = {
            raise("take_screenshot isn't available in Apparition")
            return ""
        }

        context.exceptionHandler = { _, exception in
            outcome.exception = exception?.toString() ?? "exception"
        }
        context.setObject(getPid, forKeyedSubscript: "get_pid" as NSString)
        context.setObject(sendCommand, forKeyedSubscript: "send_command" as NSString)
        context.setObject(prepareRegion, forKeyedSubscript: "prepare_memory_region" as NSString)
        context.setObject(logLine, forKeyedSubscript: "log" as NSString)
        context.setObject(hasTXM, forKeyedSubscript: "hasTXM" as NSString)
        context.setObject(resumeApp, forKeyedSubscript: "resume_app" as NSString)
        context.setObject(takeScreenshot, forKeyedSubscript: "take_screenshot" as NSString)

        log.info("jit script start: \(source.utf8.count, privacy: .public) bytes")
        context.evaluateScript(source)
        log.info("jit script end: \(outcome.lines.count + outcome.droppedLines, privacy: .public) lines, \(outcome.exception == nil ? "returned" : "threw", privacy: .public)")
        return outcome
    }
}
