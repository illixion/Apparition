enum DebugTraceImplementationDomainCaller {
    @inline(__always)
    static func caller(_ value: Int) -> Int { DebugTraceImplementationDomainLeaf.leaf(value) + 1 }
}
