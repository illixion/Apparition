// SPDX-License-Identifier: MIT
#include "APInternal.h"
#include <dlfcn.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach-o/nlist.h>
#include <mach/mach.h>
#include <stdlib.h>
#include <string.h>

void *APOpenImage(const char *path) {
    if (!path)
        return NULL;
    for (uint32_t i = 0; i < _dyld_image_count(); ++i) {
        const char *name = _dyld_get_image_name(i);
        if (!name)
            continue;
        const char *base = strrchr(name, '/');
        if (!strcmp(path, name) || (!strchr(path, '/') && !strcmp(path, base ? base + 1 : name)))
            return (void *)_dyld_get_image_header(i);
    }
    // Keep a loaded image alive: the public image handles are Mach-O headers,
    // and cannot be passed to dlclose. MSCloseImage/LHCloseImage are no-ops.
    void *handle = dlopen(path, RTLD_LAZY | RTLD_LOCAL);
    if (!handle)
        return NULL;
    for (uint32_t i = 0; i < _dyld_image_count(); ++i) {
        const char *name = _dyld_get_image_name(i);
        if (name && !strcmp(path, name))
            return (void *)_dyld_get_image_header(i);
    }
    return NULL;
}
static void *local_symbol(const struct mach_header_64 *header, intptr_t slide, const char *name) {
    if (!header || header->magic != MH_MAGIC_64)
        return NULL;
    const struct symtab_command *symbols = NULL;
    const struct segment_command_64 *linkedit = NULL;
    const uint8_t *cursor = (const void *)(header + 1), *end = cursor + header->sizeofcmds;
    for (uint32_t i = 0; i < header->ncmds; ++i) {
        if ((size_t)(end - cursor) < sizeof(struct load_command))
            return NULL;
        const struct load_command *c = (const void *)cursor;
        if (c->cmdsize < sizeof(*c) || c->cmdsize > (size_t)(end - cursor))
            return NULL;
        if (c->cmd == LC_SYMTAB && c->cmdsize >= sizeof(struct symtab_command))
            symbols = (const void *)c;
        if (c->cmd == LC_SEGMENT_64 && c->cmdsize >= sizeof(struct segment_command_64) &&
            !strncmp(((const struct segment_command_64 *)c)->segname, "__LINKEDIT", 16))
            linkedit = (const void *)c;
        cursor += c->cmdsize;
    }
    if (!symbols || !linkedit)
        return NULL;
    uint64_t table_end =
        (uint64_t)symbols->symoff + (uint64_t)symbols->nsyms * sizeof(struct nlist_64);
    uint64_t strings_end = (uint64_t)symbols->stroff + symbols->strsize;
    if (symbols->symoff < linkedit->fileoff || symbols->stroff < linkedit->fileoff ||
        table_end > linkedit->fileoff + linkedit->filesize ||
        strings_end > linkedit->fileoff + linkedit->filesize)
        return NULL;
    uintptr_t base = slide + linkedit->vmaddr - linkedit->fileoff;
    const struct nlist_64 *table = (const void *)(base + symbols->symoff);
    const char *strings = (const void *)(base + symbols->stroff);
    for (uint32_t i = 0; i < symbols->nsyms; ++i) {
        const struct nlist_64 *s = table + i;
        if ((s->n_type & N_STAB) || !(s->n_type & N_TYPE) || !s->n_value ||
            s->n_un.n_strx >= symbols->strsize)
            continue;
        const char *candidate = strings + s->n_un.n_strx;
        size_t remaining = symbols->strsize - s->n_un.n_strx;
        if (!memchr(candidate, 0, remaining))
            continue;
        if (!strcmp(candidate, name) || (*candidate == '_' && !strcmp(candidate + 1, name))) {
            if ((s->n_type & N_TYPE) == N_ABS)
                return (void *)(uintptr_t)s->n_value;
            if ((s->n_type & N_TYPE) == N_SECT)
                return (void *)(uintptr_t)(s->n_value + slide);
        }
    }
    return NULL;
}
void *APImageSymbol(const void *image, const char *name) {
    if (!name || !*name)
        return NULL;
    if (!image) {
        void *exported = APFindSymbol(NULL, name);
        if (exported)
            return exported;
    }
    for (uint32_t i = 0; i < _dyld_image_count(); ++i) {
        const struct mach_header *header = _dyld_get_image_header(i);
        if (image && image != header)
            continue;
        const char *path = _dyld_get_image_name(i);
        void *handle = path ? dlopen(path, RTLD_LAZY | RTLD_NOLOAD) : NULL;
        void *symbol = handle ? APFindSymbol(handle, name) : NULL;
        if (handle)
            dlclose(handle);
        if (!symbol)
            symbol = local_symbol((const void *)header, _dyld_get_image_vmaddr_slide(i), name);
        if (symbol)
            return symbol;
    }
    return NULL;
}
