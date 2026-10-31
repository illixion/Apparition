import DebugTrace

// A stable native Swift boundary. The rest of the module stays optimized;
// this explicit boundary has no inlined or specialized copies to go stale.
@_silgen_name("debugtrace_entry_workflow_scalar")
@inline(never)
@_optimize(none)
public func entryWorkflowScalar(_ value: Int) -> Int { value + 1 }

public struct EntryWorkflowValue {
    public var total = 2

    @_silgen_name("debugtrace_entry_workflow_value")
    @inline(never)
    @_optimize(none)
    public func apply(_ value: Int) -> Int { value + total }

    @_silgen_name("debugtrace_entry_workflow_mutating")
    @inline(never)
    @_optimize(none)
    public mutating func accumulate(_ value: Int) -> Int {
        total += value
        return total
    }

    @_silgen_name("debugtrace_entry_workflow_static")
    @inline(never)
    @_optimize(none)
    public static func scale(_ value: Int) -> Int { value * 2 }
}

@MainActor
enum EntryWorkflowLab {
    static var reads = 0
    static var receiver = EntryWorkflowValue()
    static func register() {
        DebugSurface.shared.register(.query("entryProbe", "Verify native entry patching and state retention", trace: .never) { _ in
            reads += 1
            return Probe(value: entryWorkflowScalar(10), reads: reads, method: receiver.apply(10),
                         accumulated: receiver.accumulate(1), scaled: EntryWorkflowValue.scale(10))
        })
    }
    struct Probe: Encodable { let value: Int; let reads: Int; let method: Int; let accumulated: Int; let scaled: Int }
}
