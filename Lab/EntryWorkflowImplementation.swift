// This file is one reloadable unit: only private, state-free scalar helpers and
// explicit entry functions. A helper edit regenerates both entries, including
// the one whose source body did not change. Helpers retain normal optimization.
private enum DebugTraceImplementationMath {
    @inline(__always)
    static func leaf(_ value: Int) -> Int { value &* 3 }

    static func caller(_ value: Int) -> Int { leaf(value) &+ 1 }
}

@_silgen_name("debugtrace_entry_workflow_group_first")
@inline(never)
@_optimize(none)
public func entryWorkflowGroupFirst(_ value: Int, _ offset: Int) -> Int { DebugTraceImplementationMath.caller(value) &+ offset }

@_silgen_name("debugtrace_entry_workflow_group_second")
@inline(never)
@_optimize(none)
public func entryWorkflowGroupSecond(_ value: Int, _ offset: Int) -> Int { DebugTraceImplementationMath.caller(value) &+ offset &+ 100 }
