// SPDX-License-Identifier: MIT
#include "APInternal.h"
#include <dispatch/dispatch.h>
#include <mach/mach.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <stdatomic.h>
#include <sys/sysctl.h>
#include <sys/proc.h>
#include <mach-o/dyld.h>

__attribute__((used, visibility("default"),
               section("__DATA,__aphooks"))) APControl APHookControl = {AP_CONTROL_MAGIC, 1, 0, 0,
                                                                        0};
void *APOriginalTargets[AP_MAX_HOOKS];
typedef struct {
    void *target;
    _Atomic(void *) replacement;
    void *rebound;
    void *trampoline;
    void *api_original;
    uint8_t expected[16];
    int installed;
    int failed;
} Hook;
typedef struct Patch {
    void *target;
    void *bytes;
    size_t size;
    struct Patch *next;
} Patch;
static Hook hooks[AP_MAX_HOOKS];
static _Atomic size_t hook_count;
static Patch *patches;
static pthread_mutex_t runtime_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t started = PTHREAD_ONCE_INIT;
static dispatch_source_t timer;
static int images_dirty = 1;
static int finish_announced;

static uint64_t request(uint64_t op, uint64_t a, uint64_t b, uint64_t c) {
#if defined(__aarch64__)
    register uint64_t x0 __asm__("x0") = a;
    register uint64_t x1 __asm__("x1") = b;
    register uint64_t x2 __asm__("x2") = c;
    register uint64_t x16 __asm__("x16") = op;
    __asm__ volatile("brk #0xf00d" : "+r"(x0), "+r"(x1), "+r"(x2), "+r"(x16) : : "memory", "cc");
    return x0;
#else
    (void)op;
    (void)a;
    (void)b;
    (void)c;
    return 1;
#endif
}
static int available(void) {
    if (__atomic_load_n(&APHookControl.ready, __ATOMIC_ACQUIRE) != AP_CAPABILITY)
        return 0;
    // The mailbox proves protocol support; the live tracing flag additionally
    // prevents stale grants after an abrupt debugger disconnect. CS_DEBUGGED
    // is sticky and cannot answer this question.
    int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()};
    struct kinfo_proc info;
    size_t size = sizeof info;
    if (sysctl(mib, 4, &info, &size, NULL, 0) || size != sizeof info ||
        !(info.kp_proc.p_flag & P_TRACED)) {
        __atomic_store_n(&APHookControl.ready, 0, __ATOMIC_RELEASE);
        return 0;
    }
    return 1;
}
static int read_bytes(void *address, void *out, size_t size) {
    vm_size_t read = 0;
    return vm_read_overwrite(mach_task_self(), (vm_address_t)address, size, (vm_address_t)out,
                             &read) == KERN_SUCCESS &&
           read == size;
}
static void counts(void) {
    uint64_t pending = 0, failed = 0;
    for (size_t i = 0; i < hook_count; ++i) {
        if (!hooks[i].installed && !hooks[i].failed)
            ++pending;
        if (hooks[i].failed)
            ++failed;
    }
    for (Patch *p = patches; p; p = p->next)
        ++pending;
    __atomic_store_n(&APHookControl.pending, pending, __ATOMIC_RELEASE);
    __atomic_store_n(&APHookControl.failed, failed, __ATOMIC_RELEASE);
}
// Must hold runtime_lock, so only one thread can issue hook requests or finish
// a hook session. The debugger revokes ready before it detaches.
static void *exec_locked(const void *bytes, size_t size) {
    if (!available() || !bytes || !size || size > 1024 * 1024)
        return NULL;
    size_t length = (size + vm_page_size - 1) & ~(size_t)(vm_page_size - 1);
    uint64_t address = request(1, 0, length, AP_CAPABILITY);
    if (!address)
        return NULL;
    if (request(0x4150, address, (uintptr_t)bytes, size))
        return NULL;
    return (void *)(uintptr_t)address;
}
void *APExecMemory(const void *bytes, size_t length) {
    pthread_mutex_lock(&runtime_lock);
    void *out = exec_locked(bytes, length);
    pthread_mutex_unlock(&runtime_lock);
    return out;
}
static int upgrade(Hook *h, size_t index) {
    if (!available() || h->failed)
        return 4;
    if (!h->trampoline) {
        uint32_t instructions[4];
        if (!read_bytes(h->target, instructions, sizeof instructions)) {
            h->failed = 1;
            return 4;
        }
        // Allocation must precede relocation, since internal branches target
        // expanded instruction slots in this particular mapping.
        uintptr_t address = request(1, 0, vm_page_size, AP_CAPABILITY);
        if (!address)
            return 4;
        uint8_t code[160];
        if (!APRelocate(instructions, (uintptr_t)h->target, address, code)) {
            h->failed = 1;
            return 3;
        }
        if (request(0x4150, address, (uintptr_t)code, sizeof code)) {
            h->failed = 1;
            return 4;
        }
        h->trampoline = (void *)address;
        memcpy(h->expected, instructions, 16);
    }
    struct {
        uint64_t target;
        uint8_t before[16];
        uint8_t after[16];
    } record;
    record.target = (uintptr_t)h->target;
    memcpy(record.before, h->expected, 16);
    APAbsoluteJump(record.after, (uintptr_t)h->replacement);
    // Publish a callable original before any thread can see the entry redirect.
    // The old code remains valid through the entire all-stop transaction.
    __atomic_store_n(&APOriginalTargets[index], h->trampoline, __ATOMIC_RELEASE);
    uint64_t status = request(0x4151, (uintptr_t)&record, 1, 0);
    if (status) {
        if (status != 2)
            h->failed = 1; // busy entry is retried on the next tick
        return 4;
    }
    memcpy(h->expected, record.after, 16);
    h->installed = 1;
    return 0;
}

int APFunctionHook(void *target, void *replacement, void **original) {
    if (original)
        *original = target;
    if (!target || !replacement || target == replacement)
        return 5;
    APStartRuntime();
    pthread_mutex_lock(&runtime_lock);
    size_t index;
    for (index = 0; index < hook_count; ++index)
        if (hooks[index].target == target)
            break;
    void *previous = target;
    if (index == hook_count) {
        if (hook_count == AP_MAX_HOOKS) {
            pthread_mutex_unlock(&runtime_lock);
            return 4;
        }
        Hook *h = &hooks[hook_count];
        h->target = target;
        h->rebound = target;
        __atomic_store_n(&APOriginalTargets[index], target, __ATOMIC_RELEASE);
        h->api_original = (void *)(APOriginalThunks + index * 16);
        if (original)
            *original = h->api_original;
    } else {
        if (hooks[index].replacement == replacement) {
            if (original)
                *original = hooks[index].api_original;
            int result = hooks[index].installed ? 0 : 4;
            pthread_mutex_unlock(&runtime_lock);
            return result;
        }
        previous = hooks[index].replacement;
        hooks[index].api_original = previous;
        if (original)
            *original = previous;
    }
    Hook *h = &hooks[index];
    h->replacement = replacement;
    finish_announced = 0;
    h->installed = 0;
    if (index == hook_count)
        ++hook_count;
    int status = available() ? upgrade(h, index) : 4;
    counts();
    pthread_mutex_unlock(&runtime_lock);
    APHookResult fallback = APHookImports(previous, replacement);
    return status == 0 || fallback.rebound ? 0 : status;
}
int APPatchMemory(void *target, const void *bytes, size_t size, bool queue) {
    if (!target || !bytes || !size || size > 4096)
        return 4;
    // Writable data needs no debugger and must not be confused with code.
    vm_address_t region = (vm_address_t)target;
    vm_size_t region_size = 0;
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count = VM_REGION_BASIC_INFO_COUNT_64;
    mach_port_t object = MACH_PORT_NULL;
    kern_return_t kr =
        vm_region_64(mach_task_self(), &region, &region_size, VM_REGION_BASIC_INFO_64,
                     (vm_region_info_t)&info, &count, &object);
    if (object)
        mach_port_deallocate(mach_task_self(), object);
    uintptr_t address = (uintptr_t)target;
    if (kr == KERN_SUCCESS && region <= address && address - region <= region_size &&
        size <= region_size - (address - region) &&
        (info.protection & (VM_PROT_WRITE | VM_PROT_EXECUTE)) == VM_PROT_WRITE &&
        vm_write(mach_task_self(), address, (vm_offset_t)bytes, (mach_msg_type_number_t)size) ==
            KERN_SUCCESS)
        return 0;
    APStartRuntime();
    pthread_mutex_lock(&runtime_lock);
    int status = available() ? (int)request(0x4150, (uintptr_t)target, (uintptr_t)bytes, size) : 4;
    if (status && queue) {
        Patch *p = calloc(1, sizeof *p);
        if (p && (p->bytes = malloc(size))) {
            memcpy(p->bytes, bytes, size);
            p->target = target;
            p->size = size;
            p->next = patches;
            patches = p;
        } else
            free(p);
    }
    counts();
    pthread_mutex_unlock(&runtime_lock);
    return status ? 4 : 0;
}
static void tick(void *unused) {
    (void)unused;
    // Do not hold runtime_lock across dyld callbacks/iteration.
    if (__atomic_exchange_n(&images_dirty, 0, __ATOMIC_ACQ_REL)) {
        for (size_t i = 0;; ++i) {
            pthread_mutex_lock(&runtime_lock);
            if (i == hook_count) {
                pthread_mutex_unlock(&runtime_lock);
                break;
            }
            void *from = hooks[i].rebound, *to = hooks[i].replacement;
            hooks[i].rebound = to;
            pthread_mutex_unlock(&runtime_lock);
            APHookImports(from, to);
            // New images still contain the real symbol even after prior hooks.
            if (from != hooks[i].target)
                APHookImports(hooks[i].target, to);
        }
    }
    pthread_mutex_lock(&runtime_lock);
    if (!available())
        finish_announced = 0;
    if (available() && (!finish_announced || APHookControl.pending)) {
        if (request(0x4152, 1, 0, 0) == AP_CAPABILITY) {
            for (size_t i = 0; i < hook_count; ++i)
                if (!hooks[i].installed && !hooks[i].failed)
                    upgrade(&hooks[i], i);
            Patch **next = &patches;
            while (*next) {
                Patch *p = *next;
                if (!request(0x4150, (uintptr_t)p->target, (uintptr_t)p->bytes, p->size)) {
                    *next = p->next;
                    free(p->bytes);
                    free(p);
                } else
                    next = &p->next;
            }
            counts();
            // Give busy entries another tick before allowing automatic detach.
            if (!APHookControl.pending) {
                finish_announced = 1;
                request(0x4154, 0, 0, 0);
            }
        } else
            __atomic_store_n(&APHookControl.ready, 0, __ATOMIC_RELEASE);
    }
    counts();
    pthread_mutex_unlock(&runtime_lock);
    APShowNotice();
}
static void image_added(const struct mach_header *header, intptr_t slide) {
    // dyld calls this before initializers. The published registry is fixed
    // storage and replacements are atomic, so no runtime/loader lock inversion.
    size_t count = atomic_load_explicit(&hook_count, memory_order_acquire);
    for (size_t i = 0; i < count; ++i) {
        void *replacement = atomic_load_explicit(&hooks[i].replacement, memory_order_acquire);
        APRebindImage(header, slide, hooks[i].target, replacement);
    }
    APNotifyImage();
}
void APNotifyImage(void) { __atomic_store_n(&images_dirty, 1, __ATOMIC_RELEASE); }
static void start(void) {
    _dyld_register_func_for_add_image(image_added);
    timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0,
                                   dispatch_get_global_queue(QOS_CLASS_UTILITY, 0));
    dispatch_source_set_event_handler_f(timer, tick);
    dispatch_source_set_timer(timer, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC),
                              NSEC_PER_SEC / 4, NSEC_PER_SEC / 20);
    dispatch_resume(timer);
}
void APStartRuntime(void) { pthread_once(&started, start); }
__attribute__((constructor)) static void initialize(void) { APStartRuntime(); }
