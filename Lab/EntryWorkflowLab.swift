import DebugTrace

// A stable native Swift boundary. The rest of the module stays optimized;
// this explicit boundary has no inlined or specialized copies to go stale.
@_silgen_name("debugtrace_entry_workflow_scalar")
@inline(never)
@_optimize(none)
public func entryWorkflowScalar(_ value: Int) -> Int { value + 1 }

@MainActor
enum EntryWorkflowLab {
    static var reads = 0
    static func register() {
        DebugSurface.shared.register(.query("entryProbe", "Verify native entry patching and state retention", trace: .never) { _ in
            reads += 1
            return Probe(value: entryWorkflowScalar(10), reads: reads)
        })
    }
    struct Probe: Encodable { let value: Int; let reads: Int }
}
