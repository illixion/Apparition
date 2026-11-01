enum DebugTraceImplementationDomainLeaf {
    @inline(__always)
    static func leaf(_ value: Int) -> Int { value * 3 }
}
