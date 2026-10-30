import SwiftUI

/// Runs the __TEXT patch experiment (Lab/TextPatch.c) once at launch. Launch it through
/// Apparition's launchJIT so a debugger serves the requests; launched normally it only runs the
/// unpatched controls. Reports land in Documents/textpatch-*.json for devicectl to copy off.
@main
struct ApparitionLabApp: App {
    @State private var lines: [String] = []
    @State private var started = false

    var body: some Scene {
        WindowGroup {
            VStack(alignment: .leading, spacing: 12) {
                Text("Apparition Lab: __TEXT patch").font(.title2)
                ForEach(lines, id: \.self) { Text($0).font(.system(.body, design: .monospaced)) }
            }
            .padding(32)
            .task { await run() }
        }
    }

    private func run() async {
        guard !started else { return }
        started = true
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let swift = SwiftEntryExperiment.run(documents: documents)
        lines.append(swift)
        try? swift.write(to: documents.appendingPathComponent("swift-entry-report.json"),
                         atomically: true, encoding: .utf8)
        let optimized = Self.capture {
            optimized_patch_run(documents.appendingPathComponent("optimized-patch-steps.txt").path, $0, $1)
        }
        lines.append(optimized)
        try? optimized.write(to: documents.appendingPathComponent("optimized-patch-report.json"),
                             atomically: true, encoding: .utf8)
        let report = Self.capture { text_patch_run(documents.appendingPathComponent("textpatch-steps.txt").path, $0, $1) }
        lines.append(report)
        try? report.write(to: documents.appendingPathComponent("textpatch-report.json"), atomically: true, encoding: .utf8)

        // Patched pages are now private dirty copies; check they still run a while later.
        for delay in [10, 60] {
            try? await Task.sleep(for: .seconds(delay == 10 ? 10 : 50))
            let recheck = Self.capture { text_patch_recheck($0, $1) }
            lines.append("+\(delay)s \(recheck)")
            try? recheck.write(to: documents.appendingPathComponent("textpatch-recheck-\(delay)s.json"),
                               atomically: true, encoding: .utf8)
        }
    }

    private static func capture(_ body: (UnsafeMutablePointer<CChar>, Int) -> Void) -> String {
        var buffer = [CChar](repeating: 0, count: 1024)
        buffer.withUnsafeMutableBufferPointer { body($0.baseAddress!, $0.count) }
        return String(cString: buffer)
    }
}
