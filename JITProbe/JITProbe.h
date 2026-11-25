#pragma once
#include <stddef.h>

/// Asks the debugger for executable memory (JIT26), writes code into it and runs it, twice.
/// Writes what happened into `report`. Returns 0 when both runs returned what was written.
/// The region is kept: calling it again rewrites and reruns the same code.
int jit_probe_run(char *report, size_t capacity);

/// Requests a new arena from the live debugger, then runs and frees it.
int jit_probe_fresh(char *report, size_t capacity);
