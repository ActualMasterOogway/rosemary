/* Guest-facing SteamClient bridge: the dylib's entry points.
 *
 * These are the symbols Valve's libsteam_api.dylib looks up after it loads
 * steamclient.dylib. Everything they do is forwarded to the host's real Steam
 * client through rosemary_steamclient_dispatch; the only state kept on this
 * side is the table of interface wrappers handed back to the guest.
 */

#include "rosemary_steam.h"
#include "steamclient_methods.h"

#define EXPORT extern "C" __attribute__((visibility("default")))

namespace {

int streq(const char *a, const char *b)
{
    while (*a && *a == *b) { ++a; ++b; }
    return *a == *b;
}

/* A spinlock, because a freestanding dylib has no pthreads. Contention is
 * limited to interface creation, which games do a handful of times. */
class Lock
{
public:
    Lock() { while (__atomic_exchange_n(&locked, 1, __ATOMIC_ACQUIRE)) pause(); }
    ~Lock() { __atomic_store_n(&locked, 0, __ATOMIC_RELEASE); }

private:
    static void pause()
    {
#if defined(__x86_64__)
        __asm__ __volatile__("pause");
#elif defined(__aarch64__)
        __asm__ __volatile__("yield");
#endif
    }

    static int locked;
};

int Lock::locked = 0;

/* Interfaces are cached by (version, host pointer): a guest that asks for the
 * same interface twice must get the same object back, and the dylib cannot
 * allocate. STEAM_IFACE_POOL is well above what a game creates - one object
 * per interface version per pipe. */
struct PoolEntry
{
    const char *version;
    void *u_iface;
    void *iface;
    alignas(16) unsigned char storage[STEAM_IFACE_STORAGE];
};

PoolEntry pool[STEAM_IFACE_POOL];
unsigned int pool_used;

} // namespace

void *steam_wrap_interface(const void *name, void *u_iface)
{
    const char *version = static_cast<const char *>(name);
    if (!version || !u_iface)
        return nullptr;

    Lock guard;

    for (unsigned int i = 0; i < pool_used; ++i)
        if (pool[i].u_iface == u_iface && streq(pool[i].version, version))
            return pool[i].iface;

    for (unsigned int i = 0; i < steam_iface_table_size; ++i)
    {
        if (!streq(steam_iface_table[i].version, version))
            continue;
        if (pool_used == STEAM_IFACE_POOL)
            return nullptr;

        PoolEntry &entry = pool[pool_used];
        entry.version = steam_iface_table[i].version;
        entry.u_iface = u_iface;
        entry.iface = steam_iface_table[i].ctor(entry.storage, u_iface);
        ++pool_used;
        return entry.iface;
    }

    /* An interface version this build does not know. Regenerating against a
     * newer SDK is the fix; guessing a vtable here would corrupt the guest. */
    return nullptr;
}

EXPORT void *CreateInterface(const char *name, int32_t *return_code)
{
    steam_params_create_interface params = {};
    params.name = name;
    params.return_code = return_code;

    if (rosemary_steamclient_dispatch(STEAM_DISPATCH_CREATE_INTERFACE, &params))
        return nullptr;
    if (!params._ret)
        return nullptr;
    return steam_wrap_interface(name, params._ret);
}

EXPORT int8_t Steam_IsKnownInterface(const char *version)
{
    steam_params_name_only params = {};
    params.name = version;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_IS_KNOWN_INTERFACE, &params);
    return params._ret;
}

EXPORT void Steam_NotifyMissingInterface(int32_t pipe, const char *version)
{
    steam_params_notify_missing params = {};
    params.pipe = pipe;
    params.name = version;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_NOTIFY_MISSING_INTERFACE, &params);
}

EXPORT int8_t Steam_BGetCallback(int32_t pipe, void *msg, int32_t *ignored)
{
    steam_params_get_callback params = {};
    params.pipe = pipe;
    params.msg = msg;
    params.ignored = ignored;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_GET_CALLBACK, &params);
    return params._ret;
}

EXPORT int8_t Steam_FreeLastCallback(int32_t pipe)
{
    steam_params_pipe_only params = {};
    params.pipe = pipe;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_FREE_LAST_CALLBACK, &params);
    return params._ret;
}

EXPORT int8_t Steam_GetAPICallResult(int32_t pipe, uint64_t call, void *callback,
                                     int32_t callback_size, int32_t callback_expected,
                                     bool *failed)
{
    steam_params_get_api_call_result params = {};
    params.pipe = pipe;
    params.call = call;
    params.callback = callback;
    params.callback_size = callback_size;
    params.callback_expected = callback_expected;
    params.failed = failed;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_GET_API_CALL_RESULT, &params);
    return params._ret;
}

EXPORT void Steam_ReleaseThreadLocalMemory(int32_t thread_exit)
{
    steam_params_release_tls params = {};
    params.thread_exit = thread_exit;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_RELEASE_THREAD_LOCAL_MEMORY, &params);
}

EXPORT void Breakpad_SteamMiniDumpInit(uint32_t a, const char *b, const char *c)
{
    steam_params_breakpad_init params = {};
    params.a = a;
    params.b = b;
    params.c = c;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_BREAKPAD_MINIDUMP_INIT, &params);
}

EXPORT void Breakpad_SteamSetAppID(uint32_t appid)
{
    steam_params_breakpad_appid params = {};
    params.appid = appid;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_BREAKPAD_SET_APPID, &params);
}

EXPORT int32_t Breakpad_SteamSetSteamID(uint64_t steamid)
{
    steam_params_breakpad_steamid params = {};
    params.steamid = steamid;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_BREAKPAD_SET_STEAMID, &params);
    return params._ret;
}

EXPORT int32_t Breakpad_SteamWriteMiniDumpSetComment(const char *comment)
{
    steam_params_breakpad_comment params = {};
    params.comment = comment;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_BREAKPAD_WRITE_MINIDUMP_SET_COMMENT,
                                  &params);
    return params._ret;
}

EXPORT void Breakpad_SteamWriteMiniDumpUsingExceptionInfoWithBuildId(int32_t a, int32_t b)
{
    steam_params_breakpad_minidump params = {};
    params.a = a;
    params.b = b;
    rosemary_steamclient_dispatch(STEAM_DISPATCH_BREAKPAD_WRITE_MINIDUMP_WITH_BUILD_ID,
                                  &params);
}
