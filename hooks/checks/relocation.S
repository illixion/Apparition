// SPDX-License-Identifier: MIT
.text
.p2align 2
.globl _ap_adr_value
_ap_adr_value:
    adr x9, 1f
    ldr w0, [x9]
    nop
    ret
1:  .long 21
.p2align 2
.globl _ap_page_value
_ap_page_value:
    adrp x9, _ap_fixture_data@PAGE
    ldr w0, [x9, _ap_fixture_data@PAGEOFF]
    nop
    ret
.globl _ap_literal_value
_ap_literal_value:
    ldr w0, 1f
    nop
    nop
    ret
1:  .long 23
.globl _ap_conditional_value
_ap_conditional_value:
    cbz w0, 1f
    mov w0, #29
    nop
    ret
1:  mov w0, #31
    ret
.globl _ap_call_value
_ap_call_value:
    stp x29, x30, [sp, #-16]!
    bl 1f
    ldp x29, x30, [sp], #16
    ret
1:  mov w0, #37
    ret
.data
.p2align 2
_ap_fixture_data: .long 22
