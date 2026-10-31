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
        let slot = EntryWorkflowStateSlot.slot
        let advance: (EntryWorkflowStateV3) throws -> EntryWorkflowStateV3 = {
            var candidate = $0
            candidate.total += delta
            candidate.history.append(delta)
            return candidate
        }
        let validate: (EntryWorkflowStateV3) throws -> Void = { _ in
            guard delta >= 0 else {
                throw DebugError(.invalidArgument, "negative delta", hint: "use a nonnegative lab delta")
            }
        }
        let value: EntryWorkflowStateV3
        if slot.generation == "v3" {
            value = try slot.update(generation: "v3", as: EntryWorkflowStateV3.self,
                                    transform: advance, validate: validate)
        } else {
            value = try slot.migrate(from: "v2", as: EntryWorkflowStateV2.self, to: "v3",
                transform: { try advance(EntryWorkflowStateV3(total: $0.total, history: [$0.steps])) }, validate: validate)
        }
        return value.total
    } catch let error as DebugError where error.code == .invalidArgument { return -2 }
    catch { return -1 }
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
