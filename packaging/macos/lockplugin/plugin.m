// OpenComputerUse lock-screen authorization plugin.
//
// A SecurityAgent authorization plugin with a single mechanism. When macOS
// evaluates the screen-unlock right (`system.login.screensaver`) this
// mechanism asks the OpenComputerUse helper, over a local socket, whether an
// unlock is pending for a background session. It allows the unlock only when
// the helper says yes AND the helper is signed by our own Developer ID team;
// otherwise it stays out of the way and the normal password prompt runs.
//
// The plugin never sees or stores the login password. It only turns a
// helper-confirmed "yes" into an `Allow` result on the unlock right.

#import <Foundation/Foundation.h>
#import <Security/AuthorizationPlugin.h>
#import <Security/SecTask.h>
#import <os/log.h>
#import <sys/socket.h>
#import <sys/time.h>
#import <sys/un.h>
#import <unistd.h>
#import <errno.h>
#import <stdarg.h>
#import <stdio.h>

// SPI in the Security framework (the Codex plugin links the same symbols).
// They read what the kernel validated at exec, which is how we confirm the
// peer from the _securityagent context, where SecCode guest lookup fails.
extern CFStringRef SecTaskCopyTeamIdentifier(SecTaskRef task, CFErrorRef *error);

// Must match the helper's Developer ID team and identifier, and SOCKET_PATH
// in crates/ocu-macos/src/lock.rs.
static NSString *const kExpectedTeam = @"CS54L4CF2Z";
static NSString *const kExpectedIdentifier = @"com.infrawrench.opencomputeruse";
static const char *kSocketPath = "/Library/Application Support/OpenComputerUse/run/unlock.sock";
// The unlock waits on us, so the helper gets this long to answer.
static const int kTimeoutSeconds = 1;

static os_log_t ocu_log(void) {
    static os_log_t log;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        log = os_log_create("com.infrawrench.opencomputeruse.lockplugin", "auth");
    });
    return log;
}

// Debug trace to a file, because os_log from inside the agent (running as
// _securityagent) is hard to capture. Compiled in only with OCU_PLUGIN_TRACE.
#ifdef OCU_PLUGIN_TRACE
static void ocu_trace(const char *fmt, ...) {
    FILE *f = fopen("/Library/Application Support/OpenComputerUse/run/plugin.log", "a");
    if (!f) return;
    va_list ap; va_start(ap, fmt);
    vfprintf(f, fmt, ap);
    va_end(ap);
    fputc('\n', f);
    fclose(f);
}
#else
static void ocu_trace(const char *fmt, ...) { (void)fmt; }
#endif

typedef struct {
    const AuthorizationCallbacks *callbacks;
} PluginContext;

typedef struct {
    PluginContext *plugin;
    AuthorizationEngineRef engine;
} MechanismContext;

// Confirms the process on the other end of `fd` is our signed helper.
static BOOL peer_is_trusted_helper(int fd) {
    audit_token_t token;
    socklen_t len = sizeof(token);
    if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, &token, &len) != 0 || len != sizeof(token)) {
        os_log_error(ocu_log(), "cannot read peer audit token errno=%d", errno);
        ocu_trace("peer audit token FAILED errno=%d len=%u", errno, len);
        return NO;
    }
    SecTaskRef task = SecTaskCreateWithAuditToken(NULL, token);
    if (!task) {
        os_log_error(ocu_log(), "cannot create peer task");
        ocu_trace("SecTaskCreateWithAuditToken FAILED");
        return NO;
    }
    CFStringRef sid = SecTaskCopySigningIdentifier(task, NULL);
    CFStringRef team = SecTaskCopyTeamIdentifier(task, NULL);
    BOOL ok = sid && team
        && CFEqual(sid, (__bridge CFStringRef)kExpectedIdentifier)
        && CFEqual(team, (__bridge CFStringRef)kExpectedTeam);
    ocu_trace("peer sid=%s team=%s ok=%d",
              sid ? [(__bridge NSString *)sid UTF8String] : "(nil)",
              team ? [(__bridge NSString *)team UTF8String] : "(nil)", ok);
    if (sid) CFRelease(sid);
    if (team) CFRelease(team);
    CFRelease(task);
    if (!ok) os_log_error(ocu_log(), "peer identity mismatch");
    return ok;
}

// Asks the helper whether an unlock is pending. Returns YES only on a
// verified "allow" answer from our signed helper.
static BOOL helper_allows_unlock(void) {
    ocu_trace("helper_allows_unlock begin uid=%d", (int)getuid());
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        os_log_error(ocu_log(), "socket() errno=%d", errno);
        ocu_trace("socket() FAILED errno=%d", errno);
        return NO;
    }
    // A closed peer must not SIGPIPE the agent we run in, and a stalled one
    // must not hang the lock screen.
    int on = 1;
    struct timeval tv = { .tv_sec = kTimeoutSeconds };
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, sizeof(on));
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    strlcpy(addr.sun_path, kSocketPath, sizeof(addr.sun_path));
    BOOL allow = NO;
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0) {
        ocu_trace("connected");
        if (peer_is_trusted_helper(fd)) {
            // Protocol: send "unlock?\n", read an "allow" / "deny" line.
            const char *q = "unlock?\n";
            char buf[16] = {0};
            if (write(fd, q, strlen(q)) == (ssize_t)strlen(q)) {
                ssize_t n = read(fd, buf, sizeof(buf) - 1);
                allow = n > 0 && strncmp(buf, "allow\n", 6) == 0;
            }
            os_log(ocu_log(), "helper answered '%{public}s' allow=%d", buf, allow);
            ocu_trace("helper answered '%s' allow=%d", buf, allow);
        }
    } else {
        os_log(ocu_log(), "no helper listening errno=%d (normal when disabled)", errno);
        ocu_trace("connect FAILED errno=%d", errno);
    }
    close(fd);
    return allow;
}

static OSStatus MechanismCreate(AuthorizationPluginRef inPlugin,
                                AuthorizationEngineRef inEngine,
                                AuthorizationMechanismId mechanismId,
                                AuthorizationMechanismRef *outMechanism) {
    MechanismContext *mech = calloc(1, sizeof(MechanismContext));
    if (!mech) return errSecMemoryError;
    mech->plugin = (PluginContext *)inPlugin;
    mech->engine = inEngine;
    *outMechanism = (AuthorizationMechanismRef)mech;
    return errSecSuccess;
}

static OSStatus MechanismInvoke(AuthorizationMechanismRef inMechanism) {
    MechanismContext *mech = (MechanismContext *)inMechanism;
    // Deny is not ours to unlock: the rule falls through to the password UI.
    AuthorizationResult result = kAuthorizationResultDeny;
    if (helper_allows_unlock()) {
        os_log(ocu_log(), "allowing background unlock");
        result = kAuthorizationResultAllow;
    }
    mech->plugin->callbacks->SetResult(mech->engine, result);
    return errSecSuccess;
}

static OSStatus MechanismDeactivate(AuthorizationMechanismRef inMechanism) {
    MechanismContext *mech = (MechanismContext *)inMechanism;
    return mech->plugin->callbacks->DidDeactivate(mech->engine);
}

static OSStatus MechanismDestroy(AuthorizationMechanismRef inMechanism) {
    free(inMechanism);
    return errSecSuccess;
}

static OSStatus PluginDestroy(AuthorizationPluginRef inPlugin) {
    free(inPlugin);
    return errSecSuccess;
}

static AuthorizationPluginInterface gInterface = {
    kAuthorizationPluginInterfaceVersion,
    PluginDestroy,
    MechanismCreate,
    MechanismInvoke,
    MechanismDeactivate,
    MechanismDestroy,
};

__attribute__((visibility("default")))
OSStatus AuthorizationPluginCreate(const AuthorizationCallbacks *callbacks,
                                   AuthorizationPluginRef *outPlugin,
                                   const AuthorizationPluginInterface **outPluginInterface) {
    PluginContext *ctx = calloc(1, sizeof(PluginContext));
    if (!ctx) return errSecMemoryError;
    ctx->callbacks = callbacks;
    *outPlugin = (AuthorizationPluginRef)ctx;
    *outPluginInterface = &gInterface;
    os_log(ocu_log(), "AuthorizationPluginCreate ok");
    return errSecSuccess;
}
