// Deliberately separate from the harness, with LTO disabled: the harness must call
// the real entry, while this translation unit also contains a known inlined copy.
#include <stdint.h>

__attribute__((always_inline)) uint64_t lab_optimized_target(uint64_t value) {
    return value + 1;
}

__attribute__((noinline)) uint64_t lab_optimized_inlined_caller(uint64_t value) {
    return lab_optimized_target(value);
}

__attribute__((noinline)) uint64_t lab_optimized_replacement(uint64_t value) {
    return value + 7;
}
