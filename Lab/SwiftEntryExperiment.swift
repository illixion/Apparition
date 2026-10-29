import Darwin
import Foundation

enum SwiftEntryExperiment {
    // Intentionally retain loaded images for the process lifetime. An entry or
    // an outstanding Swift value may still reference their code/metadata.
    private static var images: [UnsafeMutableRawPointer] = []
    private typealias Getter = @convention(c) () -> UnsafeRawPointer
    private typealias Scalar = @convention(thin) (UInt64) -> UInt64
    private typealias Text = @convention(thin) (String) -> String

    static func run(documents: URL) -> String {
        var report: [String: Any] = ["status": "not-traced"]
        let steps = documents.appendingPathComponent("swift-entry-steps.txt")
        func checkpoint(_ message: String) {
            // Atomic checkpoints survive a fault during an entry write/call.
            try? message.write(to: steps, atomically: true, encoding: .utf8)
        }
        func encode() -> String {
            let data = try! JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
            return String(decoding: data, as: UTF8.self)
        }
        guard lab_debugger_attached() == 1 else {
            report["scalar"] = labSwiftScalar(10)
            report["text"] = labSwiftText("hello")
            return encode()
        }
        let path = documents.appendingPathComponent("swift-entry-patch.dylib").path
        checkpoint("loading signed dylib")
        guard let handle = dlopen(path, RTLD_NOW | RTLD_LOCAL) else {
            report = ["status": "load-failed", "error": dlerror().map { String(cString: $0) } ?? "unknown"]
            return encode()
        }
        images.append(handle)
        guard let scalarSymbol = dlsym(handle, "lab_patch_scalar_address"),
              let textSymbol = dlsym(handle, "lab_patch_text_address") else {
            report = ["status": "symbols-missing"]
            return encode()
        }
        let scalarEntry = unsafeBitCast(labSwiftScalar as Scalar, to: UnsafeRawPointer.self)
        let textEntry = unsafeBitCast(labSwiftText as Text, to: UnsafeRawPointer.self)
        let scalarReplacement = unsafeBitCast(scalarSymbol, to: Getter.self)()
        let textReplacement = unsafeBitCast(textSymbol, to: Getter.self)()
        // The opaque C round trip prevents whole-module optimization from
        // substituting a known function or a constant-specialized clone here.
        let scalar = unsafeBitCast(lab_opaque_pointer(scalarEntry), to: Scalar.self)
        let text = unsafeBitCast(lab_opaque_pointer(textEntry), to: Text.self)
        var scalars: [UInt64] = [scalar(10)]
        var directScalars = [labSwiftScalar(lab_runtime_input())]
        var texts = [text("hello")]
        var writes: [Int32] = []
        checkpoint("patching native scalar entry")
        writes.append(lab_entry_patch(scalarEntry, scalarReplacement))
        scalars.append(scalar(10))
        directScalars.append(labSwiftScalar(lab_runtime_input()))
        checkpoint("restoring native scalar entry")
        writes.append(lab_entry_restore())
        scalars.append(scalar(10))
        directScalars.append(labSwiftScalar(lab_runtime_input()))
        // Never reuse the shared island if restoration failed.
        guard writes == [0, 0] else {
            report = ["status": "scalar-write-failed", "writes": writes, "scalar": scalars]
            return encode()
        }
        checkpoint("patching native String entry")
        writes.append(lab_entry_patch(textEntry, textReplacement))
        texts.append(text("hello"))
        // Exercise heap-backed Strings and ARC, not only small-string values.
        let longInput = String(repeating: "test", count: 256)
        var stringChecks = 0
        for _ in 0..<1000 {
            if text(longInput) == "replacement:" + longInput { stringChecks += 1 }
        }
        checkpoint("restoring native String entry")
        writes.append(lab_entry_restore())
        texts.append(text("hello"))
        let passed = writes == [0, 0, 0, 0] && scalars == [11, 17, 11] &&
            directScalars == [11, 17, 11] &&
            texts == ["original:hello", "replacement:hello", "original:hello"] && stringChecks == 1000
        report = ["status": passed ? "passed" : "failed", "writes": writes,
                  "scalar": scalars, "text": texts, "heapStringChecks": stringChecks,
                  "directScalar": directScalars,
                  "replacementDistance": Int(bitPattern: scalarReplacement) - Int(bitPattern: scalarEntry)]
        checkpoint("completed: " + encode())
        return encode()
    }
}
