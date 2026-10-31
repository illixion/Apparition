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
    var candidate: EntryWorkflowStateV3
    if let current = EntryWorkflowStateSlot.value as? EntryWorkflowStateV3 {
        candidate = current
    } else if let previous = EntryWorkflowStateSlot.value as? EntryWorkflowStateV2 {
        candidate = EntryWorkflowStateV3(total: previous.total, history: [previous.steps])
    } else { return -1 }
    guard delta >= 0 else { return -2 }
    candidate.total += delta
    candidate.history.append(delta)
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
