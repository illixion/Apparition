#import <Foundation/Foundation.h>
#include "ApparitionHooks.h"
#include <assert.h>
#include <dlfcn.h>
extern int ap_fixture_call(void);
extern int ap_fixture_value(void);
static int replacement(void) { return 19; }
@interface APParent : NSObject
- (int)value;
@end
@implementation APParent
- (int)value { return 7; }
@end
@interface APChild : APParent
@end
@implementation APChild
@end
static IMP original;
static int message(id self, SEL cmd) {
    return ((int (*)(id, SEL))original)(self, cmd) + 12;
}
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
        puts("ApparitionHooks checks passed");
    }
}
