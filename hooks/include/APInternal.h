// SPDX-License-Identifier: MIT
#pragma once
#include "ApparitionHooks.h"
#include <stdint.h>
#include <stdbool.h>
#define AP_MAX_HOOKS 1024
#define AP_CONTROL_MAGIC UINT64_C(0x4150484f4f4b3031)
#define AP_CAPABILITY UINT64_C(0x4150484b)
// Fixed wire layout; discovered through __DATA,__aphooks while all threads stop.
typedef struct {
    uint64_t magic;
    uint64_t version;
    uint64_t ready;
    uint64_t pending;
    uint64_t failed;
} APControl;
extern APControl APHookControl;
extern void *APOriginalTargets[AP_MAX_HOOKS];
extern const unsigned char APOriginalThunks[];
int APFunctionHook(void *target, void *replacement, void **original);
int APPatchMemory(void *target, const void *bytes, size_t length, bool queue);
void *APExecMemory(const void *bytes, size_t length);
void APStartRuntime(void);
void APShowNotice(void);
void APNotifyImage(void);
struct mach_header;
APHookResult APRebindImage(const struct mach_header *, intptr_t, void *, void *);
void *APOpenImage(const char *path);
void *APImageSymbol(const void *image, const char *name);
// Relocate 16 bytes to a 160-byte buffer at the supplied executable address.
// Fail closed on short functions and encodings we cannot preserve.
int APRelocate(const uint32_t input[4], uintptr_t source, uintptr_t destination,
               uint8_t output[160]);
void APAbsoluteJump(uint8_t output[16], uintptr_t destination);
