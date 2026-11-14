// SPDX-License-Identifier: MIT
#include "APInternal.h"
#include <string.h>
static void word(uint8_t *out, uint32_t value) { memcpy(out, &value, 4); }
static int64_t sext(uint64_t value, unsigned bits) {
    return (int64_t)(value << (64 - bits)) >> (64 - bits);
}
void APAbsoluteJump(uint8_t out[16], uintptr_t target) {
    word(out, 0x58000050);     // ldr x16, .+8
    word(out + 4, 0xd61f0200); // br x16
    memcpy(out + 8, &target, 8);
}
static void constant(uint8_t *out, unsigned reg, uintptr_t value) {
    word(out, 0xd2800000 | (uint32_t)(value & 0xffff) << 5 | reg);
    for (unsigned i = 1; i < 4; ++i)
        word(out + 4 * i,
             0xf2800000 | i << 21 | (uint32_t)((value >> (16 * i)) & 0xffff) << 5 | reg);
}
static uintptr_t remap(uintptr_t target, uintptr_t source, uintptr_t destination) {
    if (target >= source && target < source + 16 && !(target & 3))
        return destination + (target - source) / 4 * 32;
    return target;
}
int APRelocate(const uint32_t input[4], uintptr_t source, uintptr_t dest, uint8_t out[160]) {
    // Each relocated instruction owns 32 bytes. This makes internal branches
    // deterministic even when preceding PC-relative instructions expand.
    for (unsigned i = 0; i < 40; ++i)
        word(out + i * 4, 0xd503201f);
    for (unsigned i = 0; i < 4; ++i) {
        uint32_t insn = input[i];
        uint8_t *slot = out + i * 32;
        uintptr_t pc = source + i * 4;
        if ((insn & 0x1f000000) == 0x10000000) { // ADR, ADRP
            unsigned rd = insn & 31;
            if (rd == 16 || rd == 17)
                return 0;
            int64_t imm = sext(((insn >> 5) & 0x7ffff) << 2 | ((insn >> 29) & 3), 21);
            uintptr_t target = insn >> 31 ? (pc & ~(uintptr_t)4095) + imm * 4096 : pc + imm;
            constant(slot, rd, target);
        } else if ((insn & 0x7c000000) == 0x14000000) { // B, BL
            if (!(insn >> 31) && i < 3)
                return 0; // short tail-call veneer
            uintptr_t target = remap(pc + sext(insn & 0x3ffffff, 26) * 4, source, dest);
            if (insn >> 31) {
                constant(slot, 16, target);
                word(slot + 16, 0xd63f0200); // blr x16; return into NOP padding
            } else
                APAbsoluteJump(slot, target);
        } else if ((insn & 0xff000000) == 0x54000000 || (insn & 0x7e000000) == 0x34000000 ||
                   (insn & 0x7e000000) == 0x36000000) {
            int conditional = (insn & 0xff000000) == 0x54000000;
            if ((conditional && (insn & 15) >= 14) ||
                (!conditional && ((insn & 31) == 16 || (insn & 31) == 17)))
                return 0;
            unsigned bits = (insn & 0x7e000000) == 0x36000000 ? 14 : 19;
            uintptr_t target =
                remap(pc + sext((insn >> 5) & ((1u << bits) - 1), bits) * 4, source, dest);
            // Invert condition to skip the absolute branch and its literal.
            uint32_t mask = ((1u << bits) - 1) << 5;
            word(slot, ((insn ^ (conditional ? 1u : 1u << 24)) & ~mask) | 5u << 5);
            APAbsoluteJump(slot + 4, target);
        } else if ((insn & 0x3b000000) == 0x18000000) { // LDR literal, PRFM
            unsigned rt = insn & 31, opc = insn >> 30;
            unsigned vector = (insn >> 26) & 1;
            if (rt == 16 || rt == 17 || (opc == 3 && vector))
                return 0;
            uintptr_t target = pc + sext((insn >> 5) & 0x7ffff, 19) * 4;
            // The scratch register is IP0, already clobbered by entry redirects.
            constant(slot, 16, target);
            uint32_t load = vector ? (opc == 0   ? 0xbd400000
                                      : opc == 1 ? 0xfd400000
                                                 : 0x3dc00000)
                                   : (opc == 0   ? 0xb9400000
                                      : opc == 1 ? 0xf9400000
                                      : opc == 2 ? 0xb9800000
                                                 : 0xf9800000);
            word(slot + 16, load | 16u << 5 | rt);
        } else {
            // Reject exception-generating and indirect-control encodings. A
            // terminal return in the fourth instruction is safe to copy.
            unsigned group = (insn >> 25) & 15;
            int terminal_return =
                i == 3 && (insn == 0xd65f03c0 || insn == 0xd65f0fff || insn == 0xd65f0bff);
            if (((group == 10 || group == 11) && (insn >> 24) != 0xd5 && !terminal_return) ||
                (insn & 0xff000000) == 0xd4000000 || insn == 0 ||
                ((insn & 0xfe000000) == 0xd6000000 && !terminal_return))
                return 0;
            // IP0/IP1 values created in the prefix cannot survive our branch
            // back. Conservatively reject encodings naming either register.
            if ((insn & 31) == 16 || (insn & 31) == 17 || ((insn >> 5) & 31) == 16 ||
                ((insn >> 5) & 31) == 17 || ((insn >> 16) & 31) == 16 || ((insn >> 16) & 31) == 17)
                return 0;
            word(slot, insn);
        }
        // A copied instruction falls through NOP padding to the next slot.
    }
    APAbsoluteJump(out + 128, source + 16);
    return 1;
}
