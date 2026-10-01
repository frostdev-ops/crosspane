#include <ApplicationServices/ApplicationServices.h>
#include <IOKit/hidsystem/IOHIDLib.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc > 2 || (argc == 2 && strcmp(argv[1], "--request") != 0)) {
        fprintf(stderr, "Usage: CrosspaneTccProbe [--request]\n");
        return 2;
    }
    if (argc == 2) {
        CGRequestScreenCaptureAccess();
        const void *key = kAXTrustedCheckOptionPrompt;
        const void *value = kCFBooleanTrue;
        CFDictionaryRef options = CFDictionaryCreate(
            kCFAllocatorDefault, &key, &value, 1,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        if (options == NULL) {
            fprintf(stderr, "Could not create accessibility prompt options\n");
            return 1;
        }
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
        IOHIDRequestAccess(kIOHIDRequestTypeListenEvent);
    }
    printf("{\"screen_capture\":%s,\"accessibility\":%s,\"input_monitoring\":%s}\n",
           CGPreflightScreenCaptureAccess() ? "true" : "false",
           AXIsProcessTrusted() ? "true" : "false",
           IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) == kIOHIDAccessTypeGranted
               ? "true" : "false");
    return 0;
}
