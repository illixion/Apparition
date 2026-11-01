@_silgen_name("debugtrace_entry_domain_workflow_first")
@inline(never)
@_optimize(none)
public func entryWorkflowDomainFirst(_ value: Int, _ offset: Int) -> Int {
    DebugTraceImplementationDomainCaller.caller(value) + offset
}

@_silgen_name("debugtrace_entry_domain_workflow_second")
@inline(never)
@_optimize(none)
public func entryWorkflowDomainSecond(_ value: Int, _ offset: Int) -> Int {
    DebugTraceImplementationDomainCaller.caller(value) + offset + 100
}
