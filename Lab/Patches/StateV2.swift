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
        let advance: (EntryWorkflowStateV2) throws -> EntryWorkflowStateV2 = {
            var candidate = $0
            candidate.total += delta
            candidate.steps += 1
            return candidate
        }
        let validate: (EntryWorkflowStateV2) throws -> Void = { _ in
            guard delta >= 0 else {
                throw DebugError(.invalidArgument, "negative delta", hint: "use a nonnegative lab delta")
            }
        }
        let value: EntryWorkflowStateV2
        if slot.generation == "v2" {
            value = try slot.update(generation: "v2", as: EntryWorkflowStateV2.self,
                                    transform: advance, validate: validate)
        } else {
            value = try slot.migrate(from: "v1", as: EntryWorkflowStateV1.self, to: "v2",
                transform: { try advance(EntryWorkflowStateV2(total: $0.total, steps: 0, label: "migrated")) }, validate: validate)
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
