// Native Swift entries: no @_cdecl thunks and no implicit-dynamic build flags.
@inline(never)
public func labSwiftScalar(_ value: UInt64) -> UInt64 { value &+ 1 }

@inline(never)
public func labSwiftText(_ value: String) -> String { "original:" + value }
