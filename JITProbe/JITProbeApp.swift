import SwiftUI

/// The smallest app that needs JIT (JITProbe.c): launched with JIT, by Apparition or inside
/// LiveContainer with or without StikDebug's universal.js, it says "JIT works".
@main
struct JITProbeApp: App {
    @State private var lines: [String] = []

    var body: some Scene {
        WindowGroup {
            VStack(alignment: .leading, spacing: 12) {
                Text("JIT Probe").font(.title2)
                ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                    Text(line).font(.system(.body, design: .monospaced))
                }
                Button("Run Again") { run() }
            }
            .padding(32)
            .onAppear { if lines.isEmpty { run() } }
        }
    }

    private func run() {
        var buffer = [CChar](repeating: 0, count: 512)
        let result = buffer.withUnsafeMutableBufferPointer { jit_probe_run($0.baseAddress!, $0.count) }
        let line = String(cString: buffer)
        NSLog("JITProbe: %@ (%d)", line, result)
        lines.append(line)
    }
}
