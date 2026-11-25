import SwiftUI

/// The smallest app that needs JIT (JITProbe.c): launched with JIT, by Apparition or inside
/// LiveContainer with or without StikDebug's universal.js, it says "JIT works".
@main
struct JITProbeApp: App {
    @State private var lines: [String] = []
    @State private var started = ProcessInfo.processInfo.systemUptime

    var body: some Scene {
        WindowGroup {
            VStack(alignment: .leading, spacing: 12) {
                Text("JIT Probe").font(.title2)
                Text("PID \(getpid())").font(.caption.monospaced())
                ScrollView {
                    ForEach(Array(lines.reversed().enumerated()), id: \.offset) { _, line in
                        Text(line).font(.system(.footnote, design: .monospaced))
                    }
                }
                Button("Run Existing Arena") { run(fresh: false) }
                Button("Request Fresh Arena") { run(fresh: true) }
            }
            .padding(32)
            .onAppear { if lines.isEmpty { run(fresh: false) } }
        }
    }

    private func run(fresh: Bool) {
        let label = fresh ? "Fresh arena" : "Retained arena"
        let elapsed = Int(ProcessInfo.processInfo.systemUptime - started)
        var buffer = [CChar](repeating: 0, count: 512)
        let result = buffer.withUnsafeMutableBufferPointer {
            if fresh { return jit_probe_fresh($0.baseAddress!, $0.count) }
            return jit_probe_run($0.baseAddress!, $0.count)
        }
        let line = "[+\(elapsed)s] \(label) · \(String(cString: buffer))"
        NSLog("JITProbe: %@ (%d)", line, result)
        lines.append(line)
    }
}
