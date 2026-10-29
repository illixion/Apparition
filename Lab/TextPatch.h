#pragma once
#include <stddef.h>

/// Runs the __TEXT patch experiment and writes a JSON report into `report`.
/// `breadcrumbs` is a file each step is appended to (and synced) before it runs,
/// so a fault still shows how far the test got.
void text_patch_run(const char *breadcrumbs, char *report, size_t capacity);

/// Calls every target again (patched or not) and reports what each returns now.
void text_patch_recheck(char *report, size_t capacity);

/// Entry-branch experiment on optimized compiler output, including an inlined
/// caller and timing samples. Restores the entry before returning; never detaches.
void optimized_patch_run(const char *breadcrumbs, char *report, size_t capacity);
