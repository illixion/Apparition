// SPDX-License-Identifier: MIT
#include "ApparitionHooks.h"
#include <dlfcn.h>
#include <mach/mach.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <pthread.h>
#include <stdint.h>
#include <string.h>

static pthread_mutex_t hook_lock = PTHREAD_MUTEX_INITIALIZER;

void *APFindSymbol(void *image, const char *name) {
    if (!name || !*name) return NULL;
    void *result = dlsym(image ? image : RTLD_DEFAULT, name);
    return result || *name != '_' ? result : dlsym(image ? image : RTLD_DEFAULT, name + 1);
}

int APHookMessage(Class cls, SEL selector, IMP replacement, IMP *original) {
    if (original) *original = NULL;
    if (!cls || !selector || !replacement) return 0;
    pthread_mutex_lock(&hook_lock);
    Method method = class_getInstanceMethod(cls, selector);
    if (!method) {
        pthread_mutex_unlock(&hook_lock);
        return 0;
    }
    if (original) *original = method_getImplementation(method);
    class_replaceMethod(cls, selector, replacement, method_getTypeEncoding(method));
    pthread_mutex_unlock(&hook_lock);
    return 1;
}

// Change only data pages and restore their exact prior protection. In
// particular, never turn an executable page writable as a fallback.
static int replace_slot(void **slot, void *target, void *replacement) {
    vm_address_t region = (vm_address_t)slot;
    vm_size_t size = 0;
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_port_t object = MACH_PORT_NULL;
    kern_return_t kr = vm_region_64(mach_task_self(), &region, &size,
        VM_REGION_BASIC_INFO_64, (vm_region_info_t)&info, &count, &object);
    if (object != MACH_PORT_NULL) mach_port_deallocate(mach_task_self(), object);
    if (kr != KERN_SUCCESS || region > (uintptr_t)slot ||
        (info.protection & VM_PROT_EXECUTE)) return 0;
    vm_address_t page = (uintptr_t)slot & ~((vm_address_t)vm_page_size - 1);
    int changed = !(info.protection & VM_PROT_WRITE);
    if (changed && vm_protect(mach_task_self(), page, vm_page_size, FALSE,
        info.protection | VM_PROT_WRITE | VM_PROT_COPY) != KERN_SUCCESS) return 0;
    void *expected = target;
    int wrote = __atomic_compare_exchange_n(slot, &expected, replacement, 0,
        __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    if (changed && vm_protect(mach_task_self(), page, vm_page_size, FALSE,
        info.protection) != KERN_SUCCESS) {
        // Never report a protection failure as a successful hook.
        return -1;
    }
    return wrote;
}

APHookResult APHookImports(void *target, void *replacement) {
    APHookResult result = {0, 0, 0};
    if (!target || !replacement || target == replacement) return result;
    pthread_mutex_lock(&hook_lock);
    uint32_t images = _dyld_image_count();
    for (uint32_t i = 0; i < images; ++i) {
        const struct mach_header_64 *header = (const void *)_dyld_get_image_header(i);
        if (!header || header->magic != MH_MAGIC_64) continue;
        intptr_t slide = _dyld_get_image_vmaddr_slide(i);
        const uint8_t *cursor = (const void *)(header + 1);
        const uint8_t *end = cursor + header->sizeofcmds;
        for (uint32_t c = 0; c < header->ncmds; ++c) {
            if ((size_t)(end - cursor) < sizeof(struct load_command)) break;
            const struct load_command *command = (const void *)cursor;
            if (command->cmdsize < sizeof(*command) || command->cmdsize > (size_t)(end - cursor)) break;
            if (command->cmd == LC_SEGMENT_64 && command->cmdsize >= sizeof(struct segment_command_64)) {
                const struct segment_command_64 *segment = (const void *)cursor;
                if (segment->nsects > (command->cmdsize - sizeof(*segment)) / sizeof(struct section_64)) break;
                const struct section_64 *sections = (const void *)(segment + 1);
                for (uint32_t s = 0; s < segment->nsects; ++s) {
                    const struct section_64 *section = &sections[s];
                    unsigned type = section->flags & SECTION_TYPE;
                    if (type != S_LAZY_SYMBOL_POINTERS && type != S_NON_LAZY_SYMBOL_POINTERS) continue;
                    // arm64e pointer signatures depend on each slot's fixup
                    // metadata. Never copy a generic pointer into __AUTH.
                    if (!strncmp(segment->segname, "__AUTH", 6)) continue;
                    if (section->addr < segment->vmaddr || section->size > segment->vmsize ||
                        section->addr - segment->vmaddr > segment->vmsize - section->size ||
                        section->addr % sizeof(void *)) continue;
                    void **slots = (void **)(section->addr + slide);
                    for (size_t p = 0; p < section->size / sizeof(void *); ++p) {
                        if (__atomic_load_n(slots + p, __ATOMIC_ACQUIRE) != target) continue;
                        int status = replace_slot(slots + p, target, replacement);
                        if (status == 1) ++result.rebound;
                        else if (status == -1) ++result.protection_failures;
                        else ++result.unavailable;
                    }
                }
            }
            cursor += command->cmdsize;
        }
    }
    pthread_mutex_unlock(&hook_lock);
    return result;
}
