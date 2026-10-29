#pragma once
#include <stddef.h>
#include <stdint.h>

int lab_debugger_attached(void);
const void *lab_opaque_pointer(const void *pointer);
uint64_t lab_runtime_input(void);
// Lab only, single caller/thread: one active entry patch at a time. Returns 0
// on success, 1 when not traced, 2 for invalid/range state, 3 for a failed write.
int lab_entry_patch(const void *entry, const void *replacement);
int lab_entry_restore(void);

/// Runs the __TEXT patch experiment and writes a JSON report into `report`.
/// `breadcrumbs` is a file each step is appended to (and synced) before it runs,
/// so a fault still shows how far the test got.
void text_patch_run(const char *breadcrumbs, char *report, size_t capacity);

/// Calls every target again (patched or not) and reports what each returns now.
void text_patch_recheck(char *report, size_t capacity);

/// Entry-branch experiment on optimized compiler output, including an inlined
/// caller and timing samples. Restores the entry before returning; never detaches.
void optimized_patch_run(const char *breadcrumbs, char *report, size_t capacity);
