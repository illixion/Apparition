// This independent boundary must never acquire a direct reference to domain helpers.
@_silgen_name("debugtrace_entry_domain_workflow_outside")
@inline(never)
@_optimize(none)
public func entryWorkflowDomainOutside(_ value: Int, _ offset: Int) -> Int { value + offset }
