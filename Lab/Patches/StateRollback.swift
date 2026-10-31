// Stable storage and a scalar boundary for explicit state-generation experiments.
// Every access is synchronous on the main actor; payloads are value snapshots.
public struct EntryWorkflowStateV1 {
    public var total: Int
}

@MainActor
public enum EntryWorkflowStateSlot {
    public static var value: Any = EntryWorkflowStateV1(total: 10)
}

@MainActor
@_silgen_name("debugtrace_entry_workflow_state")
@inline(never)
@_optimize(none)
public func entryWorkflowState(_ delta: Int) -> Int {
    // Older code must refuse an unknown generation without destroying its state.
    guard var candidate = EntryWorkflowStateSlot.value as? EntryWorkflowStateV1 else { return -1 }
    candidate.total += delta
    EntryWorkflowStateSlot.value = candidate
    return candidate.total
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
