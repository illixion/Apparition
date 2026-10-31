import DebugTrace

// Stable storage and a scalar boundary for explicit state-generation experiments.
// Every access is synchronous on the main actor; payloads are value snapshots.
public struct EntryWorkflowStateV1 {
    public var total: Int
}

@MainActor
public enum EntryWorkflowStateSlot {
    public static let slot = try! DebugPatchStateSlot(generation: "v1", value: EntryWorkflowStateV1(total: 10))
}

@MainActor
@_silgen_name("debugtrace_entry_workflow_state")
@inline(never)
@_optimize(none)
public func entryWorkflowState(_ delta: Int) -> Int {
    do {
        let value = try EntryWorkflowStateSlot.slot.update(generation: "v1", as: EntryWorkflowStateV1.self) {
            var candidate = $0
            candidate.total += delta
            return candidate
        }
        return value.total
    } catch { return -1 }
}

public struct EntryWorkflowStateV2 {
    public var total: Int
    public var steps: Int
    public var label: String
}

public struct EntryWorkflowStateV3 {
    public var total: Int
    public var history: [Int]
}
