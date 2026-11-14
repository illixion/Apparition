// SPDX-License-Identifier: MIT
#pragma once
#include <stddef.h>
#include <objc/runtime.h>

#ifdef __cplusplus
extern "C" {
#endif

// These are deliberately separate from the Substrate ABI until compatibility is
// verified. No executable memory is allocated or modified by this interface.
typedef struct {
    size_t rebound;
    size_t unavailable;
    size_t protection_failures;
} APHookResult;

// Rebind already resolved, unauthenticated import slots in loaded images. This
// cannot intercept direct calls, unresolved lazy imports or authenticated slots.
// Call again after loading additional images. Registration is not persistent.
// The caller must keep replacement loaded for the lifetime of those images.
// The caller must exclude concurrent image unloading. A protection_failures
// result means a slot changed but its page protection could not be restored.
APHookResult APHookImports(void *target, void *replacement);

// Returns false for a missing method; inherited methods are overridden only on
// the requested class. Publish the original before installing the replacement.
int APHookMessage(Class cls, SEL selector, IMP replacement, IMP *original);

// dlsym lookup, accepting either a C name or Mach-O's leading underscore.
void *APFindSymbol(void *image, const char *name);

#ifdef __cplusplus
}
#endif
