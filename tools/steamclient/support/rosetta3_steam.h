/* Guest-facing SteamClient bridge: shared declarations.
 *
 * This header is compiled into a macOS dylib without a macOS SDK, so it may
 * not include anything: every type it needs is spelled out with compiler
 * builtin macros. The generated sources include only this file.
 */

#ifndef ROSETTA3_STEAM_H
#define ROSETTA3_STEAM_H

typedef __INT8_TYPE__ int8_t;
typedef __UINT8_TYPE__ uint8_t;
typedef __INT16_TYPE__ int16_t;
typedef __UINT16_TYPE__ uint16_t;
typedef __INT32_TYPE__ int32_t;
typedef __UINT32_TYPE__ uint32_t;
typedef __INT64_TYPE__ int64_t;
typedef __UINT64_TYPE__ uint64_t;
typedef __INTPTR_TYPE__ intptr_t;
typedef __UINTPTR_TYPE__ uintptr_t;
typedef __SIZE_TYPE__ size_t;

/* Placement new, without <new>. */
inline void *operator new(size_t, void *where) { return where; }

/* ---------------------------------------------------------------------------
 * Types passed or returned by value.
 *
 * The bridge treats every other SDK type as opaque: pointers and references
 * cross as pointers, and enums as int32_t. These eleven decide a calling
 * convention, so their layout is reproduced exactly. They are ABI stand-ins,
 * not the SDK's classes - only member types, order and packing matter.
 * -------------------------------------------------------------------------*/

#pragma pack(push, 8)

struct CSteamID { uint64_t m_steamid; };
struct CGameID { uint64_t m_ulGameID; };

struct SteamIPAddress_t
{
    union { uint32_t m_unIPv4; uint8_t m_rgubIPv6[16]; uint64_t m_ipv6Qword[2]; } x;
    int32_t m_eType;
};

/* Controller*ActionData_t are #defined to these in every SDK that has both. */
struct InputAnalogActionData_t { int32_t eMode; float x; float y; bool bActive; };
struct InputDigitalActionData_t { bool bState; bool bActive; };
struct InputMotionData_t
{
    float rotQuatX, rotQuatY, rotQuatZ, rotQuatW;
    float posAccelX, posAccelY, posAccelZ;
    float rotVelX, rotVelY, rotVelZ;
};

struct SteamPartyBeaconLocation_t { int32_t m_eType; uint64_t m_ulLocationID; };

struct RemoteStorageUpdatePublishedFileRequest_t
{
    uint64_t m_unPublishedFileId;
    const char *m_pchFile;
    const char *m_pchPreviewFile;
    const char *m_pchTitle;
    const char *m_pchDescription;
    int32_t m_eVisibility;
    void *m_pTags;
    bool m_bUpdateFile;
    bool m_bUpdatePreviewFile;
    bool m_bUpdateTitle;
    bool m_bUpdateDescription;
    bool m_bUpdateVisibility;
    bool m_bUpdateTags;
};

#pragma pack(pop)

static_assert(sizeof(CSteamID) == 8, "CSteamID layout");
static_assert(sizeof(CGameID) == 8, "CGameID layout");
static_assert(sizeof(SteamIPAddress_t) == 24, "SteamIPAddress_t layout");
static_assert(sizeof(InputAnalogActionData_t) == 16, "InputAnalogActionData_t layout");
static_assert(sizeof(InputDigitalActionData_t) == 2, "InputDigitalActionData_t layout");
static_assert(sizeof(InputMotionData_t) == 40, "InputMotionData_t layout");
static_assert(sizeof(SteamPartyBeaconLocation_t) == 16, "SteamPartyBeaconLocation_t layout");
static_assert(sizeof(RemoteStorageUpdatePublishedFileRequest_t) == 64,
              "RemoteStorageUpdatePublishedFileRequest_t layout");

/* ---------------------------------------------------------------------------
 * Host dispatch.
 *
 * Every generated method packs its arguments into a params struct and calls
 * out once. The symbol is left undefined on purpose: Rosetta 3 resolves it
 * through its import bridge, the same way it resolves libc for a guest.
 * -------------------------------------------------------------------------*/

enum steam_dispatch_status
{
    STEAM_DISPATCH_OK = 0,
    STEAM_DISPATCH_UNKNOWN_METHOD = 1,
    STEAM_DISPATCH_NO_CLIENT = 2,
};

extern "C" int32_t rosetta3_steamclient_dispatch(uint32_t method, void *params);

#pragma pack(push, 1)

struct steam_params_create_interface
{
    const char *name;
    int32_t *return_code;   /* the caller's out-parameter, written by the host */
    void *_ret;             /* out: host-side interface pointer */
};

struct steam_params_name_only { const char *name; int8_t _ret; };
struct steam_params_pipe_only { int32_t pipe; int8_t _ret; };
struct steam_params_notify_missing { int32_t pipe; const char *name; };
struct steam_params_get_callback { int32_t pipe; void *msg; int32_t *ignored; int8_t _ret; };
struct steam_params_get_api_call_result
{
    int32_t pipe;
    uint64_t call;
    void *callback;
    int32_t callback_size;
    int32_t callback_expected;
    bool *failed;
    int8_t _ret;
};
struct steam_params_release_tls { int32_t thread_exit; };
struct steam_params_breakpad_init { uint32_t a; const char *b; const char *c; };
struct steam_params_breakpad_appid { uint32_t appid; };
struct steam_params_breakpad_steamid { uint64_t steamid; int32_t _ret; };
struct steam_params_breakpad_comment { const char *comment; int32_t _ret; };
struct steam_params_breakpad_minidump { int32_t a; int32_t b; };

#pragma pack(pop)

/* ---------------------------------------------------------------------------
 * Interface objects.
 *
 * Each generated class is a vtable plus the host-side pointer it forwards to.
 * Objects come from a fixed pool: the dylib has no allocator, and a guest
 * asking for the same interface twice must get the same pointer back, the way
 * the real client behaves.
 * -------------------------------------------------------------------------*/

/* Every generated interface is a vtable pointer plus one host pointer; the
 * pool is sized for one object per interface version per pipe, with room. */
#define STEAM_IFACE_STORAGE 32
#define STEAM_IFACE_POOL 256

struct steam_iface
{
    void *u_iface;

    /* Interface objects live in the pool and are never freed, but an
     * interface with a virtual destructor gets a deleting destructor slot
     * that has to name an operator delete. A member one keeps that inside the
     * class: a global replacement would interpose on the guest's own. */
    static void operator delete(void *) {}
    static void operator delete(void *, size_t) {}
};

/* Constructors hand back the address of the object, not of its steam_iface
 * base: the vtable pointer is at offset 0 and the base subobject follows it,
 * so the guest must be given the object itself. */
typedef void *(*steam_iface_ctor)(void *storage, void *u_iface);

struct steam_iface_entry
{
    const char *version;
    steam_iface_ctor ctor;
};

/* Generated: the version string -> constructor table, and its length. */
extern const struct steam_iface_entry steam_iface_table[];
extern const unsigned int steam_iface_table_size;

#endif /* ROSETTA3_STEAM_H */
