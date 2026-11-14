// SPDX-License-Identifier: MIT
#include "ApparitionHooks.h"
#include <dlfcn.h>
#include <mach/mach.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <pthread.h>
#include <stdint.h>
#include <string.h>
#include <mach-o/nlist.h>

static pthread_mutex_t hook_lock = PTHREAD_MUTEX_INITIALIZER;

void *APFindSymbol(void *image, const char *name) {
    if (!name || !*name)
        return NULL;
    void *result = dlsym(image ? image : RTLD_DEFAULT, name);
    return result || *name != '_' ? result : dlsym(image ? image : RTLD_DEFAULT, name + 1);
}

int APHookMessage(Class cls, SEL selector, IMP replacement, IMP *original) {
    if (original)
        *original = NULL;
    if (!cls || !selector || !replacement)
        return 0;
    pthread_mutex_lock(&hook_lock);
    Method method = class_getInstanceMethod(cls, selector);
    if (!method) {
        pthread_mutex_unlock(&hook_lock);
        return 0;
    }
    if (original)
        *original = method_getImplementation(method);
    class_replaceMethod(cls, selector, replacement, method_getTypeEncoding(method));
    pthread_mutex_unlock(&hook_lock);
    return 1;
}

// Change only data pages and restore their exact prior protection. In
// particular, never turn an executable page writable as a fallback.
static int replace_slot(void **slot, void *target, void *replacement) {
    pthread_mutex_lock(&hook_lock);
    vm_address_t region = (vm_address_t)slot;
    vm_size_t size = 0;
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_port_t object = MACH_PORT_NULL;
    kern_return_t kr = vm_region_64(mach_task_self(), &region, &size, VM_REGION_BASIC_INFO_64,
                                    (vm_region_info_t)&info, &count, &object);
    if (object != MACH_PORT_NULL)
        mach_port_deallocate(mach_task_self(), object);
    if (kr != KERN_SUCCESS || region > (uintptr_t)slot || (info.protection & VM_PROT_EXECUTE)) {
        pthread_mutex_unlock(&hook_lock);
        return 0;
    }
    vm_address_t page = (uintptr_t)slot & ~((vm_address_t)vm_page_size - 1);
    int changed = !(info.protection & VM_PROT_WRITE);
    if (changed && vm_protect(mach_task_self(), page, vm_page_size, FALSE,
                              info.protection | VM_PROT_WRITE | VM_PROT_COPY) != KERN_SUCCESS) {
        pthread_mutex_unlock(&hook_lock);
        return 0;
    }
    void *expected = target;
    int wrote = __atomic_compare_exchange_n(slot, &expected, replacement, 0, __ATOMIC_SEQ_CST,
                                            __ATOMIC_SEQ_CST);
    if (changed &&
        vm_protect(mach_task_self(), page, vm_page_size, FALSE, info.protection) != KERN_SUCCESS) {
        // Never report a protection failure as a successful hook.
        pthread_mutex_unlock(&hook_lock);
        return -1;
    }
    pthread_mutex_unlock(&hook_lock);
    return wrote;
}

// Resolve an indirect import through its two-level library ordinal. This also
// handles lazy slots whose current value still points at dyld's binder.
static int imported_target(const struct mach_header_64 *header, const struct nlist_64 *symbol,
                           const char *name, void *target) {
    unsigned wanted = (symbol->n_desc >> 8) & 255, ordinal = 0;
    if (!wanted || wanted >= 0xfd)
        return APFindSymbol(NULL, name) == target;
    const uint8_t *p = (const void *)(header + 1), *end = p + header->sizeofcmds;
    for (uint32_t i = 0; i < header->ncmds; ++i) {
        if ((size_t)(end - p) < sizeof(struct load_command))
            return 0;
        const struct load_command *c = (const void *)p;
        if (c->cmdsize < sizeof(*c) || c->cmdsize > (size_t)(end - p))
            return 0;
        if (c->cmd == LC_LOAD_DYLIB || c->cmd == LC_LOAD_WEAK_DYLIB ||
            c->cmd == LC_REEXPORT_DYLIB || c->cmd == LC_LOAD_UPWARD_DYLIB) {
            ++ordinal;
            if (ordinal == wanted && c->cmdsize >= sizeof(struct dylib_command)) {
                const struct dylib_command *d = (const void *)c;
                unsigned off = d->dylib.name.offset;
                if (off >= c->cmdsize || !memchr(p + off, 0, c->cmdsize - off))
                    return 0;
                void *handle = dlopen((const char *)p + off, RTLD_LAZY | RTLD_NOLOAD);
                if (!handle)
                    return 0;
                void *resolved = APFindSymbol(handle, name);
                dlclose(handle);
                return resolved == target;
            }
        }
        p += c->cmdsize;
    }
    return 0;
}
APHookResult APRebindImage(const struct mach_header *image, intptr_t slide, void *target,
                           void *replacement) {
    APHookResult result = {0, 0, 0};
    const struct mach_header_64 *header = (const void *)image;
    if (!target || !replacement || target == replacement || !header || header->magic != MH_MAGIC_64)
        return result;
    const struct symtab_command *symtab = NULL;
    const struct dysymtab_command *dysymtab = NULL;
    const struct segment_command_64 *linkedit = NULL;
    const uint8_t *meta = (const void *)(header + 1), *meta_end = meta + header->sizeofcmds;
    for (uint32_t c = 0; c < header->ncmds; ++c) {
        if ((size_t)(meta_end - meta) < 8)
            break;
        const struct load_command *lc = (const void *)meta;
        if (lc->cmdsize < 8 || lc->cmdsize > (size_t)(meta_end - meta))
            break;
        if (lc->cmd == LC_SYMTAB && lc->cmdsize >= sizeof(*symtab))
            symtab = (const void *)lc;
        if (lc->cmd == LC_DYSYMTAB && lc->cmdsize >= sizeof(*dysymtab))
            dysymtab = (const void *)lc;
        if (lc->cmd == LC_SEGMENT_64 && lc->cmdsize >= sizeof(*linkedit) &&
            !strncmp(((const struct segment_command_64 *)lc)->segname, "__LINKEDIT", 16))
            linkedit = (const void *)lc;
        meta += lc->cmdsize;
    }
    const struct nlist_64 *symbols = NULL;
    const uint32_t *indirect = NULL;
    const char *strings = NULL;
    if (symtab && dysymtab && linkedit && symtab->symoff >= linkedit->fileoff &&
        symtab->stroff >= linkedit->fileoff && dysymtab->indirectsymoff >= linkedit->fileoff &&
        (uint64_t)symtab->symoff + (uint64_t)symtab->nsyms * 16 <=
            linkedit->fileoff + linkedit->filesize &&
        (uint64_t)symtab->stroff + symtab->strsize <= linkedit->fileoff + linkedit->filesize &&
        (uint64_t)dysymtab->indirectsymoff + (uint64_t)dysymtab->nindirectsyms * 4 <=
            linkedit->fileoff + linkedit->filesize) {
        uintptr_t base = slide + linkedit->vmaddr - linkedit->fileoff;
        symbols = (const void *)(base + symtab->symoff);
        indirect = (const void *)(base + dysymtab->indirectsymoff);
        strings = (const void *)(base + symtab->stroff);
    }
    const uint8_t *cursor = (const void *)(header + 1);
    const uint8_t *end = cursor + header->sizeofcmds;
    for (uint32_t c = 0; c < header->ncmds; ++c) {
        if ((size_t)(end - cursor) < sizeof(struct load_command))
            break;
        const struct load_command *command = (const void *)cursor;
        if (command->cmdsize < sizeof(*command) || command->cmdsize > (size_t)(end - cursor))
            break;
        if (command->cmd == LC_SEGMENT_64 &&
            command->cmdsize >= sizeof(struct segment_command_64)) {
            const struct segment_command_64 *segment = (const void *)cursor;
            if (segment->nsects > (command->cmdsize - sizeof(*segment)) / sizeof(struct section_64))
                break;
            const struct section_64 *sections = (const void *)(segment + 1);
            for (uint32_t s = 0; s < segment->nsects; ++s) {
                const struct section_64 *section = &sections[s];
                unsigned type = section->flags & SECTION_TYPE;
                if (type != S_LAZY_SYMBOL_POINTERS && type != S_NON_LAZY_SYMBOL_POINTERS)
                    continue;
                // arm64e pointer signatures depend on each slot's fixup
                // metadata. Never copy a generic pointer into __AUTH.
                if (!strncmp(segment->segname, "__AUTH", 6))
                    continue;
                if (section->addr < segment->vmaddr || section->size > segment->vmsize ||
                    section->addr - segment->vmaddr > segment->vmsize - section->size ||
                    section->addr % sizeof(void *))
                    continue;
                void **slots = (void **)(section->addr + slide);
                for (size_t p = 0; p < section->size / sizeof(void *); ++p) {
                    void *current = __atomic_load_n(slots + p, __ATOMIC_ACQUIRE);
                    if (current != target) {
                        if (type != S_LAZY_SYMBOL_POINTERS || !indirect ||
                            (uint64_t)section->reserved1 + p >= dysymtab->nindirectsyms)
                            continue;
                        uint32_t index = indirect[section->reserved1 + p];
                        if (index >= symtab->nsyms)
                            continue;
                        const struct nlist_64 *symbol = symbols + index;
                        if (symbol->n_un.n_strx >= symtab->strsize)
                            continue;
                        const char *name = strings + symbol->n_un.n_strx;
                        if (!memchr(name, 0, symtab->strsize - symbol->n_un.n_strx) ||
                            !imported_target(header, symbol, name, target))
                            continue;
                    }
                    int status = replace_slot(slots + p, current, replacement);
                    if (status == 1)
                        ++result.rebound;
                    else if (status == -1)
                        ++result.protection_failures;
                    else
                        ++result.unavailable;
                }
            }
        }
        cursor += command->cmdsize;
    }
    return result;
}

APHookResult APHookImports(void *target, void *replacement) {
    APHookResult total = {0, 0, 0};
    for (uint32_t i = 0; i < _dyld_image_count(); ++i) {
        APHookResult r = APRebindImage(_dyld_get_image_header(i), _dyld_get_image_vmaddr_slide(i),
                                       target, replacement);
        total.rebound += r.rebound;
        total.unavailable += r.unavailable;
        total.protection_failures += r.protection_failures;
    }
    return total;
}
