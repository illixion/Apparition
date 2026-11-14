// SPDX-License-Identifier: MIT
// A real compatibility client: links only Substrate's public ABI, then records
// Objective-C and shared-cache function results for a signed, injected IPA.
#import <Foundation/Foundation.h>
#include "substrate.h"
#include <dlfcn.h>
#include <stdatomic.h>
#include <sys/sysctl.h>
#include <sys/proc.h>
#include <unistd.h>
#include <errno.h>
static int (*original_atoi)(const char *);
static int (*entry_atoi)(const char *);
static _Atomic unsigned calls;
static int replacement_atoi(const char *value) {
    atomic_fetch_add(&calls, 1);
    return original_atoi(value);
}
@interface APDeviceFixture : NSObject
- (int)value;
@end
@implementation APDeviceFixture
- (int)value {
    return 5;
}
@end
static IMP original_value;
static int replacement_value(id self, SEL cmd) {
    return ((int (*)(id, SEL))original_value)(self, cmd) + 1;
}
static dispatch_source_t timer;
__attribute__((constructor)) static void start(void) {
    // This fixture is driven by the computer; avoid concurrent manual JIT
    // requests from its informational notice during an automated attach test.
    [NSUserDefaults.standardUserDefaults setBool:YES forKey:@"ApparitionHooks.suppressNotice"];
    MSHookMessageEx(APDeviceFixture.class, @selector(value), (IMP)replacement_value,
                    &original_value);
    entry_atoi = (void *)dlsym(RTLD_DEFAULT, "atoi");
    MSHookFunction((void *)entry_atoi, (void *)replacement_atoi, (void **)&original_atoi);
    timer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, dispatch_get_main_queue());
    dispatch_source_set_timer(timer, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), NSEC_PER_SEC,
                              NSEC_PER_SEC / 20);
    dispatch_source_set_event_handler(timer, ^{
      unsigned before = atomic_load(&calls);
      int value = entry_atoi("41");
      BOOL intercepted = atomic_load(&calls) > before;
      int mib[] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()};
      struct kinfo_proc info = {0};
      size_t size = sizeof info;
      int sysctl_result = sysctl(mib, 4, &info, &size, NULL, 0);
      uint64_t *control = dlsym(RTLD_DEFAULT, "APHookControl");
      NSDictionary *report = @{
          @"pid" : @(getpid()),
          @"traced" : @((info.kp_proc.p_flag & P_TRACED) != 0),
          @"sysctlResult" : @(sysctl_result),
          @"sysctlSize" : @(size),
          @"expectedSize" : @(sizeof info),
          @"ready" : @(control ? __atomic_load_n(control + 2, __ATOMIC_ACQUIRE) : 0),
          @"pending" : @(control ? __atomic_load_n(control + 3, __ATOMIC_ACQUIRE) : 0),
          @"objectiveC" : @([[APDeviceFixture new] value]),
          @"entry" : @(value),
          @"original" : @(original_atoi("41")),
          @"intercepted" : @(intercepted)
      };
      NSURL *documents = [NSFileManager.defaultManager URLsForDirectory:NSDocumentDirectory
                                                              inDomains:NSUserDomainMask]
                             .firstObject;
      NSData *data = [NSJSONSerialization dataWithJSONObject:report options:0 error:nil];
      [data writeToURL:[documents URLByAppendingPathComponent:@"hook-framework-check.json"]
            atomically:YES];
    });
    dispatch_resume(timer);
}
