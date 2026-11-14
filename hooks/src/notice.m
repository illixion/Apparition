// SPDX-License-Identifier: MIT
#include "APInternal.h"
#import <Foundation/Foundation.h>
#import <TargetConditionals.h>
#if TARGET_OS_IOS || TARGET_OS_VISION
#import <UIKit/UIKit.h>
#import <unistd.h>
void APShowNotice(void) {
    static int scheduled;
    if ((!__atomic_load_n(&APHookControl.pending, __ATOMIC_ACQUIRE) &&
         !__atomic_load_n(&APHookControl.failed, __ATOMIC_ACQUIRE)) ||
        __atomic_load_n(&APHookControl.ready, __ATOMIC_ACQUIRE) ||
        __atomic_exchange_n(&scheduled, 1, __ATOMIC_ACQ_REL))
        return;
    dispatch_async(dispatch_get_main_queue(), ^{
      static BOOL shown;
      NSUserDefaults *defaults = NSUserDefaults.standardUserDefaults;
      if (shown || [defaults boolForKey:@"ApparitionHooks.suppressNotice"])
          return;
      UIApplication *app = UIApplication.sharedApplication;
      if (app.applicationState != UIApplicationStateActive) {
          __atomic_store_n(&scheduled, 0, __ATOMIC_RELEASE);
          return;
      }
      UIViewController *presenter = nil;
      for (UIScene *scene in app.connectedScenes) {
          if (![scene isKindOfClass:UIWindowScene.class] ||
              scene.activationState != UISceneActivationStateForegroundActive)
              continue;
          for (UIWindow *window in ((UIWindowScene *)scene).windows) {
              if (window.isKeyWindow)
                  presenter = window.rootViewController;
          }
      }
      while (presenter.presentedViewController)
          presenter = presenter.presentedViewController;
      if (!presenter || presenter.isBeingDismissed ||
          [presenter isKindOfClass:UIAlertController.class]) {
          __atomic_store_n(&scheduled, 0, __ATOMIC_RELEASE);
          return;
      }
      shown = YES;
      BOOL pending = __atomic_load_n(&APHookControl.pending, __ATOMIC_ACQUIRE) != 0;
      NSURLComponents *link = [NSURLComponents componentsWithString:@"apparition://enable-jit"];
      link.queryItems = @[
          [NSURLQueryItem queryItemWithName:@"hooks-only" value:@"1"],
          [NSURLQueryItem queryItemWithName:@"bundle-id"
                                      value:NSBundle.mainBundle.bundleIdentifier ?: @""],
          [NSURLQueryItem queryItemWithName:@"pid"
                                      value:[NSString stringWithFormat:@"%d", getpid()]]
      ];
      BOOL canOpen = pending && [app canOpenURL:link.URL];
      NSString *message =
          pending ? (canOpen ? @"Some tweak features need JIT. Apparition can turn it on for this "
                               @"running app."
                             : @"Some tweak features need JIT. Install Apparition on this device, "
                               @"or launch this app with JIT from Apparition on your computer.")
                  : @"Some tweak hooks could not be applied on this device. The app can continue "
                    @"with the features that are available.";
      UIAlertController *alert =
          [UIAlertController alertControllerWithTitle:@"Tweak Features"
                                              message:message
                                       preferredStyle:UIAlertControllerStyleAlert];
      if (canOpen)
          [alert addAction:[UIAlertAction actionWithTitle:@"Turn On"
                                                    style:UIAlertActionStyleDefault
                                                  handler:^(__unused UIAlertAction *action) {
                                                    [app openURL:link.URL
                                                                  options:@{}
                                                        completionHandler:nil];
                                                  }]];
      [alert addAction:[UIAlertAction actionWithTitle:@"Not Now"
                                                style:UIAlertActionStyleCancel
                                              handler:nil]];
      [alert addAction:[UIAlertAction actionWithTitle:@"Don't Ask Again"
                                                style:UIAlertActionStyleDefault
                                              handler:^(__unused UIAlertAction *action) {
                                                [defaults
                                                    setBool:YES
                                                     forKey:@"ApparitionHooks.suppressNotice"];
                                              }]];
      [presenter presentViewController:alert animated:YES completion:nil];
    });
}
#else
void APShowNotice(void) {}
#endif
