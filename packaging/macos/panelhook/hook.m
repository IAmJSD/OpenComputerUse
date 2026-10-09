// OpenComputerUse panel hook.
//
// Loaded into an app OpenComputerUse starts (DYLD_INSERT_LIBRARIES, only for
// apps whose signature lets it in), it answers the app's open and save
// panels from the agent instead of showing them. Each panel the app runs
// becomes a request over a Unix socket (OCU_PANEL_SOCKET); the reply is the
// paths to pick, a cancel, or "show", which runs the real panel. When the
// socket cannot be reached the real panel runs, so the app behaves as it
// would without the hook.
//
// The hook removes its own variables from the environment as it loads, so
// what the app starts in turn runs without it.

#import <AppKit/AppKit.h>
#import <objc/runtime.h>
#import <stdlib.h>
#import <sys/socket.h>
#import <sys/un.h>
#import <unistd.h>

static NSString *socketPath;
static const void *kOverride = &kOverride;

static IMP originalRunModal[2];
static IMP originalBeginSheet[2];
static IMP originalBegin[2];
static IMP originalURL;
static IMP originalURLs;

// 0 for NSSavePanel's own methods, 1 for NSOpenPanel's.
static int slot(id panel, SEL sel) {
    Method mine = class_getInstanceMethod([NSOpenPanel class], sel);
    Method base = class_getInstanceMethod([NSSavePanel class], sel);
    BOOL own = mine != base;
    return ([panel isKindOfClass:[NSOpenPanel class]] && own) ? 1 : 0;
}

// What the agent said: paths to pick, cancel, or show the real panel.
typedef NS_ENUM(NSInteger, Verdict) { VerdictShow, VerdictPick, VerdictCancel };

static NSDictionary *describe(NSSavePanel *panel) {
    BOOL open = [panel isKindOfClass:[NSOpenPanel class]];
    NSMutableDictionary *d = [@{
        @"pid" : @(getpid()),
        @"kind" : open ? @"open" : @"save",
    } mutableCopy];
    if (open) {
        NSOpenPanel *o = (NSOpenPanel *)panel;
        d[@"multiple"] = @(o.allowsMultipleSelection);
        d[@"files"] = @(o.canChooseFiles);
        d[@"directories"] = @(o.canChooseDirectories);
    } else if (panel.nameFieldStringValue.length) {
        d[@"name"] = panel.nameFieldStringValue;
    }
    if (panel.directoryURL.path) d[@"directory"] = panel.directoryURL.path;
    if (panel.message.length) d[@"message"] = panel.message;
    if (panel.title.length) d[@"title"] = panel.title;
    return d;
}

// Asks the agent, blocking the calling thread (never the main one).
static Verdict ask(NSDictionary *request, NSArray<NSURL *> **urls) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return VerdictShow;
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    strlcpy(addr.sun_path, socketPath.fileSystemRepresentation, sizeof addr.sun_path);
    if (connect(fd, (struct sockaddr *)&addr, sizeof addr) != 0) {
        close(fd);
        return VerdictShow;
    }
    NSMutableData *line = [[NSJSONSerialization dataWithJSONObject:request options:0 error:nil] mutableCopy];
    [line appendBytes:"\n" length:1];
    if (write(fd, line.bytes, line.length) != (ssize_t)line.length) {
        close(fd);
        return VerdictShow;
    }
    NSMutableData *reply = [NSMutableData data];
    char buf[4096];
    for (;;) {
        ssize_t n = read(fd, buf, sizeof buf);
        if (n <= 0) break;
        [reply appendBytes:buf length:n];
        if (memchr(buf, '\n', n)) break;
    }
    close(fd);
    // The agent went away without answering: as if the user cancelled.
    if (reply.length == 0) return VerdictCancel;
    NSDictionary *answer = [NSJSONSerialization JSONObjectWithData:reply options:0 error:nil];
    if (![answer isKindOfClass:[NSDictionary class]] || [answer[@"show"] boolValue]) return VerdictShow;
    NSArray *paths = answer[@"paths"];
    if (![paths isKindOfClass:[NSArray class]] || paths.count == 0) return VerdictCancel;
    NSMutableArray *list = [NSMutableArray array];
    for (NSString *p in paths) {
        if ([p isKindOfClass:[NSString class]]) [list addObject:[NSURL fileURLWithPath:p]];
    }
    *urls = list;
    return list.count ? VerdictPick : VerdictCancel;
}

// Puts the picks in place for the panel's URL and URLs. Main thread.
static void apply(NSSavePanel *panel, Verdict v, NSArray<NSURL *> *urls) {
    if (v != VerdictPick) return;
    objc_setAssociatedObject(panel, kOverride, urls, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    if (urls.firstObject && ![panel isKindOfClass:[NSOpenPanel class]]) {
        panel.directoryURL = [urls.firstObject URLByDeletingLastPathComponent];
        panel.nameFieldStringValue = urls.firstObject.lastPathComponent;
    }
}

// Answers the panel off the main thread, then calls `done` on the main
// queue with the verdict, the picks in place.
static void answer(NSSavePanel *panel, void (^done)(Verdict)) {
    NSDictionary *request = describe(panel);
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        NSArray<NSURL *> *urls = nil;
        Verdict v = ask(request, &urls);
        dispatch_async(dispatch_get_main_queue(), ^{
            apply(panel, v, urls);
            done(v);
        });
    });
}

static NSModalResponse hookedRunModal(NSSavePanel *self, SEL _cmd) {
    IMP original = originalRunModal[slot(self, _cmd)];
    NSDictionary *request = describe(self);
    __block NSArray<NSURL *> *urls = nil;
    __block Verdict verdict = VerdictShow;
    dispatch_semaphore_t answered = dispatch_semaphore_create(0);
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        NSArray<NSURL *> *picked = nil;
        verdict = ask(request, &picked);
        urls = picked;
        dispatch_semaphore_signal(answered);
    });
    // The run loop turns while the agent decides, so the app keeps drawing,
    // as under a modal panel. Not the main queue: runModal is often called
    // from a block on it, which would hold back one queued there.
    while (dispatch_semaphore_wait(answered, DISPATCH_TIME_NOW) != 0) {
        @autoreleasepool {
            [[NSRunLoop currentRunLoop] runMode:NSDefaultRunLoopMode
                                     beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.05]];
        }
    }
    apply(self, verdict, urls);
    switch (verdict) {
    case VerdictShow:
        return ((NSModalResponse(*)(id, SEL))original)(self, _cmd);
    case VerdictPick:
        return NSModalResponseOK;
    case VerdictCancel:
        return NSModalResponseCancel;
    }
}

static void hookedBeginSheet(NSSavePanel *self, SEL _cmd, NSWindow *window, void (^handler)(NSModalResponse)) {
    IMP original = originalBeginSheet[slot(self, _cmd)];
    void (^h)(NSModalResponse) = [handler copy];
    answer(self, ^(Verdict v) {
        if (v == VerdictShow) {
            ((void (*)(id, SEL, NSWindow *, id))original)(self, _cmd, window, h);
        } else if (h) {
            h(v == VerdictPick ? NSModalResponseOK : NSModalResponseCancel);
        }
    });
}

static void hookedBegin(NSSavePanel *self, SEL _cmd, void (^handler)(NSModalResponse)) {
    IMP original = originalBegin[slot(self, _cmd)];
    void (^h)(NSModalResponse) = [handler copy];
    answer(self, ^(Verdict v) {
        if (v == VerdictShow) {
            ((void (*)(id, SEL, id))original)(self, _cmd, h);
        } else if (h) {
            h(v == VerdictPick ? NSModalResponseOK : NSModalResponseCancel);
        }
    });
}

static NSURL *hookedURL(NSSavePanel *self, SEL _cmd) {
    NSArray<NSURL *> *urls = objc_getAssociatedObject(self, kOverride);
    if (urls) return urls.firstObject;
    return ((NSURL * (*)(id, SEL)) originalURL)(self, _cmd);
}

static NSArray<NSURL *> *hookedURLs(NSOpenPanel *self, SEL _cmd) {
    NSArray<NSURL *> *urls = objc_getAssociatedObject(self, kOverride);
    if (urls) return urls;
    return ((NSArray * (*)(id, SEL)) originalURLs)(self, _cmd);
}

// Replaces `sel` on `cls` with `imp`, keeping what it replaced in `slot`.
// Only where the class has the method itself; a subclass that inherits it
// is covered through its superclass.
static void replace(Class cls, SEL sel, IMP imp, IMP *saved) {
    unsigned int count = 0;
    Method *methods = class_copyMethodList(cls, &count);
    for (unsigned int i = 0; i < count; i++) {
        if (method_getName(methods[i]) == sel) {
            *saved = method_setImplementation(methods[i], imp);
            break;
        }
    }
    free(methods);
}

__attribute__((constructor)) static void install(void) {
    const char *path = getenv("OCU_PANEL_SOCKET");
    // Gone from the environment before anything is started from here.
    unsetenv("DYLD_INSERT_LIBRARIES");
    unsetenv("OCU_PANEL_SOCKET");
    if (!path || !*path) return;
    // A sandboxed app cannot reach the socket, and could not open a file
    // it was handed without the panel granting it.
    if (getenv("APP_SANDBOX_CONTAINER_ID")) return;
    Class save = NSClassFromString(@"NSSavePanel");
    Class open = NSClassFromString(@"NSOpenPanel");
    if (!save || !open) return;
    socketPath = [NSString stringWithUTF8String:path];
    Class classes[2] = {save, open};
    for (int i = 0; i < 2; i++) {
        replace(classes[i], @selector(runModal), (IMP)hookedRunModal, &originalRunModal[i]);
        replace(classes[i], @selector(beginSheetModalForWindow:completionHandler:), (IMP)hookedBeginSheet,
                &originalBeginSheet[i]);
        replace(classes[i], @selector(beginWithCompletionHandler:), (IMP)hookedBegin, &originalBegin[i]);
    }
    replace(save, @selector(URL), (IMP)hookedURL, &originalURL);
    replace(open, @selector(URLs), (IMP)hookedURLs, &originalURLs);
}
