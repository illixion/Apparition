#import <Foundation/Foundation.h>
#include "ApparitionHooks.h"
#include "APInternal.h"
#include "substrate.h"
#include <unistd.h>
#include <assert.h>
#include <dlfcn.h>
extern int ap_fixture_call(void);
extern int ap_fixture_value(void);
static int replacement(void) { return 19; }
@interface APParent : NSObject {
  @public
    int fixtureIvar;
}
- (int)value;
@end
@implementation APParent
- (int)value {
    return 7;
}
@end
@interface APChild : APParent
@end
@implementation APChild
@end
extern int ap_entry_value(void);
extern int ap_entry_call(void);
static int (*entry_original)(void);
static int entry_replacement(void) { return entry_original() + 20; }
__attribute__((noinline)) void ap_fixture_ready(void) { __asm__ volatile("nop"); }
extern int ap_adr_value(void), ap_page_value(void), ap_literal_value(void), ap_call_value(void);
extern int ap_conditional_value(int);
static int (*adr_original)(void), (*page_original)(void), (*literal_original)(void),
    (*call_original)(void);
static int (*conditional_original)(int);
static int reloc_replacement(void) { return 99; }
@interface APCopy : NSObject
- (int)value;
@end
@implementation APCopy
- (int)value {
    return 55;
}
@end
@interface APBase : NSObject
- (int)value;
@end
@implementation APBase
- (int)value {
    return 0;
}
@end
static IMP original;
static int message(id self, SEL cmd) { return ((int (*)(id, SEL))original)(self, cmd) + 12; }
int main(void) {
    @autoreleasepool {
        assert(ap_fixture_call() == 7);
        assert(APFindSymbol(NULL, "ap_fixture_value") == (void *)ap_fixture_value);
        assert(APFindSymbol(NULL, "_ap_fixture_value") == (void *)ap_fixture_value);
        assert(!APFindSymbol(NULL, NULL));
        APHookResult invalid = APHookImports(NULL, (void *)replacement);
        assert(!invalid.rebound && !invalid.unavailable);
        int (*volatile saved)(void) = (void *)dlsym(RTLD_DEFAULT, "ap_fixture_value");
        APHookResult result = APHookImports((void *)saved, (void *)replacement);
        assert(result.rebound > 0 && result.unavailable == 0);
        assert(ap_fixture_call() == 19);
        assert(saved() == 7); // captured original remains intact
        assert(APHookMessage(APChild.class, @selector(value), (IMP)message, &original));
        assert([[APChild new] value] == 19);
        assert([[APParent new] value] == 7);
        assert(!APHookMessage(APChild.class, sel_registerName("missing"), (IMP)message, NULL));
        assert(LBHookMessage(APChild.class, sel_registerName("missing"), (IMP)message, NULL) == 1);
        assert(LHHookFunctions(NULL, 1) == 5);
        int data_value = 1, new_value = 2;
        struct LHMemoryPatch patch = {&data_value, &new_value, sizeof new_value, NULL};
        assert(LHPatchMemory(&patch, 1) == 0 && data_value == 2);
        void *page = (void *)1;
        assert(!LHExecMemory(&page, NULL, 0) && page == NULL);
        const char *names[] = {"_ap_fixture_value", "missing_fixture_symbol"};
        void *results[2];
        assert(!LHFindSymbols(NULL, names, results, 2));
        assert(results[0] && !results[1]);
        assert(MSFindSymbol(MSGetImageByName("libprovider.dylib"), "_ap_fixture_value"));
        int (*volatile entry)(void) = ap_entry_value;
        MSHookFunction((void *)entry, (void *)entry_replacement, (void **)&entry_original);
        assert(entry_original() == 11);
        assert(ap_entry_call() == 31);
        void *late = dlopen("@rpath/liblate.dylib", RTLD_NOW | RTLD_LOCAL);
        assert(late);
        int (*late_value)(void) = (void *)dlsym(late, "ap_late_initial");
        assert(late_value && late_value() == 31); // hooked before constructor
        assert(entry() == 11);                    // no debugger: only import calls changed
        APParent *object = [APParent new];
        *(int *)MSHookIvar(object, "fixtureIvar") = 42;
        assert(object->fixtureIvar == 42);
        MSHookClassPair(APParent.class, APCopy.class, APBase.class);
        assert([[APParent new] value] == 55 && [[APBase new] value] == 7);
        if (getenv("AP_FULL_TEST")) {
            MSHookFunction((void *)ap_adr_value, (void *)reloc_replacement, (void **)&adr_original);
            MSHookFunction((void *)ap_page_value, (void *)reloc_replacement,
                           (void **)&page_original);
            MSHookFunction((void *)ap_literal_value, (void *)reloc_replacement,
                           (void **)&literal_original);
            MSHookFunction((void *)ap_call_value, (void *)reloc_replacement,
                           (void **)&call_original);
            MSHookFunction((void *)ap_conditional_value, (void *)reloc_replacement,
                           (void **)&conditional_original);
            ap_fixture_ready();
            for (unsigned tries = 0; APHookControl.pending && tries < 100; ++tries)
                usleep(100000);
            assert(!APHookControl.pending && !APHookControl.failed);
            assert(entry() == 31);          // direct entry call after late attach
            assert(entry_original() == 11); // stable original now uses relocated code
            assert(adr_original() == 21 && page_original() == 22);
            assert(literal_original() == 23 && call_original() == 37);
            assert(conditional_original(0) == 31 && conditional_original(1) == 29);
            assert(ap_adr_value() == 99 && ap_page_value() == 99);
        }
        puts("ApparitionHooks checks passed");
    }
}
