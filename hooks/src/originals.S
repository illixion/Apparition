// SPDX-License-Identifier: MIT
#if defined(__aarch64__)
.text
.p2align 4
.globl _APOriginalThunks
_APOriginalThunks:
.set ap_index, 0
.rept 1024
    adrp x16, _APOriginalTargets@PAGE
    add x16, x16, _APOriginalTargets@PAGEOFF
    ldr x16, [x16, #(ap_index * 8)]
    br x16
.set ap_index, ap_index + 1
.endr
#else
#error ApparitionHooks requires ARM64
#endif
