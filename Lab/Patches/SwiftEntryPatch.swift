// Compiled separately AFTER the host app build. Only getters use the C ABI;
// replacement bodies and calls use Swift's native thin-function convention.
@inline(never)
public func replacementScalar(_ value: UInt64) -> UInt64 { value &+ 7 }

@inline(never)
public func replacementText(_ value: String) -> String { "replacement:" + value }

@_cdecl("lab_patch_scalar_address")
public func scalarAddress() -> UnsafeRawPointer {
    unsafeBitCast(replacementScalar as @convention(thin) (UInt64) -> UInt64, to: UnsafeRawPointer.self)
}

@_cdecl("lab_patch_text_address")
public func textAddress() -> UnsafeRawPointer {
    unsafeBitCast(replacementText as @convention(thin) (String) -> String, to: UnsafeRawPointer.self)
}
