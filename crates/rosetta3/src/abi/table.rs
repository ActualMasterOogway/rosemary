//! What the runtime knows about the functions a guest imports.
//!
//! ## Why a hand-written table
//!
//! A Mach-O symbol table records names, not types, and the host's headers are
//! not available to a running loader. The only honest source of a signature is
//! the embedder, so this is a table — and, per the milestone's scope note, a
//! table written for the symbols real programs import rather than for all of
//! libSystem. It grows when something fails, and a failure names the symbol.
//!
//! ## Three answers, not two
//!
//! [`lookup`] distinguishes *unknown* from *refused*, and the distinction is
//! the useful one. `_sigaction` is not missing from this table by accident: it
//! installs a guest function pointer as a signal handler, and the handler would
//! be entered with a *host* `ucontext_t` the guest cannot read. Recording that
//! as [`Lookup::Refused`] with a reason turns a crash into a sentence, and keeps
//! it out of the "someone should write this signature" list where it would sit
//! forever.
//!
//! A refusal is also the honest record of a milestone boundary, and it moves
//! when the milestone does: `qsort`, `qsort_r`, `bsearch` and `atexit` were
//! refused for taking a guest function pointer, and are now ordinary entries
//! whose pointer argument is marked in [`FunctionAbi::callbacks`] and
//! substituted with a thunk. Nothing about their signatures changed.
//!
//! [`REFUSED`] therefore holds three kinds of thing:
//!
//! - **Data**, which is imported through a slot and read, never called.
//! - **Callback takers whose callback the thunk cannot yet carry**: a signal
//!   handler, which would be entered with a host `ucontext_t` (M5).
//!   `pthread_create` was here until M4 gave a new host thread its own guest
//!   stack and register context; nothing about its signature changed either.
//! - **Stack walkers**, where passthrough is not merely unimplemented but
//!   *wrong*: the host's unwinder cannot walk guest frames, so `pthread_cancel`
//!   is refused (M5). `setjmp` and `longjmp` were here for the same reason and
//!   are now ordinary entries with a shim behind them — the signature says what
//!   the guest passes, and [`crate::jmpbuf`] does the part the host cannot.
//!
//! ## Names
//!
//! Keys are spelled without the leading underscore a Mach-O adds, so `_strlen`
//! is looked up as `strlen` and `___stdoutp` as `__stdoutp`. [`lookup`] also
//! retries with an x86-only symbol variant suffix removed — `$INODE64`,
//! `$UNIX2003`, `$1050` — mirroring the same fallback `machish`'s native
//! provider applies when *resolving* the symbol, for the same reason: arm64
//! never had the variant and the signature is the plain one's.
//!
//! ## Tags
//!
//! `tag` is the provider's verbatim echo (see the crate note on the seam). No
//! provider in this workspace assigns one yet, so every tag is zero and lookup
//! falls through to the name. The parameter is threaded now, rather than added
//! later, because a provider that *does* tag — a virtualized shim library, say,
//! which knows exactly which function it handed back — must be able to name a
//! signature without a string comparison.

use super::ArgType as T;

/// One function's signature, as microlift wants it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionAbi {
    /// The name *without* the Mach-O underscore, and `&'static str` because
    /// that is what `DynamicSymbol` requires.
    pub name: &'static str,
    /// `[0]` is the return type, `[1..]` the parameters — the layout
    /// `DynamicSymbol::arg_types` uses, so no reshuffling happens at the seam.
    pub types: &'static [T],
    /// C-variadic. Never fast-called, and only callable if [`crate::shims`]
    /// implements it: the two platforms disagree about where variadic arguments
    /// live, so there is no generic forwarding path.
    pub variadic: bool,
    /// Zero-based parameter indices that are **guest function pointers**.
    ///
    /// A pointer is a pointer to the marshaller; this is the one place the
    /// distinction matters, because handing the host an x86-64 address it will
    /// call as arm64 is the failure mode host→guest thunks exist to prevent.
    /// Each listed argument is substituted with a thunk on its way out — see
    /// [`crate::thunk`] — which is also why a signature carrying one can never
    /// be a *fast* import: the lifter would inline the call and pass the raw
    /// pointer.
    pub callbacks: &'static [u8],
}

impl FunctionAbi {
    pub fn ret(&self) -> T {
        self.types.first().copied().unwrap_or(T::None)
    }

    pub fn params(&self) -> &'static [T] {
        self.types.split_first().map_or(&[], |(_, rest)| rest)
    }
}

/// What the table has to say about a symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    Known(&'static FunctionAbi),
    /// Described, and deliberately not callable. The payload is why.
    Refused(&'static str),
    Unknown,
}

/// Strip the leading underscore a Mach-O import carries.
///
/// Exactly one, not all of them: `___stdoutp` is the C identifier `__stdoutp`,
/// and eating every underscore would collide it with `_stdoutp`.
fn c_name(symbol: &str) -> &str {
    symbol.strip_prefix('_').unwrap_or(symbol)
}

/// Look a symbol up by tag first, then by name.
pub fn lookup(tag: u64, symbol: &str) -> Lookup {
    if tag != 0
        && let Some(abi) = by_tag(tag)
    {
        return Lookup::Known(abi);
    }
    let name = c_name(symbol);
    match lookup_exact(name) {
        Lookup::Unknown => {
            if let Some(reason) = data_family(name) {
                return Lookup::Refused(reason);
            }
            match name.split_once('$') {
                // `_fts_open$INODE64` and friends: the variant only ever existed
                // on x86, and its signature is the plain symbol's.
                Some((base, _)) if !base.is_empty() => lookup_exact(base),
                _ => Lookup::Unknown,
            }
        }
        found => found,
    }
}

/// Data imports whose names carry a suffix, so [`REFUSED`] cannot list them.
///
/// Objective-C metadata is the only family in this shape: a class reference is
/// `_OBJC_CLASS_$_NSString`, one name per class the guest mentions, and an image
/// may mention hundreds. They are read through their slots and never called —
/// `_OBJC_CLASS_$_NSObject` is the *superclass field* of a guest class, which
/// [`crate::objc`] follows at registration — so refusing them by family says
/// exactly what listing them one by one would, and stays true for a class nobody
/// has heard of.
///
/// The alternative is that a real Objective-C program reports a dozen imports as
/// [`Lookup::Unknown`], which is the audit's word for "nobody has described
/// this" and would drown the one that really is undescribed.
fn data_family(name: &str) -> Option<&'static str> {
    const DATA: &str = "an Objective-C metadata import, read through its slot and never called";
    let families = [
        "OBJC_CLASS_$_",
        "OBJC_METACLASS_$_",
        "OBJC_IVAR_$_",
        // The type object a `@catch` clause matches against, which is data for
        // the same reason a C++ `type_info` is.
        "OBJC_EHTYPE_",
        "OBJC_PROTOCOL_$_",
        "OBJC_LABEL_PROTOCOL_$_",
        // The empty cache and vtable every `objc_class` the compiler emits
        // points at, and the `isa` of a constant `@"..."` string.
        "_objc_empty_cache",
        "_objc_empty_vtable",
        "__CFConstantStringClassReference",
    ];
    families.iter().any(|f| name.starts_with(f)).then_some(DATA)
}

fn lookup_exact(name: &str) -> Lookup {
    match index().get(name) {
        Some(Described::Known(abi)) => Lookup::Known(abi),
        Some(Described::Refused(reason)) => Lookup::Refused(reason),
        None => Lookup::Unknown,
    }
}

/// What the table says about one name: a callable signature, or a refusal.
#[derive(Clone, Copy)]
enum Described {
    Known(&'static FunctionAbi),
    Refused(&'static str),
}

/// [`SIGNATURES`] and [`REFUSED`], by name, built once.
///
/// The tables themselves are written in whatever order reads best — the C
/// library's functions grouped by what they are for, an Objective-C family kept
/// together with the paragraph explaining it — and this is what makes that
/// possible. An earlier revision searched them with `binary_search`, which
/// meant every entry had to be filed in `strcmp` order by hand: a
/// misplaced entry did not fail to compile, it silently reported a described
/// function as **unknown**, and the only thing standing between that and a
/// program stopping at run time was a test asserting the whole array was
/// sorted. Sorting a few hundred `&'static str`s once, on the first import
/// call, costs less than the first region this runtime lifts.
///
/// A duplicate name is still a mistake, and it is still a *test*
/// ([`tests::signatures_are_unique`]) rather than something this can detect —
/// building a map is where a duplicate becomes invisible, so nothing here
/// should ever be the thing that notices.
fn index() -> &'static std::collections::HashMap<&'static str, Described> {
    static INDEX: std::sync::OnceLock<std::collections::HashMap<&'static str, Described>> =
        std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        let mut map = std::collections::HashMap::with_capacity(SIGNATURES.len() + REFUSED.len());
        for (name, reason) in REFUSED {
            map.insert(*name, Described::Refused(reason));
        }
        // Signatures last, so that if a name is somehow in both, the callable
        // answer wins rather than the refusal — the same way the tables would
        // be read in order by hand.
        for abi in SIGNATURES {
            map.insert(abi.name, Described::Known(abi));
        }
        map
    })
}

/// Signatures reachable by a provider-assigned tag.
///
/// Empty today — see the module note. The indirection exists so that adding a
/// tagging provider is a change in one place rather than a new mechanism.
fn by_tag(_tag: u64) -> Option<&'static FunctionAbi> {
    None
}

/// `(ret, args...)`, in `DynamicSymbol` order.
macro_rules! sig {
    ($name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi { name: $name, types: &[$ret $(, $arg)*], variadic: false, callbacks: &[] }
    };
}

/// As [`sig`], for a C-variadic function. The fixed arguments are listed; the
/// ellipsis is implied by the flag.
macro_rules! vsig {
    ($name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi {
            name: $name,
            types: &[$ret $(, $arg)* , T::VariadicList],
            variadic: true,
            callbacks: &[],
        }
    };
}

/// As [`sig`], for a function taking guest function pointers. `$cb` lists the
/// zero-based parameter indices that are callbacks.
macro_rules! cbsig {
    ([$($cb:literal),* $(,)?], $name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi {
            name: $name,
            types: &[$ret $(, $arg)*],
            variadic: false,
            callbacks: &[$($cb),*],
        }
    };
}

const V: T = T::None;
const I32: T = T::SignedWord;
const U32: T = T::UnsignedWord;
const I64: T = T::SignedDoubleword;
const U64: T = T::UnsignedDoubleword;
const F32: T = T::SinglePrecisionFp;
const F64: T = T::DoublePrecisionFp;
const P: T = T::Pointer;
const U8: T = T::UnsignedByte;
const U16: T = T::UnsignedHalfword;
const BOOL: T = U8;

/// Known signatures, in **no particular order** — [`index`] builds the lookup,
/// so a new entry goes wherever it reads best. Names must still be unique;
/// `signatures_are_unique` is what says so.
///
/// Types are the C declarations, narrowed where C narrows: `size_t` is `U64`,
/// `int` is `I32`, every pointer is `P` regardless of what it points at. A
/// function whose real signature passes or returns a struct by value does not
/// belong here at all — see [`super::marshal`].
///
/// Mostly alphabetical anyway, because that is how a reader finds one; where a
/// family reads better together, it is together. `vsig!` marks a variadic.
pub static SIGNATURES: &[FunctionAbi] = &[
    sig!("AudioComponentFindNext", P, P, P),
    sig!("AudioComponentInstanceDispose", I32, P),
    sig!("AudioComponentInstanceNew", I32, P, P),
    // ## CoreAudio and AudioUnit
    //
    // A GameMaker runner mixes its own audio and pushes it through a single
    // output unit, so the render callback is the one that matters — it is
    // installed as an `AURenderCallbackStruct` property rather than as an
    // argument, which is why `AudioUnitSetProperty` is an ordinary pointer here
    // and the callback problem shows up in [`crate::shims::audiounit`] instead.
    // `inListener` is a guest function pointer CoreAudio calls back on a
    // property-change notification, not from inside this call — an ordinary
    // callback taker, like `CVDisplayLinkSetOutputCallback` above.
    cbsig!([2], "AudioObjectAddPropertyListener", I32, U32, P, P, P),
    sig!("AudioObjectGetPropertyData", I32, U32, P, U32, P, P, P),
    sig!("AudioObjectGetPropertyDataSize", I32, U32, P, U32, P, P),
    // The removal takes the same listener, and it must be the *same pointer* the
    // host was given — so it goes out as a thunk too, and [`crate::thunk`]
    // hands back the one already made for that guest address rather than a
    // second trampoline CoreAudio would not recognise.
    cbsig!([2], "AudioObjectRemovePropertyListener", I32, U32, P, P, P),
    sig!("AudioOutputUnitStart", I32, P),
    sig!("AudioOutputUnitStop", I32, P),
    // ## AudioQueue, which is the other way a program plays sound
    //
    // SDL's CoreAudio backend uses an audio queue rather than an output unit,
    // and the difference that matters here is where the callback goes: an
    // `AudioUnit` takes one inside a structure — the case
    // [`crate::shims::audiounit`] exists for — while a queue takes one as an
    // ordinary **argument**, so `cbsig!` describes it and the thunk is made at
    // the seam like every other.
    //
    // The callback is `(void *userData, AudioQueueRef, AudioQueueBufferRef)`
    // for output and four arguments for input, all integer and all `void`. It
    // arrives on whichever run loop was named, or on a thread CoreAudio owns
    // when that is null — which is the same "a host thread this runtime never
    // created" case the render callback already is.
    //
    // Every other argument is a scalar or a pointer: an `OSStatus` and an
    // `AudioQueuePropertyID` are 32-bit, a `Boolean` is a byte, and the format
    // and timestamp structures are read by CoreAudio in place.
    sig!("AudioQueueAllocateBuffer", I32, P, U32, P),
    sig!("AudioQueueDispose", I32, P, BOOL),
    sig!("AudioQueueEnqueueBuffer", I32, P, P, U32, P),
    sig!("AudioQueueFreeBuffer", I32, P, P),
    cbsig!([1], "AudioQueueNewInput", I32, P, P, P, P, P, U32, P),
    cbsig!([1], "AudioQueueNewOutput", I32, P, P, P, P, P, U32, P),
    sig!("AudioQueueSetProperty", I32, P, U32, P, U32),
    sig!("AudioQueueStart", I32, P, P),
    sig!("AudioQueueStop", I32, P, BOOL),
    sig!("AudioUnitGetProperty", I32, P, U32, U32, U32, P, P),
    sig!("AudioUnitInitialize", I32, P),
    sig!("AudioUnitRender", I32, P, P, P, U32, U32, P),
    sig!("AudioUnitSetProperty", I32, P, U32, U32, U32, P, U32),
    sig!("AudioUnitUninitialize", I32, P),
    // ## `AUGraph`, which is the pre-`AVAudioEngine` way to wire units together
    //
    // A graph is an opaque `AUGraph`; a node is an `AUNode`, which is a bare
    // `SInt32` and not a handle. Everything returns an `OSStatus`.
    // `AUGraphAddNode`'s second argument is an `AudioComponentDescription *` —
    // a pointer, because the modern form takes the description by address where
    // the deprecated `AUGraphNewNode` took it by value.
    sig!("NewAUGraph", I32, P),
    sig!("DisposeAUGraph", I32, P),
    sig!("AUGraphAddNode", I32, P, P, P),
    sig!("AUGraphConnectNodeInput", I32, P, I32, U32, I32, U32),
    sig!("AUGraphDisconnectNodeInput", I32, P, I32, U32),
    sig!("AUGraphInitialize", I32, P),
    // `AUGraphNodeInfo(AUGraph, AUNode, AudioComponentDescription *out,
    // AudioUnit *out)` — both outputs optional.
    sig!("AUGraphNodeInfo", I32, P, I32, P, P),
    sig!("AUGraphOpen", I32, P),
    sig!("AUGraphStart", I32, P),
    sig!("AUGraphStop", I32, P),
    // `AUGraphUpdate(AUGraph, Boolean *isUpdated)`, whose second argument is an
    // optional output and not a flag.
    sig!("AUGraphUpdate", I32, P, P),
    // `AudioUnitSetParameter(AudioUnit, AudioUnitParameterID,
    // AudioUnitParameterScope, AudioUnitElement, AudioUnitParameterValue,
    // UInt32 bufferOffsetInFrames)` — the value is a `Float32`, so it arrives in
    // `xmm0` while the four words before it are integers.
    sig!("AudioUnitSetParameter", I32, P, U32, U32, U32, F32, U32),
    // ## `AudioConverter`, which is how a stream is decoded
    //
    // `AudioConverterNew` takes two `AudioStreamBasicDescription *` and answers
    // an opaque converter. `FillComplexBuffer`'s second argument is the guest's
    // input callback — a function pointer the *host* calls, so it crosses as a
    // thunk like every other callback.
    sig!("AudioConverterNew", I32, P, P, P),
    sig!("AudioConverterDispose", I32, P),
    sig!("AudioConverterReset", I32, P),
    cbsig!(
        [1],
        "AudioConverterFillComplexBuffer",
        I32,
        P,
        P,
        P,
        P,
        P,
        P
    ),
    // `CCCrypt`: 11 integer-class arguments, within `MAX_INT_ARGS_TOTAL` — the
    // three past the eighth spill to the host stack, all doubleword-wide.
    sig!("CCCrypt", I32, U32, U32, U32, P, U64, P, P, U64, P, U64, P),
    sig!("CC_MD5", P, P, U32, P),
    sig!("CC_MD5_Final", I32, P, P),
    sig!("CC_MD5_Init", I32, P),
    sig!("CC_MD5_Update", I32, P, P, U32),
    sig!("CC_SHA1", P, P, U32, P),
    sig!("CC_SHA1_Final", I32, P, P),
    sig!("CC_SHA1_Init", I32, P),
    sig!("CC_SHA1_Update", I32, P, P, U32),
    sig!("CC_SHA512_Final", I32, P, P),
    sig!("CC_SHA512_Init", I32, P),
    sig!("CC_SHA512_Update", I32, P, P, U32),
    sig!("CFArrayAppendValue", V, P, P),
    // `CFRange` by value, which is the case the roadmap already settled for
    // `CFStringGetBytes`: two `CFIndex`, sixteen bytes and both integer, and
    // SysV AMD64 and AAPCS64 agree on two consecutive integer registers. The
    // members are written out, so every register lands where the callee reads
    // it. The applier is guest code the host calls back per element.
    cbsig!([3], "CFArrayApplyFunction", V, P, I64, I64, P, P),
    // ## CoreFoundation and CFNetwork
    //
    // `CFIndex` is `long`, `Boolean` is `unsigned char`, and a `CFStringEncoding`
    // is a `UInt32` — three narrow types that a "pointer or int64" guess would
    // get wrong in the direction that does not fault.
    sig!("CFArrayCreate", P, P, P, I64, P),
    sig!("CFArrayCreateMutable", P, P, I64, P),
    sig!("CFArrayExchangeValuesAtIndices", V, P, I64, I64),
    sig!("CFArrayGetCount", I64, P),
    sig!("CFArrayGetTypeID", U64),
    sig!("CFArrayGetValueAtIndex", P, P, I64),
    // The bundle these answer for is the *guest's*, because
    // [`crate::bundle`]'s re-exec made the running executable the guest's own —
    // so `CFBundleCopyExecutableURL(CFBundleGetMainBundle())` names a path
    // inside the mirror rather than `rosetta3`'s.
    sig!("CFBundleCopyExecutableURL", P, P),
    sig!("CFBundleCopyPrivateFrameworksURL", P, P),
    sig!("CFBundleCopyResourcesDirectoryURL", P, P),
    // `CFURLRef CFBundleCopyResourceURL(CFBundleRef, CFStringRef name,
    // CFStringRef type, CFStringRef subDirName)` — three `CFStringRef`s, any of
    // which may be null.
    sig!("CFBundleCopyResourceURL", P, P, P, P, P),
    sig!("CFBundleCreate", P, P, P),
    sig!("CFBundleGetIdentifier", P, P),
    sig!("CFBundleGetMainBundle", P),
    sig!("CFBundleGetVersionNumber", U32, P),
    sig!("CFBundleGetValueForInfoDictionaryKey", P, P, P),
    sig!("CFStringGetCharactersPtr", P, P),
    // ## The `CFTypeID` accessors
    //
    // `CFTypeID` is a `unsigned long`, and every one of these takes no argument
    // and answers a process-wide constant. They exist so a guest can branch on
    // `CFGetTypeID(x) == CFStringGetTypeID()` — which works here for the reason
    // the rest of the CF surface does, and would stop working the moment any of
    // them were answered by anything other than the host's own.
    sig!("CFBagGetTypeID", U64),
    sig!("CFBooleanGetTypeID", U64),
    sig!("CFDateGetTypeID", U64),
    sig!("CFNullGetTypeID", U64),
    sig!("CFSetGetTypeID", U64),
    sig!("CFURLGetTypeID", U64),
    sig!("CFUUIDGetTypeID", U64),
    sig!("CFBooleanGetValue", BOOL, P),
    // `CFDataRef CFDataCreate(CFAllocatorRef, const UInt8 *, CFIndex)` — the
    // bytes are read out of guest memory and copied into the host's object,
    // which is what makes the copy the right one: the guest may free its buffer
    // the instant this returns.
    sig!("CFDataCreate", P, P, P, I64),
    sig!("CFDataCreateCopy", P, P, P),
    sig!("CFDataGetBytePtr", P, P),
    sig!("CFDataGetLength", I64, P),
    sig!("CFDataGetTypeID", U64),
    sig!("CFDictionaryAddValue", V, P, P, P),
    sig!("CFDictionaryContainsKey", BOOL, P, P),
    sig!("CFDictionaryGetCount", I64, P),
    sig!("CFDictionaryCreate", P, P, P, P, I64, P, P),
    sig!("CFDictionaryCreateMutable", P, P, I64, P, P),
    sig!("CFDictionaryGetTypeID", U64),
    sig!("CFDictionaryGetValue", P, P, P),
    sig!("CFDictionaryGetValueIfPresent", BOOL, P, P, P),
    sig!("CFDictionarySetValue", V, P, P, P),
    sig!("CFEqual", BOOL, P, P),
    sig!("CFGetTypeID", U64, P),
    sig!("CFHTTPAuthenticationCopyMethod", P, P),
    sig!("CFHTTPAuthenticationCopyRealm", P, P),
    sig!("CFHTTPAuthenticationCreateFromResponse", P, P, P),
    sig!("CFHTTPAuthenticationIsValid", U8, P, P),
    sig!("CFHTTPAuthenticationRequiresAccountDomain", U8, P),
    sig!("CFHTTPAuthenticationRequiresUserNameAndPassword", U8, P),
    sig!("CFHTTPMessageApplyCredentialDictionary", U8, P, P, P, P),
    sig!("CFHTTPMessageCopyAllHeaderFields", P, P),
    sig!("CFHTTPMessageCopyResponseStatusLine", P, P),
    sig!("CFHTTPMessageCopyVersion", P, P),
    sig!("CFHTTPMessageCreateRequest", P, P, P, P, P),
    sig!("CFHTTPMessageGetResponseStatusCode", I64, P),
    sig!("CFHTTPMessageIsHeaderComplete", U8, P),
    sig!("CFHTTPMessageSetHeaderFieldValue", V, P, P, P),
    sig!("CFHash", U64, P),
    sig!("CFLocaleCopyCurrent", P),
    sig!("CFLocaleGetIdentifier", P, P),
    sig!("CFLocaleGetValue", P, P, P),
    sig!("CFNetworkCopySystemProxySettings", P),
    // ### The notification centres
    //
    // `CFNotificationCallback(CFNotificationCenterRef, void *observer,
    // CFNotificationName, const void *object, CFDictionaryRef userInfo)` is
    // argument two, and the host calls it on whatever thread posts — so it goes
    // out as a thunk like every other guest function pointer crossing the seam.
    // `CFNotificationSuspensionBehavior` is a `CF_ENUM(CFIndex, ...)`, hence a
    // doubleword rather than the `uint32_t` it looks like.
    //
    // The three centres differ only in who can post to them: the distributed
    // one carries notifications between processes, which is how a game hears
    // that the screen locked or the user switched away.
    cbsig!(
        [2],
        "CFNotificationCenterAddObserver",
        V,
        P,
        P,
        P,
        P,
        P,
        I64
    ),
    sig!("CFNotificationCenterGetDarwinNotifyCenter", P),
    sig!("CFNotificationCenterGetDistributedCenter", P),
    sig!("CFNotificationCenterGetLocalCenter", P),
    sig!("CFNotificationCenterPostNotification", V, P, P, P, P, BOOL),
    sig!("CFNotificationCenterRemoveEveryObserver", V, P, P),
    sig!("CFNotificationCenterRemoveObserver", V, P, P, P, P),
    sig!("CFNetworkCopyProxiesForAutoConfigurationScript", P, P, P, P),
    sig!("CFNetworkCopyProxiesForURL", P, P, P),
    sig!("CFNumberCreate", P, P, I64, P),
    sig!("CFNumberGetTypeID", U64),
    // `Boolean` is an `unsigned char`, not an `int`: the caller tests the low
    // byte and a wider return would leave the rest of `rax` to be read as part
    // of it.
    sig!("CFNumberGetValue", U8, P, I64, P),
    // ## Preferences
    //
    // The three `UnityPlayer.dylib` imports, and no more: a guest reads its own
    // defaults through these on start-up — Unity asks for the resolution, the
    // fullscreen mode and the graphics API it was last told to use.
    //
    // `CFPreferencesCopyAppValue` returns a `CFPropertyListRef` the caller owns,
    // which needs nothing from this runtime: it is a host allocation on both
    // sides and `CFRelease` above already crosses.
    //
    // The two `GetApp*Value` forms take a `Boolean *keyExistsAndHasValidFormat`
    // out-parameter, which the guest may pass as null and which is written
    // through directly — a guest pointer is a real address here, so the host's
    // own store lands where the guest reads it.
    sig!("CFPreferencesCopyAppValue", P, P, P),
    sig!("CFPreferencesGetAppBooleanValue", U8, P, P, P),
    sig!("CFPreferencesGetAppIntegerValue", I64, P, P, P),
    sig!("CFReadStreamCopyError", P, P),
    sig!("CFReadStreamCopyProperty", P, P, P),
    sig!("CFReadStreamCreateForHTTPRequest", P, P, P),
    sig!("CFReadStreamCreateForStreamedHTTPRequest", P, P, P, P),
    sig!("CFReadStreamHasBytesAvailable", U8, P),
    sig!("CFReadStreamOpen", U8, P),
    // The client callback is guest code the host calls back on the run loop;
    // the fourth argument is a `CFStreamClientContext`, whose `retain`,
    // `release` and `copyDescription` fields are function pointers a guest
    // conventionally leaves null. A guest that fills them in would hand the host
    // x86-64 addresses inside a structure this runtime never sees.
    cbsig!([2], "CFReadStreamSetClient", U8, P, U64, P, P),
    sig!("CFReadStreamSetProperty", U8, P, P, P),
    sig!("CFRelease", V, P),
    sig!("CFRetain", P, P),
    sig!("CFLocaleGetSystem", P),
    sig!("CFRunLoopAddObserver", V, P, P, P),
    sig!("CFRunLoopRemoveObserver", V, P, P, P),
    sig!("CFRunLoopObserverInvalidate", V, P),
    sig!("CFRunLoopAddSource", V, P, P, P),
    sig!("CFRunLoopGetCurrent", P),
    sig!("CFRunLoopGetMain", P),
    // `void CFRunLoopRun(void)` — no parameters. This declared one, which is
    // harmless in the direction it is wrong (the callee reads only what its own
    // prototype declares, so an extra argument is ignored) and is still a lie
    // the table should not tell. Found by `the_table_declares_the_parameters_
    // these_functions_really_have`.
    sig!("CFRunLoopRun", V),
    // `CFTimeInterval` is a `double`, so this is the first entry in the table
    // whose second argument is floating-point while its first and third are not
    // — exactly the shape the two independent register counters exist for.
    sig!("CFRunLoopRunInMode", I32, P, F64, U8),
    sig!("CFRunLoopSourceSignal", V, P),
    sig!("CFRunLoopStop", V, P),
    sig!("CFRunLoopWakeUp", V, P),
    // `CFSetApplierFunction(const void *value, void *context)`, called once
    // per member before the call returns — so unlike most callbacks here it is
    // reached on the guest's own thread, and unlike most it must still go out
    // as a thunk, because CoreFoundation is what calls it.
    cbsig!([1], "CFSetApplyFunction", V, P, P, P),
    sig!("CFSetGetCount", I64, P),
    sig!("CFSetGetValues", V, P, P),
    sig!("CFStringAppendCharacters", V, P, P, I64),
    // `CFComparisonResult` is a `CFIndex`, so a signed doubleword, and
    // `CFStringCompareFlags` is a `CFOptionFlags` — both 64-bit on LP64. This
    // is where SDL3 stopped: it compares a display mode's pixel encoding
    // against `kIO32BitDirectPixels` and friends.
    sig!("CFStringCompare", I64, P, P, U64),
    sig!("CFStringConvertEncodingToIANACharSetName", P, U32),
    sig!("CFStringConvertEncodingToNSStringEncoding", U64, U32),
    sig!("CFStringConvertIANACharSetNameToEncoding", U32, P),
    sig!("CFStringConvertNSStringEncodingToEncoding", U32, U64),
    sig!("CFStringCreateWithCharacters", P, P, P, I64),
    sig!("CFStringCreateWithCString", P, P, P, U32),
    // `CFStringRef CFStringCreateWithBytes(CFAllocatorRef, const UInt8 *bytes,
    // CFIndex numBytes, CFStringEncoding, Boolean isExternalRepresentation)`.
    // The form a program uses when its bytes are counted rather than terminated,
    // which for Unreal Engine 3 is every one: a `TCHAR` string is UTF-32 and has
    // no C terminator to find.
    sig!("CFStringCreateWithBytes", P, P, P, I64, U32, BOOL),
    // ## `CFRange` is the aggregate that needs no vocabulary
    //
    // Refused as an aggregate until M7.4, and it is worth being precise about
    // why it stopped being one. `CFStringGetBytes` takes a `CFRange` **by
    // value**, and [`AGGREGATE`] is right that this table cannot say "an
    // argument is a struct". It does not have to. A `CFRange` is two `CFIndex`
    // — sixteen bytes, both integer — and SysV AMD64 and AAPCS64 *agree* on how
    // to pass one: two consecutive integer registers. So the two members are
    // written down as two arguments, and every register lands where the callee
    // reads it.
    //
    // This is the same fact as `NSRange` in the Objective-C path, and it is
    // exactly as narrow: `NSRect` does not agree, because SysV classifies
    // anything over sixteen bytes as MEMORY while AAPCS64 sees a homogeneous
    // float aggregate. The rule that survives is "check this struct against both
    // ABIs", not "structs are fine now" — everything still in the aggregate
    // refusal list is there because it was checked and does not agree.
    //
    // Nine integer arguments, one past the host's eight; see `glTexImage2D` for
    // the precedent and [`super::marshal`] for what carries the ninth. Reached
    // by Mono, which converts every managed string through it.
    sig!("CFStringGetBytes", I64, P, I64, I64, U32, U8, U8, P, I64, P),
    // A `UniChar` is a `UInt16`. Apple's arm64 ABI requires the callee to
    // extend a narrow return to 32 bits, so reading `w0` is safe where the
    // guest reads only `ax`.
    sig!("CFStringGetCharacterAtIndex", U16, P, I64),
    // `Boolean CFStringGetCString(CFStringRef, char *buffer, CFIndex
    // bufferSize, CFStringEncoding encoding)` — **four** parameters. This entry
    // had three, and the missing one was the last, so the host read whatever
    // the guest happened to leave in `x3` as the encoding and answered `false`
    // for an encoding it did not recognise.
    //
    // Nothing failed where it was called. The conversion simply produced an
    // empty string, and Unity built a plugin path out of it: an executable name
    // appended to `Contents/MacOS/` that was not there, so every `.bundle`
    // plugin in the game — FMOD, Steam, the game's own — was `dlopen`ed as a
    // directory and reported `DllNotFoundException`. See the roadmap's M7.13.
    //
    // `bufferSize` is a `CFIndex`, which is signed; the old entry called it
    // `U64`, which passes the same bits and says the wrong thing.
    sig!("CFStringCreateMutableCopy", P, P, I64, P),
    sig!("CFStringUppercase", V, P, P),
    sig!("CFTimeZoneCopyDefault", P),
    sig!("CFTimeZoneGetName", P, P),
    sig!("CFStringGetCString", BOOL, P, P, I64, U32),
    sig!("CFStringGetCStringPtr", P, P, U32),
    sig!("CFStringGetLength", I64, P),
    sig!("CFStringGetMaximumSizeForEncoding", I64, I64, U32),
    sig!("CFStringGetSystemEncoding", U32),
    sig!("CFStringGetTypeID", U64),
    sig!("CFStringHasPrefix", BOOL, P, P),
    sig!(
        "CFURLCreateStringByAddingPercentEscapes",
        P,
        P,
        P,
        P,
        P,
        U32
    ),
    // `CFURLPathStyle` is a `CF_ENUM(CFIndex, ...)`, so the style is a
    // doubleword and not the `uint32_t` the neighbouring encoding arguments
    // are — the two look interchangeable in a header and are not.
    sig!("CFURLCopyFileSystemPath", P, P, I64),
    sig!("CFURLCreateWithBytes", P, P, P, I64, U32, P),
    // A `CFURLPathStyle` is a `CFIndex`, not an `int`, and `isDirectory` is a
    // `Boolean` — the two narrow types this family keeps getting wrong in the
    // direction that does not fault.
    sig!("CFURLCreateWithFileSystemPath", P, P, P, I64, BOOL),
    sig!("CFURLGetFileSystemRepresentation", U8, P, U8, P, I64),
    sig!("CFUUIDCreate", P, P),
    sig!("CFUUIDCreateString", P, P, P),
    // ## CoreGraphics displays
    //
    // Everything SDL3's Cocoa backend asks about a screen: which displays exist,
    // what modes each has, and which one is in use. Passthrough is right for the
    // whole family because the displays being asked about are *this machine's* —
    // there is no guest-side notion of a monitor for these to disagree with.
    //
    // Every type here is a scalar or a pointer, and each was read out of
    // `<CoreGraphics/CGDirectDisplay.h>` rather than recalled: a
    // `CGDirectDisplayID` is a `uint32_t`, a `CGError` and a `boolean_t` are
    // `int32_t`, a `CGDisplayModeRef` and every `CF*Ref` is a pointer, and the
    // sizes are `size_t`. `CGDisplayModeGetRefreshRate` returns a `double`,
    // which is the only one that lands in a vector register.
    //
    // `CGDisplayBounds` is *not* here: it returns a `CGRect` by value, thirty-two
    // bytes, which SysV AMD64 calls MEMORY and AAPCS64 returns in `v0`-`v3`. It
    // is [`crate::shims::coregraphics`], for the reason `__sincos_stret` is a
    // shim.
    sig!("CGAcquireDisplayFadeReservation", I32, F32, P),
    sig!("CGDirectDisplayCopyCurrentMetalDevice", P, I32),
    sig!("MTLCreateSystemDefaultDevice", P),
    sig!("CGAssociateMouseAndMouseCursorPosition", I32, I32),
    // `boolean_t CGCursorIsVisible(void)` — an `int`, not a `CGError`, and the
    // pair to `CGDisplayHideCursor`: a game that hides the cursor for mouse-look
    // asks this before deciding whether it has to.
    sig!("CGCursorIsVisible", I32),
    // ## The bitmap context, which is how an image becomes texture bytes
    //
    // cocos2d-x decodes every texture the same way: a `CGImageSource` over the
    // file's bytes produces a `CGImage`, a bitmap context is created over a
    // buffer the *guest* allocated, and the image is drawn into it. Everything
    // here is a `size_t`, a handle or a `CGBitmapInfo` (a `uint32_t`) except
    // the two that take a `CGRect` by value, which are
    // [`crate::shims::coregraphics`].
    sig!("CGBitmapContextCreate", P, P, U64, U64, U64, U64, P, U32),
    // ## Capturing a display, which is how a game takes exclusive fullscreen
    //
    // The whole-desktop forms take nothing and the per-display ones a
    // `CGDirectDisplayID`; every one returns a `CGError`.
    sig!("CGCaptureAllDisplays", I32),
    sig!("CGReleaseAllDisplays", I32),
    sig!("CGDisplayCapture", I32, U32),
    sig!("CGDisplayRelease", I32, U32),
    sig!("CGColorSpaceCreateDeviceRGB", P),
    sig!("CGColorSpaceRelease", V, P),
    // `CGFloat` is `double` on both architectures, so the CTM translation is
    // two ordinary vector arguments.
    sig!("CGContextRelease", V, P),
    sig!("CGContextTranslateCTM", V, P, F64, F64),
    sig!("CGDisplayCopyAllDisplayModes", P, U32, P),
    sig!("CGDisplayCopyDisplayMode", P, U32),
    // Seven floats and a flag, and the `boolean_t` at the end is the argument
    // that makes the count worth checking: AAPCS64 allocates integer and
    // floating-point arguments from *independent* counters, so `synchronous`
    // is `x0` and not a ninth anything.
    sig!("CGDisplayFade", I32, U32, F32, F32, F32, F32, F32, F32, I32),
    // Both take a `CGDirectDisplayID`, and both are reference-counted rather
    // than absolute: a game hiding the cursor for a level must show it again.
    sig!("CGDisplayHideCursor", I32, U32),
    sig!("CGDisplayIDToOpenGLDisplayMask", U32, U32),
    sig!("CGDisplayIOServicePort", U32, U32),
    sig!("CGDisplayIsMain", I32, U32),
    sig!("CGDisplayMirrorsDisplay", U32, U32),
    sig!("CGDisplayModeCopyPixelEncoding", P, P),
    sig!("CGDisplayModeGetHeight", U64, P),
    sig!("CGDisplayModeGetIOFlags", U32, P),
    sig!("CGDisplayModeGetPixelHeight", U64, P),
    sig!("CGDisplayModeGetPixelWidth", U64, P),
    sig!("CGDisplayModeGetRefreshRate", F64, P),
    sig!("CGDisplayModeGetWidth", U64, P),
    sig!("CGDisplayModeIsUsableForDesktopGUI", BOOL, P),
    sig!("CGDisplayModeRelease", V, P),
    sig!("CGDisplayModeRetain", P, P),
    // `CGPoint` by value, which is the [`CFRange`] case rather than the
    // `NSRect` one: two `double`s, sixteen bytes, and SysV AMD64 and AAPCS64
    // **agree** — two SSE eightbytes on one side, a two-member homogeneous
    // float aggregate on the other, both `xmm0`/`xmm1` and `v0`/`v1`. Written
    // out as its members, so every register lands where the callee reads it.
    sig!("CGDisplayMoveCursorToPoint", I32, U32, F64, F64),
    sig!("CGDisplayPixelsHigh", U64, U32),
    sig!("CGDisplayPixelsWide", U64, U32),
    // The host calls these back on its own thread when a display is
    // reconfigured, so the first argument leaves as a thunk like every other
    // guest function pointer crossing the seam.
    cbsig!([0], "CGDisplayRegisterReconfigurationCallback", I32, P, P),
    cbsig!([0], "CGDisplayRemoveReconfigurationCallback", I32, P, P),
    sig!("CGDisplaySetDisplayMode", I32, U32, P, P),
    sig!("CGDisplayShowCursor", I32, U32),
    // `CGEventCreate(CGEventSourceRef)` — a null source means the event
    // describes the current input state, which is how a game asks where the
    // pointer is without a window under it. `CGEventGetLocation` returns a
    // `CGPoint` and is [`crate::shims::coregraphics`]; a `CGEventType` is a
    // `uint32_t`.
    sig!("CGEventCreate", P, P),
    sig!("CGEventPost", V, U32, P),
    sig!("CGEventSetType", V, P, U32),
    // The gamma ramp, read and written. The tables are `float *` — arrays the
    // caller owns — and the capacity and sample counts are `uint32_t`, so both
    // are pointers and 32-bit integers even though what travels through them is
    // floating point.
    sig!("CGGetDisplayTransferByTable", I32, U32, U32, P, P, P, P),
    sig!("CGSetDisplayTransferByTable", I32, U32, U32, P, P, P),
    sig!("CGGetOnlineDisplayList", I32, U32, P, P),
    // `void CGGetLastMouseDelta(int32_t *dx, int32_t *dy)` — how a game reads
    // relative mouse motion when the cursor is hidden and locked, which is
    // every first-person camera. Both out-parameters are guest memory the host
    // writes in place.
    sig!("CGGetLastMouseDelta", V, P, P),
    // `CGEventFlags` is a `uint64_t` of modifier bits; the source state id is
    // an enum.
    sig!("CGEventSourceFlagsState", U64, U32),
    sig!("CGDisplayModelNumber", U32, U32),
    sig!("CGDisplayVendorNumber", U32, U32),
    sig!("CGDisplayRotation", F64, U32),
    sig!("CGColorSpaceCreateWithName", P, P),
    sig!("CGGetActiveDisplayList", I32, U32, P, P),
    // ## CGL, which is what an OpenGL context is underneath
    //
    // `NSOpenGLContext` is a thin wrapper, and a Unity player written before
    // Metal talks to the layer below it directly. Every one of these is
    // scalars and opaque handles: a `CGLError` is an `int`, a
    // `CGLContextObj`/`CGLPixelFormatObj`/`CGLRendererInfoObj` is an opaque
    // pointer, and the enumerations (`CGLContextEnable`, `CGLContextParameter`,
    // `CGLRendererProperty`, `CGLPixelFormatAttribute`) are all `uint32_t`.
    // Checked against the framework's own headers rather than recalled, because
    // the family's return types are not uniform — the `Retain` calls hand the
    // object back, the `Release` calls return `void`, and everything else
    // returns an error code.
    sig!("CGLChoosePixelFormat", I32, P, P, P),
    sig!("CGLCreateContext", I32, P, P, P),
    sig!("CGLDescribePixelFormat", I32, P, I32, U32, P),
    sig!("CGLDescribeRenderer", I32, P, I32, U32, P),
    sig!("CGLDestroyContext", I32, P),
    sig!("CGLDestroyPixelFormat", I32, P),
    sig!("CGLDestroyRendererInfo", I32, P),
    sig!("CGLDisable", I32, P, U32),
    sig!("CGLEnable", I32, P, U32),
    sig!("CGLErrorString", P, I32),
    sig!("CGLFlushDrawable", I32, P),
    sig!("CGLGetCurrentContext", P),
    sig!("CGLGetParameter", I32, P, U32, P),
    sig!("CGLGetPixelFormat", P, P),
    sig!("CGLGetVirtualScreen", I32, P, P),
    sig!("CGLLockContext", I32, P),
    sig!("CGLQueryRendererInfo", I32, U32, P, P),
    sig!("CGLReleaseContext", V, P),
    sig!("CGLReleasePixelFormat", V, P),
    sig!("CGLRetainContext", P, P),
    sig!("CGLRetainPixelFormat", P, P),
    sig!("CGLSetCurrentContext", I32, P),
    sig!("CGLSetParameter", I32, P, U32, P),
    sig!("CGLSetVirtualScreen", I32, P, I32),
    sig!("CGLUnlockContext", I32, P),
    sig!("CGLUpdateContext", I32, P),
    // ## Images, and the source that decodes one
    //
    // `CGImageGetWidth`/`Height`/`BitsPerComponent` are `size_t`; a
    // `CGImageAlphaInfo` is a `uint32_t` enum; the index into a source is a
    // `size_t`. The `options` dictionaries are ordinary `CFDictionaryRef`s the
    // guest usually passes null for.
    sig!("CGImageGetAlphaInfo", U32, P),
    sig!("CGImageGetBitsPerComponent", U64, P),
    sig!("CGImageGetColorSpace", P, P),
    sig!("CGImageGetHeight", U64, P),
    sig!("CGImageGetWidth", U64, P),
    sig!("CGImageSourceCreateImageAtIndex", P, P, U64, P),
    sig!("CGImageSourceCreateWithData", P, P, P),
    sig!("CGMainDisplayID", U32),
    sig!("CGReleaseDisplayFadeReservation", I32, U32),
    // A `CGWindowLevel` and a `CGWindowLevelKey` are both `int32_t`. The
    // shielding level is what a fullscreen window is raised to so it sits above
    // everything else the window server draws.
    sig!("CGShieldingWindowLevel", I32),
    // `CGPoint` by value again; see `CGDisplayMoveCursorToPoint`.
    sig!("CGWarpMouseCursorPosition", I32, F64, F64),
    sig!("CGWindowLevelForKey", I32, I32),
    sig!("CGColorGetConstantColor", P, P),
    sig!("CGColorCreateCopy", P, P),
    sig!("CGColorRelease", V, P),
    sig!("CGColorRetain", P, P),
    // ## CoreVideo
    //
    // The display-link callback is how a GameMaker runner paces its frames: the
    // host calls it on CoreVideo's own thread, which is a thread this runtime
    // never created, so it reaches the guest exactly the way a `pthread_create`
    // start routine does — through a thunk, on a guest stack made on demand.
    // `CFTimeInterval CACurrentMediaTime(void)` — `mach_absolute_time` in
    // seconds, as a `double`. Roblox paces its renderer off it.
    sig!("CACurrentMediaTime", F64),
    // ## ForceFeedback
    //
    // The rumble half of the HID surface, which a game reaches once it has found
    // a gamepad through IOKit. Everything is a scalar or a pointer: `HRESULT` is
    // an `int`, an `FFDeviceObjectReference` and an `FFEffectObjectReference`
    // are opaque handles, an `io_service_t` is a Mach port name, and the
    // parameter and capability structures are written through pointers the guest
    // owns.
    //
    // `FFDeviceCreateEffect` takes a `GUID *` and an `FFEFFECT *`, both by
    // address, and the second holds a pointer to a type-specific parameter block
    // — all data, none of it function pointers, so none of it is the problem
    // `CFRunLoopSourceCreate` is. What it *would* be a problem for is a guest
    // that installs an effect envelope callback, which the API has no way to
    // express.
    sig!("FFIsForceFeedback", I32, U32),
    sig!("FFCreateDevice", I32, U32, P),
    sig!("FFReleaseDevice", I32, P),
    sig!("FFDeviceCreateEffect", I32, P, P, P, P),
    sig!("FFDeviceReleaseEffect", I32, P, P),
    sig!("FFDeviceGetForceFeedbackCapabilities", I32, P, P),
    sig!("FFDeviceGetForceFeedbackProperty", I32, P, U32, P, U32),
    sig!("FFDeviceSetForceFeedbackProperty", I32, P, U32, P),
    sig!("FFDeviceSendForceFeedbackCommand", I32, P, U32),
    sig!("FFEffectGetEffectStatus", I32, P, P),
    sig!("FFEffectSetParameters", I32, P, P, U32),
    sig!("FFEffectStart", I32, P, U32, U32),
    sig!("FFEffectStop", I32, P),
    sig!("CVDisplayLinkCreateWithActiveCGDisplays", I32, P),
    sig!("CVDisplayLinkCreateWithCGDisplay", I32, U32, P),
    sig!("CVDisplayLinkGetActualOutputVideoRefreshPeriod", F64, P),
    // `CVTime` by value: sixteen bytes, both eightbytes integer, and the two
    // ABIs *agree* — `rax`/`rdx` on one side, `x0`/`x1` on the other. It is
    // still not a table entry, because [`super::marshal`]'s return path carries
    // one register. The types here are a placeholder;
    // [`crate::shims::coregraphics`] claims the name and never reads them.
    sig!("CVDisplayLinkGetNominalOutputVideoRefreshPeriod", V, P),
    sig!("CVDisplayLinkRelease", V, P),
    sig!("CVDisplayLinkRetain", P, P),
    sig!("CVDisplayLinkSetCurrentCGDisplay", I32, P, U32),
    sig!(
        "CVDisplayLinkSetCurrentCGDisplayFromOpenGLContext",
        I32,
        P,
        P,
        P
    ),
    cbsig!([1], "CVDisplayLinkSetOutputCallback", I32, P, P, P),
    sig!("CVDisplayLinkStart", I32, P),
    sig!("CVDisplayLinkStop", I32, P),
    // ### The pixel buffer, and the OpenGL texture cache over it
    //
    // How a Unity player gets a decoded video frame onto the GPU: lock the
    // buffer, hand it to the cache, get a texture name back. A `CVReturn` is an
    // `int32_t`, a `CVOptionFlags`/`CVPixelBufferLockFlags` is a `uint64_t`,
    // and a `GLuint`/`GLenum` is a `uint32_t` — so the family is entirely
    // handles and scalars despite what it does.
    sig!("CVBufferRelease", V, P),
    sig!("CVBufferRetain", P, P),
    sig!("CVOpenGLTextureCacheCreate", I32, P, P, P, P, P, P),
    sig!(
        "CVOpenGLTextureCacheCreateTextureFromImage",
        I32,
        P,
        P,
        P,
        P,
        P
    ),
    sig!("CVOpenGLTextureCacheFlush", V, P, U64),
    sig!("CVOpenGLTextureCacheRelease", V, P),
    sig!("CVOpenGLTextureGetName", U32, P),
    sig!("CVOpenGLTextureGetTarget", U32, P),
    sig!("CVPixelBufferGetBaseAddress", P, P),
    sig!("CVPixelBufferGetBytesPerRow", U64, P),
    sig!("CVPixelBufferGetHeight", U64, P),
    sig!("CVPixelBufferGetWidth", U64, P),
    sig!("CVPixelBufferLockBaseAddress", I32, P, U64),
    sig!("CVPixelBufferUnlockBaseAddress", I32, P, U64),
    // ## The Carbon Component Manager, and `Gestalt`
    //
    // Removed from the SDK's headers years ago and still exported by
    // CoreServices, which is why these signatures are written from the last
    // published declarations rather than read out of a header. `OSErr` is
    // `SInt16`; Apple's arm64 ABI requires a callee to extend a narrow return
    // value to 32 bits, so reading `w0` as an `int` is safe where the guest
    // reads only `ax`.
    sig!("CloseComponent", I32, P),
    sig!("FindNextComponent", P, P, P),
    sig!("Gestalt", I32, U32, P),
    // ## IOKit
    //
    // Every handle here — `mach_port_t`, `io_object_t`, `io_iterator_t`,
    // `io_registry_entry_t` — is a 32-bit port name, not a pointer. Declaring
    // one as `P` would pass 64 bits where the host reads 32, which happens to
    // work until the upper half is not zero.
    sig!("IOBSDNameMatching", P, U32, U32, P),
    // `CFDictionaryRef IODisplayCreateInfoDictionary(io_service_t, IOOptionBits)`
    // — how SDL asks a framebuffer service for its display's localized name.
    // Both arguments are 32-bit, per the note above; the result is an ordinary
    // CoreFoundation object the caller owns.
    sig!("IODisplayCreateInfoDictionary", P, U32, U32),
    sig!("IOHIDDeviceCopyMatchingElements", P, P, P, U32),
    // `IOReturn IOHIDDeviceOpen(IOHIDDeviceRef, IOOptionBits)`, and its close.
    sig!("IOHIDDeviceOpen", I32, P, U32),
    sig!("IOHIDDeviceClose", I32, P, U32),
    sig!("IOHIDDeviceGetProperty", P, P, P),
    // `IOHIDDeviceGetService` hands back the `io_service_t` behind a device,
    // which is the 32-bit port name the registry calls are asked in terms of —
    // the one place in this family where a handle *becomes* a port name.
    sig!("IOHIDDeviceGetService", U32, P),
    // `IOReturn IOHIDDeviceGetValue(IOHIDDeviceRef, IOHIDElementRef,
    // IOHIDValueRef *)` — the polled counterpart to the value callback below.
    sig!("IOHIDDeviceGetValue", I32, P, P, P),
    // The host calls the callback with `(context, IOReturn, sender, value)`:
    // four integer arguments and no result, which is the shape the generic
    // trampoline was written for.
    cbsig!([1], "IOHIDDeviceRegisterInputValueCallback", V, P, P, P),
    // Removal is `IOHIDCallback(void *context, IOReturn, void *sender)`: the
    // guest's function pointer leaves as a thunk like every other.
    cbsig!([1], "IOHIDDeviceRegisterRemovalCallback", V, P, P, P),
    // Both take the run loop and a mode string, and both are `void`.
    sig!("IOHIDDeviceScheduleWithRunLoop", V, P, P, P),
    sig!("IOHIDDeviceUnscheduleFromRunLoop", V, P, P, P),
    // `IOHIDReportCallback(void *context, IOReturn, void *sender,
    // IOHIDReportType, uint32_t reportID, uint8_t *report, CFIndex length)`.
    // The callback is argument three, and the `report` buffer belongs to the
    // *guest* — it hands the address in and the host fills it in place, which
    // the identity mapping makes free.
    cbsig!(
        [3],
        "IOHIDDeviceRegisterInputReportCallback",
        V,
        P,
        P,
        I64,
        P,
        P
    ),
    sig!("IOHIDDeviceSetProperty", BOOL, P, P, P),
    sig!("IOHIDDeviceSetReport", I32, P, U32, I64, P, I64),
    sig!("IOHIDElementGetChildren", P, P),
    sig!("IOHIDElementGetDevice", P, P),
    sig!("IOHIDElementGetCookie", U32, P),
    // `CFIndex` is a `long`, so these are sixty-four bits wide and signed —
    // a logical minimum is routinely negative.
    // The rest of the element's description, all of it read straight off the
    // `IOHIDElement` and none of it a handle: the min/max pairs are `CFIndex`,
    // the unit and its exponent `uint32_t`, and the predicates `Boolean`.
    sig!("IOHIDElementGetCollectionType", U32, P),
    sig!("IOHIDElementGetName", P, P),
    sig!("IOHIDElementGetParent", P, P),
    sig!("IOHIDElementGetPhysicalMax", I64, P),
    sig!("IOHIDElementGetPhysicalMin", I64, P),
    sig!("IOHIDElementGetUnit", U32, P),
    sig!("IOHIDElementGetUnitExponent", U32, P),
    sig!("IOHIDElementHasNullState", BOOL, P),
    sig!("IOHIDElementHasPreferredState", BOOL, P),
    sig!("IOHIDElementIsArray", BOOL, P),
    sig!("IOHIDElementIsNonLinear", BOOL, P),
    sig!("IOHIDElementIsRelative", BOOL, P),
    sig!("IOHIDElementIsVirtual", BOOL, P),
    sig!("IOHIDElementIsWrapping", BOOL, P),
    sig!("IOHIDElementGetLogicalMax", I64, P),
    sig!("IOHIDElementGetLogicalMin", I64, P),
    sig!("IOHIDElementGetType", U32, P),
    // A `CFTypeID` is an `unsigned long`, so sixty-four bits, and this takes
    // nothing: it is how a caller checks that an unknown `CFTypeRef` is an
    // element before treating it as one.
    sig!("IOHIDElementGetTypeID", U64),
    sig!("IOHIDElementGetUsage", U32, P),
    sig!("IOHIDElementGetUsagePage", U32, P),
    sig!("IOHIDManagerClose", I32, P, U32),
    // `IOHIDManagerRef IOHIDManagerCreate(CFAllocatorRef, IOOptionBits)` —
    // a null allocator is the default one, which is what a game passes.
    sig!("IOHIDManagerCopyDevices", P, P),
    sig!("IOHIDManagerCreate", P, P, U32),
    sig!("IOHIDManagerOpen", I32, P, U32),
    cbsig!(
        [1],
        "IOHIDManagerRegisterDeviceMatchingCallback",
        V,
        P,
        P,
        P
    ),
    cbsig!([1], "IOHIDManagerRegisterDeviceRemovalCallback", V, P, P, P),
    // `IOHIDValueCallback(void *context, IOReturn, void *sender,
    // IOHIDValueRef)` — four integer arguments and no result, delivered on
    // whichever run loop the manager was scheduled on.
    cbsig!([1], "IOHIDManagerRegisterInputValueCallback", V, P, P, P),
    sig!("IOHIDManagerSetDeviceMatching", V, P, P),
    sig!("IONotificationPortCreate", P, I32),
    sig!("IONotificationPortGetMachPort", I32, P),
    sig!("IOHIDManagerScheduleWithRunLoop", V, P, P, P),
    sig!("IOHIDManagerSetDeviceMatchingMultiple", V, P, P),
    sig!("IOHIDManagerUnscheduleFromRunLoop", V, P, P, P),
    // The queue is the pull half of HID: a device's elements are added to it
    // and read back one value at a time. `IOHIDQueueCopyNextValueWithTimeout`
    // takes a `CFTimeInterval` — a `double`, and the only floating-point
    // argument in the family, allocated from AAPCS64's vector counter so the
    // queue handle is still `x0`.
    sig!("IOHIDQueueAddElement", V, P, P),
    sig!("IOHIDQueueContainsElement", BOOL, P, P),
    sig!("IOHIDQueueCopyNextValueWithTimeout", P, P, F64),
    sig!("IOHIDQueueCreate", P, P, P, I64, U32),
    sig!("IOHIDQueueStart", V, P),
    sig!("IOHIDQueueStop", V, P),
    sig!("IOHIDValueGetElement", P, P),
    sig!("IOHIDValueGetIntegerValue", I64, P),
    sig!("IOHIDValueGetLength", I64, P),
    sig!("IOIteratorIsValid", I32, U32),
    sig!("IOIteratorNext", U32, U32),
    sig!("IOMasterPort", I32, U32, P),
    sig!("IOObjectIsEqualTo", I32, U32, U32),
    sig!("IOObjectRelease", I32, U32),
    sig!("IOObjectRetain", I32, U32),
    // ### Power, which is two families that look like one
    //
    // An `IOPMAssertionID` is a `uint32_t`; the timeout in the middle of the
    // creation call is a `CFTimeInterval`, a `double`, and it is the only
    // floating-point argument in IOKit's whole surface here. AAPCS64 allocates
    // it from the vector counter, so the two `CFStringRef`s after it are still
    // integer arguments five and six rather than six and seven.
    sig!(
        "IOPMAssertionCreateWithDescription",
        I32,
        P,
        P,
        P,
        P,
        P,
        F64,
        P,
        P
    ),
    sig!("IOPMAssertionRelease", I32, U32),
    // The sleep/wake notification family is the *other* power API, and it is
    // the one an application registers with in order to be told before the
    // machine sleeps. `IORegisterForSystemPower` takes an
    // `IOServiceInterestCallback(void *refcon, io_service_t, natural_t
    // messageType, void *messageArgument)` — a guest function pointer in an
    // argument, so `cbsig!` marks it and the existing thunk seam carries it.
    // The two calls after it are the reply: a client that is told about a sleep
    // must answer, and `IOAllowPowerChange`/`IOCancelPowerChange` are how.
    //
    // Subnautica registers before it creates a window, which is why this is
    // where the player stopped: the refusal left the guest's `io_connect_t`
    // unset, and Unity treats that as fatal to its start-up rather than as a
    // feature it can do without.
    cbsig!([2], "IORegisterForSystemPower", U32, P, P, P, P),
    sig!("IODeregisterForSystemPower", I32, P),
    sig!("IOAllowPowerChange", I32, U32, I64),
    sig!("IOCancelPowerChange", I32, U32, I64),
    // `IONotificationPortRef` is opaque, and the run-loop source it vends is
    // what puts the notification on the application's own run loop.
    sig!("IONotificationPortGetRunLoopSource", P, P),
    sig!("IONotificationPortDestroy", V, P),
    // The power-source family is pure CoreFoundation: an opaque blob, a list
    // over it, and a dictionary per source. Every one is a `CFTypeRef`.
    sig!("IOPSCopyPowerSourcesInfo", P),
    sig!("IOPSCopyPowerSourcesList", P, P),
    sig!("IOPSGetPowerSourceDescription", P, P, P),
    // `CFRunLoopSourceRef IOPSNotificationCreateRunLoopSource(
    //     IOPowerSourceCallbackType callback, void *context)` — the callback is
    // guest code the host invokes from the run loop when the battery changes,
    // so it is substituted like any other. The context is an opaque `void *`
    // handed straight back to that callback and is never dereferenced by the
    // framework, which is what makes it a plain pointer rather than the
    // structure-of-function-pointers problem `CFRunLoopSourceCreate` is.
    cbsig!([0], "IOPSNotificationCreateRunLoopSource", P, P, P),
    sig!("IORegistryEntryCreateCFProperties", I32, U32, P, P, U32),
    sig!("IORegistryEntryCreateCFProperty", P, U32, P, P, U32),
    // An iterator over the registry, and the two ways of naming a starting
    // point. `IORegistryEntrySearchCFProperty` is the recursive form, and its
    // `plane` is a C string rather than a `CFStringRef`.
    sig!("IORegistryEntryCreateIterator", I32, U32, P, U32, P),
    sig!("IORegistryEntryFromPath", U32, U32, P),
    sig!("IORegistryEntrySearchCFProperty", P, U32, P, P, P, U32),
    sig!("IORegistryEntryGetParentEntry", I32, U32, P, P),
    sig!("IOServiceGetMatchingServices", I32, U32, P, P),
    sig!("IOServiceMatching", P, P),
    // `KBGetLayoutType(SInt16) -> SInt32`, and the only Apple entry point in
    // this table with **no header anywhere on the system** — it is private
    // HIToolbox, present in `Carbon.tbd`'s export list and declared nowhere.
    //
    // So the shape comes from the image, which is this project's own rule for
    // "which bytes are code" applied to a signature. libSDL3 calls it:
    //
    // ```text
    // callq  _LMGetKbdType        ; UInt8 in al
    // movzbl %al, %edi            ; one argument, 32 bits
    // callq  _KBGetLayoutType
    // cmpl   $0x49534f20, %eax    ; 32-bit result, against 'ISO '
    // ```
    //
    // One integer in, one integer out, and that is the whole of what the
    // marshaller needs: the guest puts a value in `edi` and reads `eax`, the
    // host takes `w0` and returns `w0`. A wider prototype would be a guess
    // about a function nobody documents; this is a reading of the caller.
    sig!("KBGetLayoutType", I32, I32),
    // `EventTime GetCurrentEventTime(void)` is a `double` of seconds since
    // boot, and `OSErr UpdateSystemActivity(UInt8)` is how a program that is
    // busy but not touching the keyboard tells the system not to sleep — which
    // a game does every frame. Both are Carbon, both are declared in headers
    // the SDK still ships, and both return through the same convention the
    // Component Manager entries above use: `OSErr` is a `SInt16` the callee
    // extends to 32 bits.
    sig!("GetCurrentEventTime", F64),
    sig!("UpdateSystemActivity", I32, U8),
    // The two error-string lookups beside them, which hand back a pointer into
    // a static table.
    sig!("GetMacOSStatusCommentString", P, I32),
    sig!("GetMacOSStatusErrorString", P, I32),
    // `UInt8 LMGetKbdType(void)` — the low-memory global that still backs
    // `UCKeyTranslate`'s keyboard-type argument.
    sig!("LMGetKbdType", U8),
    // ## AppKit and Foundation's C surface
    //
    // `NSApplicationMain` is where an application stops being a program and
    // becomes an event loop: it loads the main nib, creates `NSApp`, sends
    // `applicationDidFinishLaunching:` to the guest's delegate, and does not
    // return until the application terminates. Everything it does after that
    // point reaches the guest through the Objective-C paths M6 built.
    sig!("NSApplicationMain", I32, I32, P),
    // `NSLog` takes an `NSString` format, not a C one, so even the rendering
    // path the `printf` family uses needs a conversion first. Classified so it
    // fails as "variadic, and no shim implements it" rather than as an
    // undescribed symbol.
    vsig!("NSLog", V, P),
    // Shimmed, and listed for the reason `NSLog` is. The format is the *second*
    // argument, so the entry names five pointers before the tail.
    vsig!("NSRunAlertPanel", I64, P, P, P, P, P),
    sig!("NSHomeDirectory", P),
    sig!("NSClassFromString", P, P),
    // `SEL NSSelectorFromString(NSString *)` — two pointers, and the reason it
    // needs no translation is [M6.2](../../../docs/ROADMAP.md)'s: a guest's
    // `@selector(…)` references were uniqued into the **host's** selectors at
    // registration, so a `SEL` is the same value on both sides. A guest that
    // builds one from a string here and compares it against one of its own gets
    // the equality it wrote the code expecting.
    sig!("NSSelectorFromString", P, P),
    // The handler is a guest function pointer the *runtime* calls, from inside
    // whatever raised — so it goes out as a thunk like every other callback
    // crossing the seam.
    cbsig!([0], "NSSetUncaughtExceptionHandler", V, P),
    sig!("NSSearchPathForDirectoriesInDomains", P, U64, U64, U8),
    sig!("NSTemporaryDirectory", P),
    // The `Add` pair returns the value *after* adding, and the
    // `CompareAndSwap` pair returns whether the swap happened. Two adjacent
    // entries in one deprecated header, two different return types, and
    // declaring `Add` as `BOOL` is not a refusal or a fault — the sum is
    // truncated to a byte, and a caller that uses the result as an offset
    // indexes from the wrong place. Unity's block allocator does exactly that:
    // it adds its block size to a cursor and stores through the answer, so a
    // truncated return wrote one block *below* a reservation, faulting at an
    // address the trace showed being mapped correctly moments earlier.
    sig!("OSAtomicAdd32", I32, I32, P),
    sig!("OSAtomicAdd32Barrier", I32, I32, P),
    sig!("OSAtomicAdd64", I64, I64, P),
    sig!("OSAtomicAdd64Barrier", I64, I64, P),
    sig!("OSAtomicCompareAndSwap32", BOOL, I32, I32, P),
    sig!("OSAtomicCompareAndSwap32Barrier", BOOL, I32, I32, P),
    sig!("OSAtomicCompareAndSwap64", BOOL, I64, I64, P),
    sig!("OSAtomicCompareAndSwap64Barrier", BOOL, I64, I64, P),
    sig!("OSAtomicCompareAndSwapInt", BOOL, I32, I32, P),
    sig!("OSAtomicCompareAndSwapIntBarrier", BOOL, I32, I32, P),
    sig!("OSAtomicCompareAndSwapPtr", BOOL, P, P, P),
    sig!("OSAtomicCompareAndSwapPtrBarrier", BOOL, P, P, P),
    sig!("OSMemoryBarrier", V),
    // `OSSpinLock` is a bare `int32_t` the guest allocates itself, and the
    // host's implementation operates on it in place — the same arrangement as
    // `pthread_mutex_t`, and true for the same checked reason: there is no
    // pointer and no `long` in it, so the two architectures cannot disagree
    // about its layout. What the two architectures *do* disagree about is the
    // spin, and that is the host's problem rather than a signature's.
    sig!("OSSpinLockLock", V, P),
    sig!("OSSpinLockTry", BOOL, P),
    sig!("OSSpinLockUnlock", V, P),
    // Filed after the `OSAtomic` family because lowercase `p` sorts above `S`,
    // which is now a reader's convenience rather than a correctness condition —
    // [`index`] finds it wherever it is. See `CloseComponent` for what the
    // Component Manager is doing here at all.
    sig!("OpenAComponent", I32, P, P),
    sig!("SCDynamicStoreCopyProxies", P, P),
    // ## The Keychain, which is where a game keeps the token it logged in with
    //
    // Every argument is a `CFTypeRef` and every return an `OSStatus`, so these
    // are scalar in the only sense that matters: a Core Foundation object is a
    // host allocation on both sides, and the `kSec*` keys the guest builds its
    // query dictionary from are *data* imports resolving to the host's own
    // constants — so the dictionary that arrives is keyed by the same pointers
    // the framework compares against.
    //
    // The out-parameter is a `CFTypeRef *` the guest owns and the framework
    // writes a **retained** object into, which the guest then releases. That
    // works here for the reason the rest of the CF surface does and is worth
    // stating once: the retain count being manipulated is the host object's own.
    sig!("SecItemAdd", I32, P, P),
    sig!("SecItemCopyMatching", I32, P, P),
    sig!("SecItemDelete", I32, P),
    sig!("SecItemUpdate", I32, P, P),
    // ## `SCDynamicStore` and `SCPreferences`, which is how a game asks what the
    // network is doing
    //
    // `SCDynamicStoreCreate`'s third argument is a callback the host invokes on
    // the run loop when a watched key changes, and its fourth is an
    // `SCDynamicStoreContext` — `version`, `info`, and the same `retain` /
    // `release` / `copyDescription` triple `CFStreamClientContext` has. It is
    // declared exactly as `CFReadStreamSetClient` is, and carries the same
    // caveat: a guest that fills those three in would hand the host x86-64
    // addresses inside a structure this runtime never sees. Conventionally they
    // are null, and Roblox's are.
    //
    // Everything else in the family is Core Foundation objects and scalars.
    cbsig!([2], "SCDynamicStoreCreate", P, P, P, P, P),
    sig!("SCDynamicStoreCopyValue", P, P, P),
    sig!("SCDynamicStoreKeyCreateNetworkGlobalEntity", P, P, P, P),
    sig!("SCNetworkInterfaceGetInterfaceType", P, P),
    sig!("SCNetworkServiceCopy", P, P, P),
    sig!("SCNetworkServiceGetInterface", P, P),
    sig!("SCPreferencesCreate", P, P, P, P),
    sig!("SCPreferencesGetValue", P, P, P),
    sig!("SCNetworkReachabilityCreateWithAddress", P, P, P),
    sig!("SCNetworkReachabilityCreateWithName", P, P, P),
    sig!("SCNetworkReachabilityGetFlags", U8, P, P),
    sig!("SCNetworkReachabilityScheduleWithRunLoop", U8, P, P, P),
    cbsig!([1], "SCNetworkReachabilitySetCallback", U8, P, P, P),
    sig!("SCNetworkReachabilityUnscheduleFromRunLoop", U8, P, P, P),
    // ## The two Security calls a player makes about certificates
    //
    // Unity's networking asks the system for its trust anchors before it
    // validates anything, and copies each certificate's DER out to hand to
    // whichever TLS implementation it was built against. Both are pointer in,
    // pointer or `OSStatus` out; the `CFArrayRef *` and `CFDataRef` on either
    // side are host allocations the guest only ever passes back to
    // CoreFoundation, which already crosses.
    //
    // `SecCertificateCopyData` is the copying spelling of the deprecated
    // `SecCertificateGetData` below it, and a program that has both imports
    // both — this one does.
    sig!("SecCertificateCopyData", P, P),
    sig!("SecCertificateGetData", I32, P, P),
    sig!("SecKeychainItemCopyContent", I32, P, P, P, P, P),
    sig!("SecKeychainItemFreeContent", I32, P, P),
    sig!("SecKeychainOpen", I32, P, P),
    sig!("SecKeychainSearchCopyNext", I32, P, P),
    sig!("SecKeychainSearchCreateFromAttributes", I32, P, U32, P, P),
    sig!("SecTrustCopyAnchorCertificates", I32, P),
    sig!("SecStaticCodeCreateWithPath", I32, P, U64, P),
    sig!("SecRequirementCreateWithString", I32, P, U64, P),
    sig!("SecStaticCodeCheckValidityWithErrors", I32, P, U64, P, P),
    // ## Text input sources, for the keyboard layout
    //
    // SDL3 asks the current layout for its Unicode key-layout data so it can
    // translate a virtual key code into characters. Passthrough, because the
    // layout being asked about is the one the user has selected in this
    // session — there is no guest-side keyboard for it to disagree with.
    // The three "which layout is current" spellings differ in what they do
    // when the selected source cannot produce ASCII — a Chinese or Japanese
    // input method — rather than in their shape: the ASCII-capable one falls
    // back to a Roman layout so a game's WASD keeps working, and Unity asks for
    // that one. All three take nothing and hand back a `TISInputSourceRef`.
    sig!("TISCopyCurrentASCIICapableKeyboardLayoutInputSource", P),
    sig!("TISCopyCurrentKeyboardInputSource", P),
    sig!("TISCopyCurrentKeyboardLayoutInputSource", P),
    sig!("TISGetInputSourceProperty", P, P, P),
    // `OSStatus UCKeyTranslate(const UCKeyboardLayout *, UInt16, UInt16,
    // UInt32, UInt32, OptionBits, UInt32 *, UniCharCount, UniCharCount *,
    // UniChar[])` — the call that turns a virtual key code into characters,
    // read out of CarbonCore's `UnicodeUtilities.h`.
    //
    // **Ten integer arguments, two of them past AAPCS64's eighth**, which is
    // exactly the case [`super::marshal`] documents its limits around. Both
    // overflowing arguments — `actualStringLength` and `unicodeString` — are
    // pointers, so both are already a doubleword wide, which is the condition
    // Apple's stack-packing rule imposes: a narrower one would sit at `[sp+4]`
    // where a `u64` prototype writes `[sp+8]`. `glTexImage2D` needed one such
    // slot; this needs two, and they are the same shape.
    //
    // `UniCharCount` is `unsigned long`, so a doubleword, and the two `UInt16`s
    // are halfwords rather than words — narrow arguments in registers, where
    // the packing rule does not apply.
    sig!(
        "UCKeyTranslate",
        I32,
        P,
        U16,
        U16,
        U32,
        U32,
        U32,
        P,
        U64,
        P,
        P
    ),
    sig!("UTTypeCopyPreferredTagWithClass", P, P, P),
    sig!("UTTypeCreatePreferredIdentifierForTag", P, P, P, P),
    // ## Blocks
    //
    // `_Block_copy` is **shimmed** — see [`crate::shims::block::block_copy`].
    // The signature stays for [`longjmp`]'s reason: it is what stops the site
    // being promoted to an inlined fast import and what names it at an exit.
    //
    // What the entry used to say, and why it was wrong, is worth keeping: it
    // read "copying a block is a memory operation … neither reads the `invoke`
    // field", which is true and insufficient. A block carrying
    // `BLOCK_HAS_COPY_DISPOSE` has a **copy helper** in its descriptor, and
    // `_Block_copy` calls it on the new literal before returning. That is guest
    // code, run by the host, inside this call.
    sig!("_Block_copy", P, P),
    sig!("_Block_object_assign", V, P, P, I32),
    sig!("_Block_object_dispose", V, P, I32),
    // Answered by `crate::shims::runtime` rather than by the host, whose
    // executable path is only the right answer because `crate::bundle` made it
    // so. Two underscores in the Mach-O, one after `c_name` strips the mangling.
    // The rest of the process-state family [`crate::shims::runtime`] answers
    // with the host's own. Resolving them was not enough: an import needs a
    // signature to be *called*, and these had the resolution without one, which
    // is a refusal by name rather than a wrong value — and is how steamclient's
    // initializer reported it.
    sig!("_NSGetArgc", P),
    sig!("_NSGetArgv", P),
    sig!("_NSGetEnviron", P),
    sig!("_NSGetExecutablePath", I32, P, P),
    // Clang's stack-clash probe. It has no arguments and no result; it only
    // touches the guest stack before the function's body continues.
    sig!("___chkstk_darwin", V),
    // The C++ exception surface, all of it shimmed: see [`crate::unwind`] for
    // why none of it can reach the host's libc++abi. `_ZSt9terminatev` is the
    // exception to the exception — `std::terminate` really does mean "stop this
    // process", and the host's does that correctly.
    // sig!("_Unwind_Resume", V, P),
    // The `basic_string` surface used to be described here, on the reasoning that
    // every entry point is a `this` pointer and scalars and so the host's libc++
    // could rewrite the guest's object in place. That reasoning was wrong, and
    // wrongly in the way this table cannot see: the *layout* of a `basic_string`
    // differs between x86-64 and arm64 macOS, so a correct signature still
    // delivered the object to code that reads different fields out of it. All of
    // it is now implemented against the guest's own layout in
    // [`crate::shims::cxxstring`], which is also where the evidence is written
    // down.
    // What a guest class deriving from `std::exception` reaches: the base's
    // constructor and destructor, and `operator delete`. Ordinary host calls —
    // the object is guest memory the host writes a vtable pointer into, which
    // works for the same reason a `pthread_mutex_t` does.
    // sig!("_ZNKSt9exception4whatEv", P, P),
    // sig!("_ZNSt11logic_errorC2EPKc", V, P, P),
    // sig!("_ZNSt12length_errorD1Ev", V, P),
    // sig!("_ZNSt13runtime_errorD1Ev", V, P),
    // `std::call_once`'s out-of-line half. The third argument is the guest's
    // own thunk-taking function pointer — libc++ calls it with the second as
    // its argument — so it crosses host->guest like any other callback.
    // cbsig!([2], "_ZNSt3__111__call_onceERVmPvPFvS2_E", V, P, P, P),
    // sig!("_ZNSt3__112__next_primeEm", U64, U64),
    // libc++'s per-thread bookkeeping block, constructed in place.
    // sig!("_ZNSt3__115__thread_structC1Ev", V, P),
    // sig!("_ZNSt3__115__thread_structD1Ev", V, P),
    // `condition_variable::__do_timed_wait(unique_lock<mutex> &, time_point)`.
    // The `time_point` is by value and is one `long long` — a single eightbyte
    // to SysV and a single doubleword composite to AAPCS64, which is the one
    // shape where the two ABIs agree without the aggregate marshaller.
    // sig!(
    //     "_ZNSt3__118condition_variable15__do_timed_waitERNS_11unique_lockINS_5mutexEEENS_6chrono10time_pointINS5_12system_clockENS5_8durationIxNS_5ratioILl1ELl1000000000EEEEEEE",
    //     V,
    //     P,
    //     P,
    //     I64
    // ),
    // sig!("_ZNSt3__119__thread_local_dataEv", P),
    // `std::mutex` and `std::thread` are a `pthread_mutex_t` and a `pthread_t`
    // with a vtable-free wrapper around them, and both of those are
    // byte-identical on the two architectures — the fact M4 rests on. So the
    // host's libc++ operates on the guest's object in place.
    // `std::recursive_timed_mutex`, reached by a `steamclient.dylib`
    // initializer. Passthrough for the same reason as `std::mutex` above:
    // it is a `pthread_mutex_t`, a `pthread_cond_t`, a count and a thread id,
    // and the first two are byte-identical on the two architectures.
    // `try_lock` returns `bool`, which SysV leaves in `al`.
    // sig!("_ZNSt3__121recursive_timed_mutex4lockEv", V, P),
    // sig!("_ZNSt3__121recursive_timed_mutex6unlockEv", V, P),
    // sig!("_ZNSt3__121recursive_timed_mutex8try_lockEv", U8, P),
    // sig!("_ZNSt3__121recursive_timed_mutexC1Ev", V, P),
    // sig!("_ZNSt3__121recursive_timed_mutexC2Ev", V, P),
    // sig!("_ZNSt3__121recursive_timed_mutexD1Ev", V, P),
    // sig!("_ZNSt3__121recursive_timed_mutexD2Ev", V, P),
    // sig!("_ZNSt3__15mutex4lockEv", V, P),
    // sig!("_ZNSt3__15mutex6unlockEv", V, P),
    // sig!("_ZNSt3__15mutexD1Ev", V, P),
    // `chrono::{steady,system}_clock::now()`. A `time_point` is one `long long`,
    // which is the same single-eightbyte shape `__do_timed_wait` takes one of —
    // the one aggregate both ABIs agree about without the marshaller.
    // sig!("_ZNSt3__16chrono12steady_clock3nowEv", I64),
    // sig!("_ZNSt3__16chrono12system_clock3nowEv", I64),
    // sig!("_ZNSt3__16thread4joinEv", V, P),
    // sig!("_ZNSt3__16threadD1Ev", V, P),
    // sig!("_ZNSt9exceptionC1Ev", V, P),
    // sig!("_ZNSt9exceptionC2Ev", V, P),
    // sig!("_ZNSt9exceptionD0Ev", V, P),
    // sig!("_ZNSt9exceptionD1Ev", V, P),
    // sig!("_ZNSt9exceptionD2Ev", V, P),
    // sig!("_ZSt9terminatev", V),
    // sig!("_ZdaPv", V, P),
    // sig!("_ZdaPvm", V, P, U64),
    // sig!("_ZdlPv", V, P),
    // sig!("_ZdlPvm", V, P, U64),
    // sig!("_Znam", P, U64),
    // sig!("_Znwm", P, U64),
    // ## The locale-aware C library, which the vendored libc++ actually calls
    //
    // The `_l` family — `___mb_cur_max_l`, `___tolower_l`, `___toupper_l`,
    // `__maskrune_l`, `btowc_l`, `freelocale`, `localeconv_l`, `mbrlen_l`,
    // `mbrtowc_l`, `mbsnrtowcs_l`, `mbsrtowcs_l`, `mbtowc_l`, `newlocale`,
    // `nl_langinfo_l`, `strcoll_l`, `strftime_l`, `strtod_l`, `strtof_l`,
    // `strxfrm_l`, `wcrtomb_l`, `wcscoll_l`, `wcsnrtombs_l`, `wcsxfrm_l` and
    // `wctob_l`, scattered through this table by name — plus the wide-character
    // calls libc++ reaches on the same paths.
    //
    // Every prototype was read out of the SDK headers (`xlocale/_ctype.h`,
    // `xlocale/_stdlib.h`, `xlocale/_wchar.h`) rather than recalled, because
    // `mbtowc_l` had been wrong in this table for as long as it had been in it
    // and nothing found it — see its own note.
    //
    // `locale_t` is an opaque `struct _xlocale *` that the guest only ever
    // obtains from `newlocale` (or as `LC_GLOBAL_LOCALE`) and hands straight
    // back, so it crosses as a plain pointer and neither side reads the
    // structure behind it. That is what makes this whole family passthrough
    // rather than the [`crate::cxxlib`] problem again.
    sig!("___mb_cur_max_l", I32, P),
    sig!("___tolower_l", I32, I32, P),
    sig!("___toupper_l", I32, I32, P),
    sig!("__assert_rtn", V, P, P, I32, P),
    sig!("__bzero", V, P, U64),
    // sig!("__cxa_allocate_exception", P, U64),
    // Shimmed, and listed for the reasons `sigaction` is: the entry keeps it out
    // of the audit's unknown list and off the fast path. The handler is a guest
    // function pointer, but no `cbsig!` marks it — [`crate::shims::atexit`]
    // substitutes it itself, because it must also replace the DSO handle.
    sig!("__cxa_atexit", I32, P, P, P),
    // sig!("__cxa_begin_catch", P, P),
    // sig!("__cxa_call_unexpected", V, P),
    // sig!("__cxa_end_catch", V),
    // sig!("__cxa_free_exception", V, P),
    // The function-local static guards. The guard object is eight bytes of
    // *guest* memory and libc++abi operates on it in place, holding its own
    // process-wide lock while it does — the same in-place arrangement the
    // `pthread` types rest on, and for the same reason: there is one process
    // here, so one lock is the right number.
    // sig!("__cxa_guard_abort", V, P),
    // sig!("__cxa_guard_acquire", I32, P),
    // sig!("__cxa_guard_release", V, P),
    // sig!("__cxa_pure_virtual", V),
    // sig!("__cxa_rethrow", V),
    // sig!("__cxa_throw", V, P, P, P),
    sig!("__darwin_check_fd_set_overflow", I32, I32, P, I32),
    sig!("__error", P),
    sig!("__exp10f", F32, F32),
    sig!("__isnand", I32, F64),
    // The `float` twin, which clang emits for `isnan()` on a `float` — a
    // different symbol from `__isnand` and not a variant of it, so a table with
    // only the `double` form leaves it undescribed.
    sig!("__isnanf", I32, F32),
    // The rest of `<math.h>`'s classification macros, which are functions on
    // Darwin. Each takes its argument in a floating-point register and answers an
    // `int`, and each width is its own symbol: `f` is a `float`, the bare name a
    // `double`, `l` a `long double` — which arrives as a `double` here, since
    // `long double` on x86-64 is what [`crate::abi::table::REFUSED`] declines
    // and clang emits the `d` form for anything narrower.
    sig!("__isfinitef", I32, F32),
    sig!("__isfinited", I32, F64),
    sig!("__isinff", I32, F32),
    sig!("__isinfd", I32, F64),
    sig!("__isnormalf", I32, F32),
    sig!("__isnormald", I32, F64),
    sig!("__signbitf", I32, F32),
    sig!("__signbitd", I32, F64),
    sig!("__fpclassifyf", I32, F32),
    sig!("__fpclassifyd", I32, F64),
    sig!("__maskrune", I32, I32, U64),
    sig!("__maskrune_l", I32, I32, U64, P),
    sig!("__memcpy_chk", P, P, P, U64, U64),
    sig!("__memmove_chk", P, P, P, U64, U64),
    sig!("__memset_chk", P, P, I32, U64, U64),
    vsig!("__snprintf_chk", I32, P, U64, I32, U64, P),
    vsig!("__sprintf_chk", I32, P, I32, U64, P),
    sig!("__stack_chk_fail", V),
    sig!("__stpcpy_chk", P, P, P, U64),
    sig!("__strcat_chk", P, P, P, U64),
    sig!("__strcpy_chk", P, P, P, U64),
    sig!("__strlcat_chk", U64, P, P, U64, U64),
    sig!("__strlcpy_chk", U64, P, P, U64, U64),
    sig!("__strncat_chk", P, P, P, U64, U64),
    sig!("__strncpy_chk", P, P, P, U64, U64),
    sig!("__tolower", I32, I32),
    sig!("__toupper", I32, I32),
    // Darwin's futex, which is how a modern `libc++` blocks on
    // `std::atomic::wait` and how mimalloc parks a contended thread. It passes
    // through: the address is guest memory, guest memory is host memory, and the
    // kernel hashes the address the same either way — the only thing that would
    // break it is a guest and a host thread waiting on the *same* word, which
    // cannot happen because the word is the guest's.
    //
    // `__ulock_wait` takes its compare value as a `uint64_t` and the timeout as
    // `uint32_t` microseconds; `__ulock_wait2` widens the timeout to nanoseconds
    // in a `uint64_t` and adds a second value. Both are declared here because a
    // guest built against a newer SDK calls the second and one built against an
    // older one calls the first.
    sig!("__ulock_wait", I32, U32, P, U64, U32),
    sig!("__ulock_wait2", I32, U32, P, U64, U64, U64),
    sig!("__ulock_wake", I32, U32, P, U64),
    // The fortified `vsnprintf`; shimmed with the rest of the `va_list` family
    // for the reason [`crate::shims::valist`] gives.
    sig!("__vsnprintf_chk", I32, P, U64, I32, U64, P, P),
    vsig!("__vsprintf_chk", I32, P, I32, U64, P),
    // What `@available` / `__builtin_available` compiles to:
    // `bool _availability_version_check(uint32_t count, dyld_build_version_t *)`.
    // A `dyld_build_version_t` is two `uint32_t` — platform and version — so the
    // array the guest passes is read identically on both architectures, and it
    // travels by pointer rather than by value.
    //
    // Passthrough is the *right* answer here rather than a convenient one. The
    // question is "is the OS running this process at least version X", and the
    // OS running this process is the host's — an x86-64 binary asking it on
    // Rosetta 2 gets the same answer from the same function. SDL3's Cocoa
    // backend asks it before using anything version-gated, which is most of
    // AppKit, and reached it from a *guest callback* — so a refusal surfaces on
    // whatever thread AppKit was calling into rather than at start-up.
    sig!("_availability_version_check", BOOL, U32, P),
    // Guest dyld image introspection is served by [`crate::dyld`], which knows
    // the x86-64 executable and libraries this linker mapped.
    sig!("_dyld_image_count", U32),
    sig!("_dyld_get_image_header", P, U32),
    sig!("_dyld_get_image_vmaddr_slide", U64, U32),
    sig!("_dyld_image_containing_address", P, P),
    cbsig!([0], "_dyld_register_func_for_add_image", V, P),
    cbsig!([0], "_dyld_register_func_for_remove_image", V, P),
    sig!("_dyld_get_image_name", P, U32),
    sig!("_exit", V, I32),
    // The Darwin spellings that skip the signal mask, imported as `__longjmp`
    // and `__setjmp`. One implementation serves both pairs: the distinction is
    // about a mask this runtime does not model yet. See [`crate::jmpbuf`].
    sig!("_longjmp", V, P, I32),
    sig!("_setjmp", I32, P),
    // `void _tlv_atexit(void (*termfunc)(void *), void *objAddr)` — the
    // destructor half of the thread-local family. Unlike the bootstrap below it
    // is the *host's* own function (see [`crate::shims::runtime`]); what it
    // needs from this table is the one thing a raw passthrough would get wrong,
    // which is that `termfunc` is guest code dyld calls as arm64 at thread
    // exit.
    cbsig!([0], "_tlv_atexit", V, P, P),
    // Both spellings of the thread-local bootstrap. It is an ordinary host
    // function taking the descriptor's address and returning this thread's
    // address for the variable — see [`crate::tls`] — and it is reached through
    // an *indirect* call (`callq *(%rdi)`), not a stub, so the signature is what
    // lets the runtime service it at all.
    sig!("_tlv_bootstrap", P, P),
    sig!("abort", V),
    sig!("abs", I32, I32),
    sig!("accept", I32, I32, P, P),
    sig!("access", I32, P, I32),
    sig!("acl_free", I32, P),
    sig!("acl_get_entry", I32, P, I32, P),
    sig!("acl_get_flag_np", I32, P, I32),
    sig!("acl_get_flagset_np", I32, P, P),
    sig!("acl_get_link_np", P, P, I32),
    sig!("acl_get_perm_np", I32, P, I32),
    sig!("acl_get_permset", I32, P, P),
    sig!("acl_get_qualifier", P, P),
    sig!("acl_get_tag_type", I32, P, P),
    sig!("acos", F64, F64),
    sig!("acosf", F32, F32),
    sig!("acosh", F64, F64),
    sig!("acoshf", F32, F32),
    // C11's aligned allocation, reached because the vendored libc++abi uses it
    // for an exception object with an over-aligned type in it. Its result is
    // freed with the ordinary `free`, which is already passed through, so the
    // two halves stay on the same heap.
    sig!("aligned_alloc", P, U64, U64),
    sig!("arc4random", U32),
    sig!("arc4random_buf", V, P, U64),
    sig!("asin", F64, F64),
    sig!("asinf", F32, F32),
    sig!("asinh", F64, F64),
    sig!("asinhf", F32, F32),
    vsig!("asprintf", I32, P, P),
    sig!("atan", F64, F64),
    sig!("atan2", F64, F64, F64),
    sig!("atan2f", F32, F32, F32),
    sig!("atanf", F32, F32),
    sig!("atanh", F64, F64),
    sig!("atanhf", F32, F32),
    // `atexit`, `bsearch`, `qsort` and `qsort_r` were refused until the
    // host->guest thunk existed. What changed is only that the pointer they
    // take can now be substituted; the signature was never in doubt.
    cbsig!([0], "atexit", I32, P),
    sig!("atof", F64, P),
    sig!("atoi", I32, P),
    sig!("atol", I64, P),
    sig!("atoll", I64, P),
    // `<execinfo.h>`, and the reason a *crash handler* is the first thing a
    // C++ game reaches: Starbound installs one before it does anything else, so
    // this is in front of its start-up rather than after a fault.
    //
    // Passthrough walks the **host's** stack, which is this runtime's frames
    // and not the guest's — a guest that prints the result gets a trace of the
    // JIT rather than of itself. That is the honest answer available here and
    // it is not a wrong one: the guest's own frames live on a guest stack the
    // host unwinder has never heard of, and reporting them means walking it
    // with [`crate::unwind`], which is a different piece of work. What matters
    // for start-up is that installing a handler does not stop the program.
    sig!("backtrace", I32, P, I32),
    sig!("backtrace_symbols", P, P, I32),
    sig!("backtrace_symbols_fd", V, P, I32, I32),
    // `basename` and `dirname` both hand back a pointer into a static buffer
    // the host owns, which the guest only ever reads.
    sig!("basename", P, P),
    sig!("bcmp", I32, P, P, U64),
    // `void bcopy(const void *src, void *dst, size_t n)` — the BSD spelling,
    // and the one place in this table where the two pointers are the other way
    // round from `memcpy`. Nothing here depends on the order, since both are
    // `P`; it is written down because a reader checking this entry against
    // `memcpy` two hundred lines below would otherwise conclude it was wrong.
    sig!("bcopy", V, P, P, U64),
    sig!("bind", I32, I32, P, U32),
    // `kern_return_t bootstrap_look_up(mach_port_t, const name_t, mach_port_t *)`.
    // How a guest finds a service registered with launchd — for UNDERTALE, the
    // running Steam client. Ports are plain 32-bit names, and the identity
    // mapping makes the out-parameter directly writable.
    sig!("bootstrap_look_up", I32, U32, P, P),
    cbsig!([4], "bsearch", P, P, P, U64, U64, P),
    sig!("btowc_l", I32, I32, P),
    sig!("bzero", V, P, U64),
    sig!("calloc", P, U64, U64),
    sig!("cbrt", F64, F64),
    sig!("cbrtf", F32, F32),
    sig!("ceil", F64, F64),
    sig!("ceilf", F32, F32),
    sig!("chdir", I32, P),
    sig!("chmod", I32, P, U32),
    sig!("clearerr", V, P),
    // `clock_t clock(void)`. `clock_t` is `unsigned long` on Darwin — 64 bits on
    // both architectures — so the value crosses whole; a narrower return would
    // truncate a process that has run for more than an hour of CPU time.
    sig!("clock", U64),
    // `struct timespec` is two 64-bit words on both architectures, and the
    // guest passes it by address, so the host fills it in place.
    sig!("clock_get_time", I32, U32, P),
    sig!("clock_gettime", I32, U32, P),
    sig!("clock_gettime_nsec_np", U64, I32),
    // `kern_return_t clock_sleep(mach_port_t, sleep_type_t, mach_timespec_t,
    // mach_timespec_t *)`. The third argument is a struct **by value**, and it
    // is eight bytes of two `unsigned int` — one integer register under SysV
    // AMD64 and one under AAPCS64 alike — so a `U64` places it correctly on both
    // sides. That equivalence is the whole reason it can be a signature; a
    // sixteen-byte aggregate could not be. See `semaphore_timedwait`.
    sig!("clock_sleep", I32, U32, I32, U64, P),
    sig!("close", I32, I32),
    sig!("closedir", I32, P),
    sig!("compat_mode", U8, P, P),
    sig!("compress", I32, P, P, P, U64),
    sig!("connect", I32, I32, P, U32),
    sig!("cos", F64, F64),
    sig!("cosf", F32, F32),
    sig!("cosh", F64, F64),
    sig!("coshf", F32, F32),
    // ## zlib
    //
    // `deflateInit_` and `inflateInit_` are the real entry points: `deflateInit`
    // is a macro that passes `ZLIB_VERSION` and `sizeof(z_stream)`, so a program
    // importing the underscore forms is one that used the ordinary API.
    sig!("adler32", U64, U64, P, U32),
    sig!("alarm", U32, U32),
    // `double cabs(double complex)`. A `double complex` is two doubles, and it is
    // one of the aggregates the two ABIs agree about: SSE/SSE on one side, a
    // two-member homogeneous float aggregate on the other, `xmm0`/`xmm1` and
    // `v0`/`v1`. Written out as its members like [`CGPoint`].
    sig!("cabs", F64, F64, F64),
    sig!("cfsetispeed", I32, P, U64),
    sig!("cfsetospeed", I32, P, U64),
    sig!("chflags", I32, P, U32),
    sig!("closelog", V),
    sig!("confstr", U64, I32, P, U64),
    sig!("crc32", U64, U64, P, U32),
    sig!("creat", I32, P, U32),
    sig!("ctime_r", P, P, P),
    // ## libcurl
    //
    // Passthrough to `/usr/lib/libcurl.4.dylib`, which is the library the guest
    // names in its own install path — so the host's copy *is* the one it asked
    // for. Only the two setters need more than a signature: they are declared
    // variadic, and their argument's type is named by the argument in front of
    // it, one possibility being a function pointer. See
    // [`crate::shims::curl`].
    sig!("curl_easy_cleanup", V, P),
    vsig!("curl_easy_getinfo", I32, P, I32),
    sig!("curl_easy_init", P),
    sig!("curl_easy_perform", I32, P),
    sig!("curl_easy_reset", V, P),
    vsig!("curl_easy_setopt", I32, P, I32),
    sig!("curl_global_cleanup", V),
    sig!("curl_global_init", I32, I64),
    sig!("deflate", I32, P, I32),
    sig!("deflateEnd", I32, P),
    sig!("deflateInit2_", I32, P, I32, I32, I32, I32, I32, P, I32),
    sig!("deflateInit_", I32, P, I32, P, I32),
    sig!("deflateReset", I32, P),
    // ## Grand Central Dispatch, as `steamclient.dylib` uses it
    //
    // A dispatch object is opaque and reference-counted by libdispatch itself,
    // so every one of these is a handle the guest received and hands back —
    // nothing about the object crosses. Prototypes from `<dispatch/*.h>`;
    // `dispatch_time_t` and `uintptr_t` are both `U64`, `intptr_t` is `I64`.
    //
    // The three `_f` forms take a **`dispatch_function_t`**, which is a guest
    // function pointer libdispatch will call on a thread this runtime never
    // created. `cbsig!` marks the parameter so it goes out as a thunk, the way
    // `qsort`'s comparator and `pthread_create`'s start routine do — and it is
    // what makes them slow imports rather than inlined ones, since substitution
    // only exists at an exit. Without the marking libdispatch would call
    // x86-64 as arm64, seconds later, on a thread with nothing of this
    // runtime's on its stack.
    // `double difftime(time_t, time_t)` — two 64-bit integers in, a `double`
    // out, so the arguments and the result come from different register
    // files.
    sig!("difftime", F64, I64, I64),
    sig!("dirname", P, P),
    sig!("dispatch_get_context", P, P),
    sig!("dispatch_get_global_queue", P, I64, U64),
    cbsig!([3], "dispatch_group_async_f", V, P, P, P, P),
    sig!("dispatch_group_create", P),
    sig!("dispatch_group_wait", I64, P, U64),
    cbsig!([2], "dispatch_once_f", V, P, P, P),
    sig!("dispatch_release", V, P),
    sig!("dispatch_resume", V, P),
    sig!("dispatch_queue_create", P, P, P),
    sig!("dispatch_semaphore_create", P, I64),
    // Both halves return `intptr_t` rather than `void`: `wait` answers whether
    // it timed out, and `signal` answers whether a thread was woken. A guest
    // that checks either — and a guest waiting with a timeout always checks —
    // reads `rax`, so the return type is load-bearing here in the way
    // `OSAtomicAdd32Barrier`'s was.
    sig!("dispatch_semaphore_signal", I64, P),
    sig!("dispatch_semaphore_wait", I64, P, U64),
    // Block-taking calls are handled by the block shim, which rewrites the
    // guest invoke pointer before libdispatch sees it. This is essential for
    // dispatch_sync: an uncallable guest block makes its caller wait forever.
    // sig!("dispatch_async", V, P, P),
    // sig!("dispatch_sync", V, P, P),
    // sig!("dispatch_after", V, U64, P, P),
    // sig!("dispatch_once", V, P, P),
    // sig!("dispatch_data_create", P, P, U64, P, P),
    sig!("dispatch_set_context", V, P, P),
    sig!("dispatch_source_cancel", V, P),
    sig!("dispatch_source_create", P, P, U64, U64, P),
    cbsig!([1], "dispatch_source_set_cancel_handler_f", V, P, P),
    cbsig!([1], "dispatch_source_set_event_handler_f", V, P, P),
    sig!("dispatch_source_set_timer", V, P, U64, U64, U64),
    sig!("dispatch_suspend", V, P),
    // `dispatch_time(dispatch_time_t base, int64_t delta)` — the base is an
    // opaque absolute time and the delta is signed nanoseconds, which is why
    // the two arguments are not the same type despite both being doublewords.
    sig!("dispatch_time", U64, U64, I64),
    // `Dl_info` is four pointers and nothing else, so the guest allocates one
    // and the host fills it in place. The strings it comes back pointing at are
    // the host's, which is what a guest asking "what is at this address" wants
    // for a host address and cannot be given for a guest one — the same
    // asymmetry `_dyld_get_image_name` has.
    sig!("dladdr", I32, P, P),
    sig!("dlclose", I32, P),
    sig!("dlerror", P),
    sig!("dlopen", P, P, I32),
    sig!("dlsym", P, P, P),
    vsig!("dprintf", I32, I32, P),
    sig!("dup", I32, I32),
    sig!("dup2", I32, I32, I32),
    sig!("endgrent", V),
    sig!("endpwent", V),
    sig!("endusershell", V),
    vsig!("err", V, I32, P),
    vsig!("errx", V, I32, P),
    sig!("execv", I32, P, P),
    sig!("execve", I32, P, P, P),
    sig!("execvp", I32, P, P),
    sig!("exit", V, I32),
    sig!("exp", F64, F64),
    sig!("exp2", F64, F64),
    sig!("exp2f", F32, F32),
    sig!("expf", F32, F32),
    sig!("expm1", F64, F64),
    sig!("expm1f", F32, F32),
    // ## The rest of `<math.h>`, by width
    //
    // Added as a family rather than one at a time, which is worth saying because
    // everything else in this table got here by stopping a program by name.
    // These are safe to add ahead of that for a reason specific to them: each is
    // one or two `double`s or `float`s in and one out, the widths are in the
    // name, and the arity test compiles every one of them against `<math.h>`.
    // There is no judgement left over for a program to catch us getting wrong —
    // which is exactly not true of the pointer-returning and structure-taking
    // entries elsewhere here, and is why this is a licence for `<math.h>` and
    // not a licence generally.
    //
    // `remquo` is the one with a pointer: the quotient's low bits go to an `int`
    // the caller owns, and the remainder comes back in a vector register.
    sig!("erf", F64, F64),
    sig!("erff", F32, F32),
    sig!("erfc", F64, F64),
    sig!("erfcf", F32, F32),
    sig!("tgamma", F64, F64),
    sig!("tgammaf", F32, F32),
    sig!("lgamma", F64, F64),
    sig!("lgammaf", F32, F32),
    sig!("exp10", F64, F64),
    sig!("exp10f", F32, F32),
    sig!("remainder", F64, F64, F64),
    sig!("remainderf", F32, F32, F32),
    sig!("remquo", F64, F64, F64, P),
    sig!("remquof", F32, F32, F32, P),
    sig!("nextafterf", F32, F32, F32),
    sig!("copysign", F64, F64, F64),
    sig!("copysignf", F32, F32, F32),
    // `double nan(const char *tagp)` takes a **string**, not a number: the
    // payload of the quiet NaN it builds, spelled in digits. Declaring it
    // `F64, F64` would read the pointer out of the wrong register file
    // entirely.
    sig!("nan", F64, P),
    sig!("nanf", F32, P),
    sig!("fabs", F64, F64),
    sig!("fabsf", F32, F32),
    sig!("fchmod", I32, I32, U32),
    sig!("fclose", I32, P),
    vsig!("fcntl", I32, I32, I32),
    sig!("fcopyfile", I32, I32, P, U32),
    sig!("fdopen", P, I32, P),
    sig!("feof", I32, P),
    sig!("ferror", I32, P),
    sig!("fflagstostr", P, U64),
    sig!("fflush", I32, P),
    sig!("fgetc", I32, P),
    sig!("fgetpos", I32, P, P),
    sig!("fgets", P, P, I32, P),
    sig!("fileno", I32, P),
    // ## The POSIX and BSD surface `steamclient.dylib`'s start-up walks
    //
    // `flock`, `fsync`, `ftruncate`, `gethostname`, `getifaddrs`/`freeifaddrs`,
    // `getpagesize`, `getpwuid`, `getrlimit`, `inet_aton`, `mktemp`,
    // `popen`/`pclose`, `pipe`, `pread`, `recvmsg`/`sendmsg`, `setsid`,
    // `socketpair`, `symlink` and `uname`, scattered through this table by name.
    // Each stopped an initializer by name and was added for it, which is how
    // every entry here got here.
    //
    // Every one is scalar or writes through a pointer to a structure that was
    // **checked** to be identical on both architectures rather than assumed to
    // be: `rlimit`, `utsname`, `passwd`, `msghdr`, `ifaddrs`, `in_addr` and
    // `Dl_info` were compiled both ways and their sizes and every field offset
    // compared. That is the question `std::string` turned from a formality into
    // a requirement — see [`crate::shims::cxxstring`].
    sig!("flock", I32, I32, I32),
    // The `FILE *` lock a guest takes around a run of unlocked reads. Both are
    // `void`, and the lock is recursive per thread.
    sig!("flockfile", V, P),
    sig!("floor", F64, F64),
    sig!("floorf", F32, F32),
    sig!("fma", F64, F64, F64, F64),
    sig!("fmaf", F32, F32, F32, F32),
    sig!("fmax", F64, F64, F64),
    sig!("fmaxf", F32, F32, F32),
    sig!("fmin", F64, F64, F64),
    sig!("fminf", F32, F32, F32),
    sig!("fmod", F64, F64, F64),
    sig!("fmodf", F32, F32, F32),
    sig!("fnmatch", I32, P, P, I32),
    sig!("fopen", P, P, P),
    // `fork` passes through, and what the child gets is this whole runtime:
    // the code cache, the thunk table and the guest's memory, all copied by the
    // kernel. That is the same deal a forked native process gets and is why no
    // shim is warranted — but only one thread survives in the child, so a guest
    // that forks while another guest thread holds the cache's lock deadlocks.
    // Unity forks only to spawn its crash handler, before that is possible.
    sig!("fork", I32),
    sig!("fpathconf", I64, I32, I32),
    vsig!("fprintf", I32, P, P),
    sig!("fputc", I32, I32, P),
    sig!("fputs", I32, P, P),
    sig!("fputwc", I32, I32, P),
    sig!("fread", U64, P, U64, U64, P),
    sig!("free", V, P),
    sig!("freeaddrinfo", V, P),
    sig!("freeifaddrs", V, P),
    sig!("freelocale", V, P),
    sig!("freopen", P, P, P, P),
    // `double frexp(double, int *)` — the exponent goes to a pointer the
    // guest owns and the mantissa comes back in a vector register.
    sig!("frexp", F64, F64, P),
    vsig!("fscanf", I32, P, P),
    sig!("fseek", I32, P, I64, I32),
    sig!("fseeko", I32, P, I64, I32),
    sig!("fsetpos", I32, P, P),
    sig!("fstat", I32, I32, P),
    sig!("fstatfs", I32, I32, P),
    sig!("fstatvfs", I32, I32, P),
    sig!("fsync", I32, I32),
    sig!("ftell", I64, P),
    sig!("ftello", I64, P),
    // `int ftime(struct timeb *)`, which is older than POSIX and still
    // exported. `time_t` is eight bytes on both architectures and the three
    // fields after it are `short`s, so the structure the guest passes by
    // address is filled in place.
    sig!("ftime", I32, P),
    sig!("ftruncate", I32, I32, I64),
    sig!("fts_children", P, P, I32),
    sig!("fts_close", I32, P),
    // The comparator takes `const FTSENT **`, which is still just a pointer:
    // `ls` sorts its output with one, so this is the entry that makes the
    // milestone-1 acceptance binary produce output at all.
    cbsig!([2], "fts_open", P, P, I32, P),
    sig!("fts_read", P, P),
    sig!("fts_set", I32, P, P, I32),
    sig!("funlockfile", V, P),
    sig!("futimes", I32, I32, P),
    sig!("fwrite", U64, P, U64, U64, P),
    sig!("gai_strerror", P, I32),
    sig!("getaddrinfo", I32, P, P, P, P),
    sig!("getbsize", P, P, P),
    sig!("getc", I32, P),
    sig!("getcwd", P, P, U64),
    sig!("getdomainname", I32, P, I32),
    sig!("getdtablesize", I32),
    sig!("getegid", U32),
    sig!("getenv", P, P),
    sig!("geteuid", U32),
    sig!("getgid", U32),
    // ### The passwd and group databases
    //
    // `struct passwd` is five pointers, two 32-bit ids and two `time_t`s;
    // `struct group` is two pointers, a `gid_t` and a pointer. Every member is
    // a pointer or a fixed-width integer, so both are byte-identical on the two
    // architectures for the same reason the `pthread` types are — and, as
    // there, that is a fact about *these* structures rather than a general
    // property of libc's.
    sig!("getgrent", P),
    sig!("getgrgid", P, U32),
    sig!("getgrgid_r", I32, U32, P, P, U64, P),
    sig!("getgrnam", P, P),
    sig!("getgrnam_r", I32, P, P, P, U64, P),
    sig!("gethostbyname", P, P),
    sig!("gethostid", I64),
    sig!("gethostname", I32, P, U64),
    sig!("getifaddrs", I32, P),
    sig!("getloadavg", I32, P, I32),
    sig!("getlogin_r", I32, P, U64),
    sig!("getnameinfo", I32, P, U32, P, U32, P, U32, I32),
    sig!("getopt", I32, I32, P, P),
    sig!("getopt_long", I32, I32, P, P, P, P),
    sig!("getpagesize", I32),
    sig!("getpeername", I32, I32, P, P),
    sig!("getpgid", I32, I32),
    sig!("getpid", I32),
    sig!("getppid", I32),
    sig!("getpriority", I32, I32, U32),
    sig!("getprogname", P),
    // `struct protoent` — a name, an alias vector and an `int` — comes back the
    // same way `getpwuid`'s does, as a pointer into libSystem's storage, and has
    // the same layout on both architectures.
    sig!("getprotobyname", P, P),
    sig!("getpwent", P),
    sig!("getpwnam", P, P),
    sig!("getpwnam_r", I32, P, P, P, U64, P),
    // `struct passwd` comes back as a pointer into libSystem's own storage,
    // which the guest then reads directly — so this one depends on the layout
    // check above in a way the writers-through-a-pointer do not.
    sig!("getpwuid", P, U32),
    sig!("getpwuid_r", I32, U32, P, P, U64, P),
    sig!("getrlimit", I32, I32, P),
    sig!("getrusage", I32, I32, P),
    // `const struct section_64 *getsectbynamefromheader_64(const struct
    // mach_header_64 *, const char *seg, const char *sect)`.
    //
    // The one pointer argument that is a *Mach-O*, and that is what makes it
    // passthrough rather than a shim: a `mach_header_64` and the load commands
    // behind it are a **file format**, byte-identical whichever architecture the
    // image is for — the `cputype` field is data, not layout. So libmacho's own
    // implementation walks the guest's x86-64 header correctly, and the
    // `section_64 *` it returns points into guest memory, which the guest can
    // read because everything here is identity-mapped.
    //
    // Reached from a `_dyld_register_func_for_add_image` callback, which is the
    // only way a guest gets a header to ask about; see [`crate::dyld`].
    sig!("getsectbynamefromheader_64", P, P, P, P),
    sig!("getservbyname", P, P, P),
    sig!("getsid", I32, I32),
    sig!("getsockname", I32, I32, P, P),
    sig!("getsockopt", I32, I32, I32, I32, P, P),
    sig!("gettimeofday", I32, P, P),
    sig!("getuid", U32),
    sig!("getwc", I32, P),
    sig!("getxattr", I64, P, P, P, U64, U32, I32),
    sig!("getentropy", I32, P, U64),
    sig!("getattrlist", I32, P, P, P, U64, U64),
    // ## OpenGL
    //
    // The whole of what a GameMaker runner draws with, and the most mechanical
    // block in this table: `GLenum`, `GLuint` and `GLbitfield` are `unsigned
    // int`, `GLint` and `GLsizei` are `int`, `GLfloat` is `float`, `GLclampd`
    // is `double`, and `GLboolean` is `unsigned char`. The distinction between
    // `GLfloat` and `GLclampd` is the one worth checking rather than assuming:
    // `glClearColor` takes four `float`s and `glClearDepth` one `double`, so a
    // uniform "floating point is `f64`" rule silently mis-clears the screen.
    sig!("glActiveTexture", V, U32),
    // A `GLclampf` is a `float`, so the reference value lands in a vector
    // register while the comparison function does not.
    sig!("glAlphaFunc", V, U32, F32),
    sig!("glAttachShader", V, U32, U32),
    sig!("glBegin", V, U32),
    sig!("glBindAttribLocation", V, U32, U32, P),
    sig!("glBindBuffer", V, U32, U32),
    sig!("glBindFramebuffer", V, U32, U32),
    sig!("glBindFramebufferEXT", V, U32, U32),
    sig!("glBindRenderbuffer", V, U32, U32),
    sig!("glBindRenderbufferEXT", V, U32, U32),
    sig!("glBindTexture", V, U32, U32),
    // The APPLE vertex-array-object extension, which is what an OpenGL 2.1
    // context on macOS has instead of the core entry points — same shapes as
    // `glGenVertexArrays`/`glBindVertexArray`/`glDeleteVertexArrays`.
    sig!("glBindVertexArrayAPPLE", V, U32),
    sig!("glBlendEquation", V, U32),
    sig!("glBlendFunc", V, U32, U32),
    sig!("glBlendFuncSeparate", V, U32, U32, U32, U32),
    // A `GLsizeiptr` and a `GLintptr` are both `long`, not `int`: a buffer
    // upload of more than 2 GiB is not the case that matters, but a *negative*
    // narrow read of a legitimate size would be.
    sig!("glBufferData", V, U32, I64, P, U32),
    sig!("glBufferSubData", V, U32, I64, I64, P),
    sig!("glCheckFramebufferStatus", U32, U32),
    sig!("glCheckFramebufferStatusEXT", U32, U32),
    sig!("glClear", V, U32),
    sig!("glClearColor", V, F32, F32, F32, F32),
    sig!("glClearDepth", V, F64),
    sig!("glClearStencil", V, I32),
    sig!("glColorMask", V, U8, U8, U8, U8),
    sig!("glCompileShader", V, U32),
    // Eight integer arguments exactly, which is the guest's six registers plus
    // two it spilled; the host takes all eight in `x0`–`x7`.
    sig!(
        "glCompressedTexImage2D",
        V,
        U32,
        I32,
        U32,
        I32,
        I32,
        I32,
        I32,
        P
    ),
    sig!("glCreateProgram", U32),
    sig!("glCreateShader", U32, U32),
    sig!("glDeleteBuffers", V, I32, P),
    sig!("glDeleteFramebuffers", V, I32, P),
    sig!("glDeleteFramebuffersEXT", V, I32, P),
    sig!("glDeleteProgram", V, U32),
    sig!("glDeleteRenderbuffers", V, I32, P),
    sig!("glDeleteRenderbuffersEXT", V, I32, P),
    sig!("glDeleteShader", V, U32),
    sig!("glDeleteTextures", V, I32, P),
    sig!("glDeleteVertexArraysAPPLE", V, I32, P),
    sig!("glDepthFunc", V, U32),
    sig!("glDepthMask", V, U8),
    sig!("glDepthRange", V, F64, F64),
    sig!("glDisable", V, U32),
    sig!("glDisableVertexAttribArray", V, U32),
    sig!("glDrawArrays", V, U32, I32, I32),
    sig!("glDrawBuffer", V, U32),
    sig!("glDrawBuffers", V, I32, P),
    // The last argument is a pointer *or* a byte offset into the bound element
    // buffer, depending on whether one is bound — an integer either way, and
    // identity mapping means a guest pointer needs no translation.
    sig!("glDrawElements", V, U32, I32, U32, P),
    sig!("glEnable", V, U32),
    sig!("glEnableVertexAttribArray", V, U32),
    sig!("glEnd", V),
    sig!("glFinish", V),
    sig!("glFlush", V),
    sig!("glFramebufferRenderbuffer", V, U32, U32, U32, U32),
    sig!("glFramebufferRenderbufferEXT", V, U32, U32, U32, U32),
    sig!("glFramebufferTexture2D", V, U32, U32, U32, U32, I32),
    sig!("glFramebufferTexture2DEXT", V, U32, U32, U32, U32, I32),
    sig!("glFrontFace", V, U32),
    sig!("glGenBuffers", V, I32, P),
    sig!("glGenFramebuffers", V, I32, P),
    sig!("glGenFramebuffersEXT", V, I32, P),
    sig!("glGenRenderbuffers", V, I32, P),
    sig!("glGenRenderbuffersEXT", V, I32, P),
    sig!("glGenTextures", V, I32, P),
    sig!("glGenVertexArraysAPPLE", V, I32, P),
    sig!("glGenerateMipmap", V, U32),
    sig!("glGetActiveUniform", V, U32, U32, I32, P, P, P, P),
    sig!("glGetAttribLocation", I32, U32, P),
    sig!("glGetBooleanv", V, U32, P),
    sig!("glGetError", U32),
    sig!("glGetFloatv", V, U32, P),
    sig!("glGetIntegerv", V, U32, P),
    sig!("glGetProgramInfoLog", V, U32, I32, P, P),
    sig!("glGetProgramiv", V, U32, U32, P),
    sig!("glGetShaderInfoLog", V, U32, I32, P, P),
    sig!("glGetShaderSource", V, U32, I32, P, P),
    sig!("glGetShaderiv", V, U32, U32, P),
    sig!("glGetString", P, U32),
    sig!("glGetUniformLocation", I32, U32, P),
    sig!("glIsEnabled", U8, U32),
    sig!("glLineWidth", V, F32),
    sig!("glLinkProgram", V, U32),
    sig!("glLoadIdentity", V),
    // Returns a pointer into the driver's own mapping of a buffer object,
    // which the guest then writes through — a host address in guest hands, which
    // identity mapping makes ordinary.
    sig!("glMapBuffer", P, U32, U32),
    sig!("glMatrixMode", V, U32),
    sig!("glPixelStorei", V, U32, I32),
    sig!("glPopMatrix", V),
    sig!("glPushMatrix", V),
    sig!("glReadPixels", V, I32, I32, I32, I32, U32, U32, P),
    sig!("glRenderbufferStorage", V, U32, U32, I32, I32),
    sig!("glRenderbufferStorageEXT", V, U32, U32, I32, I32),
    sig!("glScissor", V, I32, I32, I32, I32),
    sig!("glShaderSource", V, U32, I32, P, P),
    sig!("glStencilFunc", V, U32, I32, U32),
    sig!("glStencilMask", V, U32),
    sig!("glStencilOp", V, U32, U32, U32),
    sig!("glTexCoord2f", V, F32, F32),
    // Nine integer arguments, which is one more than either the guest's six
    // registers or the host's eight. See [`super::marshal`] for what carries the
    // ninth: the guest spilled three to its stack and the host takes one on its
    // own.
    sig!("glTexImage2D", V, U32, I32, I32, I32, I32, I32, U32, U32, P),
    // ### Sync objects
    //
    // A `GLsync` is an opaque pointer and a `GLuint64` timeout is a doubleword,
    // so the fence family crosses without ceremony. `glBlitFramebuffer` is
    // deliberately *not* here: it takes ten arguments, eight of them `GLint`,
    // and the two that spill occupy four bytes each on the host's stack —
    // Apple's arm64 ABI does not round a stack argument up to eight, so no
    // wider prototype can place them. It refuses by name instead.
    sig!("glClientWaitSync", U32, P, U32, U64),
    sig!("glDeleteSync", V, P),
    sig!("glFenceSync", P, U32, U32),
    sig!("glGetTexLevelParameteriv", V, U32, I32, U32, P),
    // ### The GL 3.2 core surface, which arrives by `dlsym` rather than by bind
    //
    // Almost nothing below is a static import. Unity's GL backend resolves each
    // entry point by name at run time, so [`tools/missing-signatures.sh`] — which
    // reads a Mach-O's undefined symbols — cannot see any of it, and the
    // discovery mechanism for this family is a program stopping on one call at a
    // time. That is a bad trade when the prototypes are as fixed as GL's, so
    // these are written from `<OpenGL/gl3.h>` in bulk instead.
    //
    // Types throughout: `GLenum`/`GLuint`/`GLbitfield` are `uint32_t`,
    // `GLint`/`GLsizei` are `int32_t`, `GLboolean` a byte, `GLintptr`/
    // `GLsizeiptr` doublewords, `GLsync` an opaque pointer.
    //
    // The five entry points here that overflow AAPCS64's eight integer
    // registers with a *narrow* argument — `glBlitFramebuffer`,
    // `glCopyTexSubImage3D`, `glTexImage3D`, `glTexSubImage3D` and
    // `glCompressedTexSubImage3D` — were refused until [`super::marshal`]
    // learned to declare an overflow tail at its own width. They are the reason
    // it learned: a Unity player uploads a texture array on its render thread,
    // and a refusal there is fatal rather than parked.
    sig!("glBeginQuery", V, U32, U32),
    sig!("glBindBufferBase", V, U32, U32, U32),
    sig!("glBindBufferRange", V, U32, U32, U32, I64, I64),
    sig!("glBindFragDataLocation", V, U32, U32, P),
    sig!("glBindSampler", V, U32, U32),
    sig!("glBindVertexArray", V, U32),
    sig!("glBlendEquationSeparate", V, U32, U32),
    sig!("glClearBufferfi", V, U32, I32, F32, I32),
    sig!("glClearBufferfv", V, U32, I32, P),
    sig!("glClearBufferiv", V, U32, I32, P),
    sig!("glClearBufferuiv", V, U32, I32, P),
    sig!("glClearDepthf", V, F32),
    sig!("glColorMaski", V, U32, U8, U8, U8, U8),
    sig!(
        "glCompressedTexSubImage2D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        U32,
        I32,
        P
    ),
    sig!("glCopyBufferSubData", V, U32, U32, I64, I64, I64),
    sig!(
        "glCopyTexSubImage2D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32
    ),
    sig!("glCullFace", V, U32),
    sig!("glDeleteQueries", V, I32, P),
    sig!("glDeleteSamplers", V, I32, P),
    sig!("glDeleteVertexArrays", V, I32, P),
    sig!("glDepthRangef", V, F32, F32),
    sig!("glDetachShader", V, U32, U32),
    sig!("glDisablei", V, U32, U32),
    sig!("glDrawArraysInstanced", V, U32, I32, I32, I32),
    sig!("glDrawElementsInstanced", V, U32, I32, U32, P, I32),
    sig!("glDrawRangeElements", V, U32, U32, U32, I32, U32, P),
    sig!("glEnablei", V, U32, U32),
    sig!("glEndQuery", V, U32),
    sig!("glFlushMappedBufferRange", V, U32, I64, I64),
    sig!("glFramebufferTextureLayer", V, U32, U32, U32, I32, I32),
    sig!("glGenQueries", V, I32, P),
    sig!("glGenSamplers", V, I32, P),
    sig!("glGenVertexArrays", V, I32, P),
    sig!("glGetActiveAttrib", V, U32, U32, I32, P, P, P, P),
    sig!("glGetActiveUniformBlockName", V, U32, U32, I32, P, P),
    sig!("glGetActiveUniformBlockiv", V, U32, U32, U32, P),
    sig!("glGetAttachedShaders", V, U32, I32, P, P),
    sig!("glGetBufferParameteriv", V, U32, U32, P),
    sig!("glGetFramebufferAttachmentParameteriv", V, U32, U32, U32, P),
    sig!("glGetInteger64v", V, U32, P),
    sig!("glGetInternalformativ", V, U32, U32, U32, I32, P),
    sig!("glGetMultisamplefv", V, U32, U32, P),
    sig!("glGetProgramBinary", V, U32, I32, P, P, P),
    sig!("glGetQueryObjectui64v", V, U32, U32, P),
    sig!("glGetQueryObjectuiv", V, U32, U32, P),
    sig!("glGetRenderbufferParameteriv", V, U32, U32, P),
    // `const GLubyte *glGetStringi(GLenum name, GLuint index)` — how a core
    // context enumerates its extensions, since `glGetString(GL_EXTENSIONS)` is
    // an error there. It is the first thing Unity's GL backend asks after the
    // device exists, which is why it is what the player stopped on.
    sig!("glGetStringi", P, U32, U32),
    sig!("glGetSynciv", V, P, U32, I32, P, P),
    sig!("glGetTexParameterfv", V, U32, U32, P),
    sig!("glGetTexParameteriv", V, U32, U32, P),
    sig!("glGetUniformBlockIndex", U32, U32, P),
    sig!("glGetVertexAttribiv", V, U32, U32, P),
    sig!("glHint", V, U32, U32),
    sig!("glIsBuffer", U8, U32),
    sig!("glIsEnabledi", U8, U32, U32),
    sig!("glIsFramebuffer", U8, U32),
    sig!("glIsProgram", U8, U32),
    sig!("glIsRenderbuffer", U8, U32),
    sig!("glIsShader", U8, U32),
    sig!("glIsTexture", U8, U32),
    sig!("glIsQuery", U8, U32),
    sig!("glIsSampler", U8, U32),
    sig!("glIsSync", U8, P),
    sig!("glIsVertexArray", U8, U32),
    // ### GL 4.1 and the extensions this driver advertises
    //
    // Added from the same header for the same reason as the block above: these
    // arrive by `dlsym`, so waiting for each to stop a run costs a build and a
    // launch per entry point. The extension names come from what the renderer
    // reported at start-up — separate shader objects, viewport arrays,
    // transform feedback, tessellation, timer queries, per-buffer blending and
    // Apple's own debug and texture-range calls — rather than from a guess
    // about what a game might use.
    sig!("glBeginTransformFeedback", V, U32),
    sig!("glBindFragDataLocationIndexed", V, U32, U32, U32, P),
    sig!("glBindProgramPipeline", V, U32),
    sig!("glBindTransformFeedback", V, U32, U32),
    sig!("glBlendEquationSeparatei", V, U32, U32, U32),
    sig!("glBlendEquationi", V, U32, U32),
    sig!("glBlendFuncSeparatei", V, U32, U32, U32, U32, U32),
    sig!("glBlendFunci", V, U32, U32, U32),
    sig!("glCreateShaderProgramv", U32, U32, I32, P),
    sig!("glDebugMessageControl", V, U32, U32, U32, I32, P, U8),
    sig!("glDebugMessageInsert", V, U32, U32, U32, U32, I32, P),
    sig!("glDeleteProgramPipelines", V, I32, P),
    sig!("glDeleteTransformFeedbacks", V, I32, P),
    sig!("glDrawArraysIndirect", V, U32, P),
    sig!("glDrawElementsIndirect", V, U32, U32, P),
    sig!("glDrawTransformFeedback", V, U32, U32),
    sig!("glEndTransformFeedback", V),
    sig!("glFlushRenderAPPLE", V),
    sig!("glGenProgramPipelines", V, I32, P),
    sig!("glGenTransformFeedbacks", V, I32, P),
    sig!("glGetActiveUniformsiv", V, U32, I32, P, U32, P),
    sig!("glGetDoublev", V, U32, P),
    sig!("glGetFragDataIndex", I32, U32, P),
    sig!("glGetProgramPipelineInfoLog", V, U32, I32, P, P),
    sig!("glGetProgramPipelineiv", V, U32, U32, P),
    sig!("glGetQueryObjecti64v", V, U32, U32, P),
    sig!("glGetShaderPrecisionFormat", V, U32, U32, P, P),
    sig!("glGetSubroutineIndex", U32, U32, U32, P),
    sig!("glGetUniformIndices", V, U32, I32, P, P),
    sig!("glInsertEventMarkerEXT", V, I32, P),
    sig!("glLabelObjectEXT", V, U32, U32, I32, P),
    sig!("glGetObjectLabelEXT", V, U32, U32, I32, P, P),
    // ### The draw calls, and the uniform setters beside them
    //
    // Added in bulk for a reason the render thread makes sharp: a refusal there
    // is the *outermost* guest frame on that thread, so it exits the process
    // rather than parking — one launch per missing entry point, and a launch is
    // ninety seconds. `glDrawElementsBaseVertex` is what a Unity renderer
    // issues for a mesh with a vertex offset, which is most of them.
    sig!("glBlendColor", V, F32, F32, F32, F32),
    sig!("glDrawElementsBaseVertex", V, U32, I32, U32, P, I32),
    // ### The `ARB` and `APPLE` spellings, and the fixed-function leftovers
    //
    // Unreal Engine 3's OpenGL RHI predates these being core, so it resolves the
    // *extension* names — same functions, different symbols, and a table that
    // has only `glDrawArraysInstanced` does not answer `…InstancedARB`.
    //
    // `glVertexPointer`, `glTexCoordPointer` and the client-state pair are the
    // compatibility-profile fixed-function path, which the engine still uses for
    // its one-off screen quads. `glTexEnvf`'s third argument is a `GLfloat` and
    // arrives in `xmm0` while the two before it are integers, which is the one
    // entry here a wrong guess would place in the wrong register file.
    sig!("glDrawArraysInstancedARB", V, U32, I32, I32, I32),
    sig!("glDrawElementsInstancedARB", V, U32, I32, U32, P, I32),
    sig!("glVertexAttribDivisorARB", V, U32, U32),
    // `void glBufferParameteriAPPLE(GLenum target, GLenum pname, GLint param)`,
    // which is how `APPLE_flush_buffer_range` turns off the driver's implicit
    // flush, and `glFlushMappedBufferRangeAPPLE(target, offset, size)`, which is
    // the explicit one — an offset and a size, both pointer-width.
    sig!("glBufferParameteriAPPLE", V, U32, U32, I32),
    sig!("glFlushMappedBufferRangeAPPLE", V, U32, I64, I64),
    sig!("glGetQueryObjectiv", V, U32, U32, P),
    sig!("glEnableClientState", V, U32),
    sig!("glDisableClientState", V, U32),
    sig!("glVertexPointer", V, I32, U32, I32, P),
    sig!("glTexCoordPointer", V, I32, U32, I32, P),
    sig!("glTexEnvf", V, U32, U32, F32),
    sig!(
        "glDrawElementsInstancedBaseVertex",
        V,
        U32,
        I32,
        U32,
        P,
        I32,
        I32
    ),
    sig!(
        "glDrawRangeElementsBaseVertex",
        V,
        U32,
        U32,
        U32,
        I32,
        U32,
        P,
        I32
    ),
    sig!("glGetBooleani_v", V, U32, U32, P),
    sig!("glGetFloati_v", V, U32, U32, P),
    sig!("glGetIntegeri_v", V, U32, U32, P),
    sig!("glGetUniformfv", V, U32, I32, P),
    sig!("glGetUniformiv", V, U32, I32, P),
    sig!("glLogicOp", V, U32),
    sig!("glMultiDrawArrays", V, U32, P, P, I32),
    sig!("glMultiDrawElements", V, U32, P, U32, P, I32),
    sig!("glMultiDrawElementsBaseVertex", V, U32, P, U32, P, I32, P),
    sig!("glPixelStoref", V, U32, F32),
    sig!("glTexParameterfv", V, U32, U32, P),
    sig!("glTexParameteriv", V, U32, U32, P),
    sig!("glUniform1ui", V, I32, U32),
    sig!("glUniform1uiv", V, I32, I32, P),
    sig!("glUniform2ui", V, I32, U32, U32),
    sig!("glUniform2uiv", V, I32, I32, P),
    sig!("glUniform3ui", V, I32, U32, U32, U32),
    sig!("glUniform3uiv", V, I32, I32, P),
    sig!("glUniform4ui", V, I32, U32, U32, U32, U32),
    sig!("glUniform4uiv", V, I32, I32, P),
    sig!("glUniformMatrix2x3fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix2x4fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix3x2fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix3x4fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix4x2fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix4x3fv", V, I32, I32, U8, P),
    sig!("glVertexAttrib1f", V, U32, F32),
    sig!("glVertexAttrib2f", V, U32, F32, F32),
    sig!("glVertexAttrib3f", V, U32, F32, F32, F32),
    sig!("glVertexAttrib4f", V, U32, F32, F32, F32, F32),
    sig!("glVertexAttrib4fv", V, U32, P),
    sig!("glVertexAttribI4i", V, U32, I32, I32, I32, I32),
    sig!("glVertexAttribI4ui", V, U32, U32, U32, U32, U32),
    sig!("glMinSampleShading", V, F32),
    sig!("glPatchParameterfv", V, U32, P),
    sig!("glPatchParameteri", V, U32, I32),
    sig!("glPopGroupMarkerEXT", V),
    sig!("glProgramUniform1f", V, U32, I32, F32),
    sig!("glProgramUniform1i", V, U32, I32, I32),
    sig!("glProgramUniform4fv", V, U32, I32, I32, P),
    sig!("glProgramUniformMatrix4fv", V, U32, I32, I32, U8, P),
    sig!("glPushGroupMarkerEXT", V, I32, P),
    sig!("glQueryCounter", V, U32, U32),
    sig!("glScissorArrayv", V, U32, I32, P),
    sig!("glTextureBarrierNV", V),
    sig!("glTextureRangeAPPLE", V, U32, I32, P),
    sig!("glTransformFeedbackVaryings", V, U32, I32, P, U32),
    sig!("glUniformSubroutinesuiv", V, U32, I32, P),
    sig!("glUseProgramStages", V, U32, U32, U32),
    sig!("glVertexAttribLPointer", V, U32, I32, U32, I32, P),
    sig!("glViewportArrayv", V, U32, I32, P),
    sig!("glMapBufferRange", P, U32, I64, I64, U32),
    sig!("glObjectLabel", V, U32, U32, I32, P),
    sig!("glPointSize", V, F32),
    sig!("glPolygonMode", V, U32, U32),
    sig!("glPolygonOffset", V, F32, F32),
    sig!("glPopDebugGroup", V),
    sig!("glPrimitiveRestartIndex", V, U32),
    sig!("glProgramBinary", V, U32, U32, P, I32),
    sig!("glProgramParameteri", V, U32, U32, I32),
    sig!("glProvokingVertex", V, U32),
    sig!("glPushDebugGroup", V, U32, U32, I32, P),
    sig!("glReadBuffer", V, U32),
    sig!(
        "glRenderbufferStorageMultisample",
        V,
        U32,
        I32,
        U32,
        I32,
        I32
    ),
    sig!("glSampleCoverage", V, F32, U8),
    sig!("glSampleMaski", V, U32, U32),
    sig!("glSamplerParameterf", V, U32, U32, F32),
    sig!("glSamplerParameterfv", V, U32, U32, P),
    sig!("glSamplerParameteri", V, U32, U32, I32),
    sig!("glStencilFuncSeparate", V, U32, U32, I32, U32),
    sig!("glStencilMaskSeparate", V, U32, U32),
    sig!("glStencilOpSeparate", V, U32, U32, U32, U32),
    sig!("glTexBuffer", V, U32, U32, U32),
    sig!("glTexImage2DMultisample", V, U32, I32, U32, I32, I32, U8),
    sig!("glTexStorage2D", V, U32, I32, U32, I32, I32),
    sig!("glTexStorage3D", V, U32, I32, U32, I32, I32, I32),
    sig!(
        "glTexImage3D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        U32,
        U32,
        P
    ),
    sig!(
        "glTexSubImage3D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        U32,
        U32,
        P
    ),
    sig!(
        "glCompressedTexSubImage3D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        U32,
        I32,
        P
    ),
    sig!(
        "glCopyTexSubImage3D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32
    ),
    sig!(
        "glBlitFramebuffer",
        V,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        I32,
        U32,
        U32
    ),
    sig!("glTexStorage1D", V, U32, I32, U32, I32),
    sig!(
        "glCompressedTexImage3D",
        V,
        U32,
        I32,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        P
    ),
    sig!("glFramebufferTexture3D", V, U32, U32, U32, U32, I32, I32),
    sig!("glGetCompressedTexImage", V, U32, I32, P),
    sig!("glGetTexImage", V, U32, I32, U32, U32, P),
    sig!("glUniformBlockBinding", V, U32, U32, U32),
    sig!("glValidateProgram", V, U32),
    sig!("glVertexAttribDivisor", V, U32, U32),
    sig!("glVertexAttribIPointer", V, U32, I32, U32, I32, P),
    sig!("glWaitSync", V, P, U32, U64),
    sig!("glTexParameterf", V, U32, U32, F32),
    sig!("glTexParameteri", V, U32, U32, I32),
    sig!(
        "glTexSubImage2D",
        V,
        U32,
        I32,
        I32,
        I32,
        I32,
        I32,
        U32,
        U32,
        P
    ),
    sig!("glUniform1f", V, I32, F32),
    sig!("glUniform1fv", V, I32, I32, P),
    sig!("glUniform1i", V, I32, I32),
    sig!("glUniform1iv", V, I32, I32, P),
    sig!("glUniform2f", V, I32, F32, F32),
    sig!("glUniform2fv", V, I32, I32, P),
    sig!("glUniform2i", V, I32, I32, I32),
    sig!("glUniform2iv", V, I32, I32, P),
    sig!("glUniform3f", V, I32, F32, F32, F32),
    sig!("glUniform3fv", V, I32, I32, P),
    sig!("glUniform3i", V, I32, I32, I32, I32),
    sig!("glUniform3iv", V, I32, I32, P),
    sig!("glUniform4f", V, I32, F32, F32, F32, F32),
    sig!("glUniform4fv", V, I32, I32, P),
    sig!("glUniform4i", V, I32, I32, I32, I32, I32),
    sig!("glUniform4iv", V, I32, I32, P),
    sig!("glUniformMatrix2fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix3fv", V, I32, I32, U8, P),
    sig!("glUniformMatrix4fv", V, I32, I32, U8, P),
    sig!("glUnmapBuffer", U8, U32),
    sig!("glUseProgram", V, U32),
    sig!("glVertex2f", V, F32, F32),
    sig!("glVertexAttribPointer", V, U32, I32, U32, U8, I32, P),
    sig!("glViewport", V, I32, I32, I32, I32),
    // `glob`'s error callback is guest code called once per unreadable
    // directory. The import is spelled `_glob$INODE64`, and the `$`-suffix
    // fallback is what finds this entry.
    cbsig!([2], "glob", I32, P, I32, P, P),
    sig!("gluOrtho2D", V, F64, F64, F64, F64),
    sig!("gmtime", P, P),
    sig!("gmtime_r", P, P, P),
    sig!("group_from_gid", P, U32, I32),
    // `strhash.h`'s table, still exported by libSystem. The replace function is
    // guest code, called when a key is already present.
    sig!("hash_create", P, I32),
    cbsig!([3], "hash_search", P, P, P, P, P),
    sig!("herror", V, P),
    // Two `host_*` calls Valve's tier0 makes to size physical memory. The host
    // port they take is this process's, for the reason `mach_host_self` in
    // `shims::runtime` gives: there is one task here, and one host.
    sig!("host_get_clock_service", I32, U32, U32, P),
    // ## The Mach kernel interface, and why it needs no translation
    //
    // Every entry below is a MIG routine: the arguments are `mach_port_t`,
    // `integer_t` and friends — fixed-width 32-bit types by definition, not
    // `long` — and the structures behind the pointers are arrays of those. There
    // is nothing here whose layout can differ between the two architectures, and
    // there is nothing to translate besides: **the guest's task is this task**,
    // and the guest's threads are this process's threads, so a port name means
    // the same thing on both sides of the seam. See the `mach_task_self_` note
    // in `shims::runtime`.
    //
    // Terraria's embedded Mono is what reaches them, and reaches nearly all of
    // them: Mach semaphores for its own synchronisation, the clock services for
    // timing, and the task/thread introspection its garbage collector suspends
    // the world with.
    sig!("host_page_size", I32, U32, P),
    sig!("host_statistics", I32, U32, I32, P, P),
    // The 64-bit flavours, which differ from the pair above only in that the
    // structure they fill has 64-bit counters. Same five arguments, same
    // meaning: a host port, a flavour, the caller's buffer, and a count that is
    // read for the buffer's size and written back with what was filled. Roblox
    // asks for `HOST_VM_INFO64` to decide how much memory it may use.
    sig!("host_statistics64", I32, U32, I32, P, P),
    sig!("host_info", I32, U32, I32, P, P),
    sig!("hypot", F64, F64, F64),
    sig!("hypotf", F32, F32, F32),
    sig!("iconv", U64, P, P, P, P, P),
    sig!("iconv_close", I32, P),
    sig!("iconv_open", P, P, P),
    sig!("if_indextoname", P, U32, P),
    sig!("if_nametoindex", U32, P),
    sig!("ilogb", I32, F64),
    sig!("ilogbf", I32, F32),
    sig!("inet_addr", U32, P),
    sig!("inet_aton", I32, P, P),
    sig!("inet_ntop", P, I32, P, P, U32),
    sig!("inet_pton", I32, I32, P, P),
    sig!("inflate", I32, P, I32),
    sig!("inflateEnd", I32, P),
    sig!("inflateInit2_", I32, P, I32, P, I32),
    sig!("inflateInit_", I32, P, P, I32),
    sig!("inflateReset", I32, P),
    sig!("initgroups", I32, P, I32),
    vsig!("ioctl", I32, I32, U64),
    sig!("isalnum", I32, I32),
    sig!("isalpha", I32, I32),
    sig!("isascii", I32, I32),
    sig!("isatty", I32, I32),
    sig!("isdigit", I32, I32),
    sig!("islower", I32, I32),
    sig!("isprint", I32, I32),
    sig!("ispunct", I32, I32),
    sig!("isspace", I32, I32),
    sig!("isupper", I32, I32),
    sig!("isxdigit", I32, I32),
    // BSD event notification, which is how `steamclient.dylib` waits on its
    // sockets and its pipe to the client. Passed straight through, and the
    // reason is a check rather than an assumption: `struct kevent` is 32 bytes
    // with `ident` at 0, `filter` at 8, `flags` at 10, `fflags` at 12, `data` at
    // 16 and `udata` at 24 on **both** architectures, as are `kevent64_s` and
    // `timespec`. That question — is the structure behind this pointer the same
    // on both sides? — is the one `std::string` answered "no" to, and it is now
    // asked of every pointer argument before one is described. See
    // [`crate::shims::cxxstring`].
    //
    // `udata` is opaque to the kernel and comes back to the guest untouched, so
    // it is not the [callback-inside-a-structure] hazard `zlib` and `audiounit`
    // are: nothing ever calls it.
    //
    // [callback-inside-a-structure]: crate::shims::zlib
    // (`kill` sorts between the two and is unrelated.)
    sig!("kevent", I32, I32, P, I32, P, I32, P),
    sig!("kill", I32, I32, I32),
    sig!("kqueue", I32),
    sig!("labs", I64, I64),
    sig!("ldexp", F64, F64, I32),
    sig!("ldexpf", F32, F32, I32),
    sig!("listen", I32, I32, I32),
    sig!("listxattr", I64, P, P, U64, I32),
    // `struct lconv *localeconv(void)` — the current locale's numeric
    // punctuation, which C++'s `num_put` reads on the way to formatting a
    // number. A pointer to storage the host owns.
    sig!("localeconv", P),
    sig!("localeconv_l", P, P),
    sig!("localtime", P, P),
    // `localtime_r` and `gmtime_r` above: the reentrant forms, which write into
    // a `struct tm` the *caller* owns. That is 56 bytes with every field at the
    // same offset on both architectures — `tm_gmtoff` at 40, `tm_zone` at 48 —
    // so the host fills the guest's structure in place, the way it fills a
    // `pthread_mutex_t`. Checked against the compiler, which is now the rule for
    // every pointer argument; see the note on `kevent`.
    sig!("localtime_r", P, P, P),
    sig!("lockf", I32, I32, I32, I64),
    // `const char *locale_charset(void)` — libiconv's, not libc's, and the way
    // Mono asks what encoding the current locale uses before it converts a
    // managed string. It hands back a pointer into a static table the host
    // owns, which the guest only reads.
    sig!("locale_charset", P),
    sig!("log", F64, F64),
    sig!("log10", F64, F64),
    sig!("log10f", F32, F32),
    sig!("log1p", F64, F64),
    sig!("log1pf", F32, F32),
    sig!("log2", F64, F64),
    sig!("log2f", F32, F32),
    sig!("logb", F64, F64),
    sig!("logbf", F32, F32),
    sig!("logf", F32, F32),
    // The integer-returning rounding family. `long` and `long long` are both
    // eight bytes on LP64, so `lrint`/`llrint` and `lround`/`llround` agree on
    // both architectures and differ only in name.
    sig!("llrint", I64, F64),
    sig!("llrintf", I64, F32),
    sig!("llround", I64, F64),
    sig!("llroundf", I64, F32),
    // `longjmp` and `setjmp` are *shimmed*, not forwarded: the host's would save
    // and restore arm64 registers and the JIT's own stack frame, which is the
    // one kind of wrongness that does not fault. The signature is here anyway
    // because it is what stops them being promoted to an inlined fast import and
    // what names them at an exit — see [`crate::jmpbuf`].
    sig!("longjmp", V, P, I32),
    sig!("lrint", I64, F64),
    sig!("lrintf", I64, F32),
    sig!("lround", I64, F64),
    sig!("lroundf", I64, F32),
    sig!("lseek", I64, I32, I64, I32),
    sig!("lstat", I32, P, P),
    sig!("lutimes", I32, P, P),
    sig!("mach_absolute_time", U64),
    sig!("mach_host_self", U32),
    // The three Mach IPC calls `libsteam_api.dylib` makes to talk to the Steam
    // client. They need no translation at all: the guest's task is this
    // process's task (see the `_mach_task_self_` note in `shims::runtime`), and
    // the message buffer is guest memory the identity mapping makes directly
    // readable by the kernel. Port names are plain 32-bit integers on both
    // architectures.
    sig!("mach_msg", I32, P, I32, U32, U32, U32, U32, U32),
    sig!("mach_msg_destroy", V, P),
    sig!("mach_port_allocate", I32, U32, U32, P),
    sig!("mach_port_deallocate", I32, U32, U32),
    // `kern_return_t mach_port_extract_right(ipc_space_t, mach_port_name_t,
    // mach_msg_type_name_t, mach_port_t *poly, mach_msg_type_name_t *polyPoly)`
    // — the two out-parameters are a 32-bit port name and a 32-bit type name,
    // written through pointers the guest owns. Every scalar here is a Mach name,
    // which is a `natural_t` on both architectures, and the task the guest names
    // is this task.
    sig!("mach_port_extract_right", I32, U32, U32, U32, P, P),
    sig!("mach_port_insert_right", I32, U32, U32, U32, U32),
    sig!("mach_port_mod_refs", I32, U32, U32, U32, I32),
    sig!("mach_port_type", I32, U32, U32, P),
    // ### Mach's own memory calls
    //
    // Passthrough, and unlike `mmap` they need no shim: none of these carries a
    // protection argument, so none of them can hand the guest a page this CPU
    // would be asked to execute. `mach_vm_map` is the exception and is
    // deliberately absent — it takes eleven arguments *and* a protection pair,
    // so it is both unmarshallable and the one entry point here that would need
    // [`crate::shims::vmmap`]'s treatment.
    //
    // The page-size question is unchanged and unanswered: these present the
    // host's 16 KiB, and a guest doing arithmetic against `vm_page_size` is
    // still the open item the roadmap records.
    sig!("mach_make_memory_entry_64", I32, U32, P, U64, I32, P, U32),
    sig!("mach_vm_allocate", I32, U32, P, U64, I32),
    sig!("mach_vm_deallocate", I32, U32, U64, U64),
    sig!("mach_vm_read_overwrite", I32, U32, U64, U64, U64, P),
    sig!("vm_allocate", I32, U32, P, U64, I32),
    sig!("vm_deallocate", I32, U32, U64, U64),
    sig!("mach_thread_self", U32),
    sig!("mach_timebase_info", I32, P),
    sig!("madvise", U64, P, U64, I32),
    sig!("malloc", P, U64),
    sig!("malloc_size", U64, P),
    sig!("malloc_zone_statistics", V, P, P),
    sig!("mbrlen_l", U64, P, U64, P, P),
    sig!("mbrtowc", U64, P, P, U64, P),
    sig!("mbrtowc_l", U64, P, P, U64, P, P),
    sig!("mbsnrtowcs_l", U64, P, P, U64, U64, P, P),
    sig!("mbsrtowcs", U64, P, P, U64, P),
    sig!("mbsrtowcs_l", U64, P, P, U64, P, P),
    sig!("mbstowcs", U64, P, P, U64),
    sig!("mbtowc", I32, P, P, U64),
    // `int mbtowc_l(wchar_t *, const char *, size_t, locale_t)`.
    //
    // This entry used to read `sig!("mbtowc_l", P, P, U64, U64)` — a pointer
    // return and *three* arguments, which is `mbtowc`'s shape with a `locale_t`
    // pasted over the return type. The `locale_t` was therefore never passed,
    // and the host read whatever was in `rcx`. It sat here harmlessly for as
    // long as libc++ was the host's, because nothing in the guest ever called
    // it; the first thing to do so was the *vendored* libc++
    // ([`crate::cxxlib`]), whose locale set-up died writing through the garbage
    // pointer into libsystem_c's own read-only data.
    //
    // Worth the paragraph because of what it says about the table rather than
    // about this function: an entry that is never exercised is not evidence of
    // anything, and there is no test that can find a wrong signature for a
    // symbol no program calls.
    sig!("mbtowc_l", I32, P, P, U64, P),
    sig!("memchr", P, P, I32, U64),
    sig!("memcmp", I32, P, P, U64),
    sig!("memcpy", P, P, P, U64),
    sig!("memmove", P, P, P, U64),
    sig!("memset", P, P, I32, U64),
    // `errno_t memset_s(void *s, rsize_t smax, int c, rsize_t n)` — Annex K's
    // memset, and the reason a program uses it is that the compiler may not
    // optimize it away. It answers an `errno_t`, which is an `int`, rather than
    // the destination `memset` returns.
    sig!("memset_s", I32, P, U64, I32, U64),
    sig!("memset_pattern16", V, P, P, U64),
    // Answered honestly rather than usefully: `mincore` reports *residency* and
    // succeeds for an address with nothing mapped at it, which is why
    // [`crate::vm::GuestView`] probes with `msync` instead. Passing it through
    // gives the guest exactly what it would get natively.
    sig!("mincore", I32, P, U64, P),
    sig!("mkdir", I32, P, U32),
    sig!("mkdtemp", P, P),
    sig!("mknod", I32, P, U32, I32),
    sig!("mkstemp", I32, P),
    sig!("mktemp", P, P),
    sig!("mktime", I64, P),
    sig!("mlock", I32, P, U64),
    // ## The memory-mapping group, and the page size
    //
    // Passthrough, with one caveat this runtime cannot hide: the guest was
    // compiled expecting 4 KiB pages and Apple Silicon enforces 16 KiB, so a
    // guest rounding a length or an address to *its* page size gets a request
    // the kernel rounds again. Every use in a real program is either a whole
    // allocation or a `mprotect` of a region the same program `mmap`ed, both of
    // which survive the coarser granularity. A guest doing page arithmetic
    // against `getpagesize` would not — see M7 in the roadmap.
    sig!("mmap", P, P, U64, I32, I32, I32, I64),
    sig!("modf", F64, F64, P),
    sig!("modff", F32, F32, P),
    sig!("mprotect", I32, P, U64, I32),
    sig!("msync", I32, P, U64, I32),
    sig!("munlock", I32, P, U64),
    sig!("munmap", I32, P, U64),
    sig!("nanosleep", I32, P, P),
    sig!("nearbyintf", F32, F32),
    sig!("newlocale", P, I32, P, P),
    sig!("nextafter", F64, F64, F64),
    sig!("nl_langinfo", P, I32),
    sig!("nl_langinfo_l", P, I32, P),
    // Variadic in their declarations and *fixed* in fact: the ellipsis is at
    // most one `mode_t`, and `O_CREAT` in the flag word is what says whether it
    // is there. So they are written out with the optional argument always
    // present — the guest's own call placed a mode in that register or it did
    // not, and which is which is decided where the argument is consumed rather
    // than where it is passed.
    //
    // This is only sound because these names resolve to **this runtime's**
    // wrappers rather than the host's `open`: the host's really are variadic,
    // and Apple's arm64 ABI puts a variadic argument on the stack, so a fixed
    // signature would leave the mode in a register no `va_arg` reads. The
    // wrappers are ordinary C functions that take the mode as a parameter and
    // apply the `O_CREAT` rule themselves. See [`crate::shims::openat`].
    sig!("open", I32, P, I32, U32),
    sig!("openat", I32, I32, P, I32, U32),
    sig!("opendir", P, P),
    sig!("openlog", V, P, I32, I32),
    sig!("pathconf", I64, P, I32),
    sig!("pclose", I32, P),
    sig!("perror", V, P),
    // `int poll(struct pollfd *, nfds_t, int)`. `nfds_t` is `unsigned int` on
    // Darwin, and `struct pollfd` is eight bytes laid out identically on both
    // architectures — checked alongside `kevent` above, for the same reason.
    sig!("pipe", I32, P),
    sig!("poll", I32, P, U32, I32),
    sig!("popen", P, P, P),
    sig!("posix_madvise", I32, P, U64, I32),
    sig!("posix_memalign", I32, P, U64, U64),
    // `posix_spawn` and its two attribute objects, which is how the Steam
    // client starts a helper process. Every argument is a pointer to guest
    // memory the host reads in place; the child is a *host* process running a
    // host binary, so nothing about it comes back through this runtime.
    sig!("posix_spawn", I32, P, P, P, P, P, P),
    // `posix_spawnp` differs from `posix_spawn` only in searching `PATH`, so it
    // takes the same six arguments. Crashpad — which is Roblox's crash reporter
    // and starts before anything else — spawns its handler through this one.
    sig!("posix_spawnp", I32, P, P, P, P, P, P),
    sig!("posix_spawn_file_actions_addchdir_np", I32, P, P),
    // `int posix_spawn_file_actions_addinherit_np(posix_spawn_file_actions_t *,
    // int)` — "keep this descriptor open across the spawn", which is how a game
    // hands its crash handler the pipe it will report on. Roblox spawns
    // `RobloxCrashHandler` before anything else happens.
    sig!("posix_spawn_file_actions_addinherit_np", I32, P, I32),
    sig!("posix_spawn_file_actions_adddup2", I32, P, I32, I32),
    sig!("posix_spawn_file_actions_addclose", I32, P, I32),
    sig!("posix_spawn_file_actions_addopen", I32, P, I32, P, I32, U16),
    sig!("posix_spawn_file_actions_destroy", I32, P),
    sig!("posix_spawn_file_actions_init", I32, P),
    sig!("posix_spawnattr_destroy", I32, P),
    sig!("posix_spawnattr_init", I32, P),
    sig!("posix_spawnattr_setbinpref_np", I32, P, U64, P, P),
    // `short flags`, which SysV passes in the low half of a register and
    // AAPCS64 reads back out of one: narrower than the slot on both sides.
    sig!("posix_spawnattr_setflags", I32, P, U32),
    sig!("posix_spawnattr_setsigmask", I32, P, P),
    sig!("pow", F64, F64, F64),
    sig!("powf", F32, F32, F32),
    sig!("pread", I64, I32, P, U64, I64),
    vsig!("printf", I32, P),
    sig!("proc_name", I32, I32, P, U32),
    // `int proc_pidinfo(int pid, int flavour, uint64_t arg, void *buffer,
    // int buffersize)` — the general form of `proc_pidpath` above, and how a
    // program asks the kernel about its own task: resident size, thread counts,
    // open descriptors. Every flavour writes a fixed structure into a buffer the
    // guest owns, and the two architectures agree on all of them for the reason
    // the pthread family does: they are kernel ABI, not compiler ABI.
    sig!("proc_pidinfo", I32, I32, I32, U64, P, I32),
    sig!("proc_pidpath", I32, I32, P, U32),
    sig!("psignal", V, U32, P),
    cbsig!([0, 1, 2], "pthread_atfork", I32, P, P, P),
    // ## The pthread family
    //
    // Passthrough is correct here for a reason worth stating rather than
    // assuming, because it is the whole basis of M4: every opaque pthread type
    // is **byte-identical** on x86_64 and arm64 macOS — `pthread_mutex_t` is 64
    // bytes on both, `pthread_cond_t` 48, `pthread_rwlock_t` 200,
    // `pthread_attr_t` 64, `pthread_t` a pointer. The guest allocates one on its
    // own stack and the host's libpthread operates on it in place, with no
    // translation at all. (`pthread_types_agree_across_architectures` checks
    // this rather than trusting it.)
    //
    // What is *not* passthrough is `pthread_create`. The start routine is guest
    // code and goes out as a thunk like any other callback — the difference
    // being that the host calls it on a brand-new host thread, which is why M4
    // needed per-thread guest state before this entry could exist. And the
    // attribute set is the one place in the family where "the two layouts agree,
    // so operate in place" stops being enough: a guest thread has two stacks,
    // the host's for the JIT's frames and the guest's for its own, and every
    // stack attribute here is about the second while passthrough applied it to
    // the first. The entry stays for its signature and its callback index; the
    // call is serviced by [`crate::shims::pthread`]. See [`crate::thread`].
    sig!("pthread_attr_destroy", I32, P),
    // Every getter in this family is `(const pthread_attr_t *, T *)` and every
    // setter takes its value directly, which is why the two halves do not read
    // symmetrically: `setdetachstate` takes an `int`, `getdetachstate` an
    // `int *`.
    sig!("pthread_attr_getdetachstate", I32, P, P),
    sig!("pthread_attr_getguardsize", I32, P, P),
    sig!("pthread_attr_getinheritsched", I32, P, P),
    sig!("pthread_attr_getschedparam", I32, P, P),
    sig!("pthread_attr_getschedpolicy", I32, P, P),
    sig!("pthread_attr_getscope", I32, P, P),
    sig!("pthread_attr_getstack", I32, P, P, P),
    sig!("pthread_attr_getstackaddr", I32, P, P),
    sig!("pthread_attr_getstacksize", I32, P, P),
    sig!("pthread_attr_init", I32, P),
    sig!("pthread_attr_setdetachstate", I32, P, I32),
    sig!("pthread_attr_setguardsize", I32, P, U64),
    sig!("pthread_attr_setinheritsched", I32, P, I32),
    sig!("pthread_attr_setscope", I32, P, I32),
    sig!("pthread_attr_setstack", I32, P, P, U64),
    sig!("pthread_attr_setstackaddr", I32, P, P),
    // `struct sched_param` is `int sched_priority` and an opaque tail, and the
    // guest passes it by address — the same in-place arrangement every other
    // `pthread` type here rests on.
    // The QoS surface. `qos_class_t` is an `unsigned int` enum and the relative
    // priority an `int`, so every one of these is scalar. They pass through
    // because the class a guest asks for is a real scheduling request about a
    // real host thread — a guest that names its audio thread `USER_INTERACTIVE`
    // means it.
    sig!("pthread_attr_set_qos_class_np", I32, P, U32, I32),
    sig!("pthread_attr_get_qos_class_np", I32, P, P, P),
    sig!("pthread_attr_setschedparam", I32, P, P),
    sig!("pthread_attr_setschedpolicy", I32, P, I32),
    sig!("pthread_attr_setstacksize", I32, P, U64),
    sig!("pthread_cond_broadcast", I32, P),
    sig!("pthread_cond_destroy", I32, P),
    sig!("pthread_cond_init", I32, P, P),
    sig!("pthread_cond_signal", I32, P),
    sig!("pthread_cond_timedwait", I32, P, P, P),
    sig!("pthread_cond_timedwait_relative_np", I32, P, P, P),
    sig!("pthread_cond_wait", I32, P, P),
    sig!("pthread_condattr_destroy", I32, P),
    sig!("pthread_condattr_init", I32, P),
    // `int pthread_cpu_number_np(size_t *)` — how mimalloc, which Roblox links
    // as its allocator, picks a per-CPU heap. The out-parameter is a `size_t`
    // the guest owns and the host writes an index into; the count it is
    // compared against is the host's core count, which is the honest answer.
    sig!("pthread_cpu_number_np", I32, P),
    cbsig!([2], "pthread_create", I32, P, P, P, P),
    sig!("pthread_detach", I32, U64),
    sig!("pthread_equal", I32, U64, U64),
    // Terminates the calling host thread from inside our thunk: the Rust frames
    // between here and the JIT entry are abandoned rather than unwound. That is
    // what Darwin's own `pthread_exit` does to any C frame, and it is safe here
    // only because the runtime never holds a lock across guest code. Thread-local
    // destructors still run, so this thread's guest stack and TLS blocks are
    // still released.
    sig!("pthread_exit", V, P),
    sig!("pthread_get_qos_class_np", I32, U64, P, P),
    sig!("pthread_get_stackaddr_np", P, U64),
    sig!("pthread_get_stacksize_np", U64, U64),
    sig!("pthread_getname_np", I32, U64, P, U64),
    sig!("pthread_getschedparam", I32, U64, P, P),
    sig!("pthread_getspecific", P, U64),
    sig!("pthread_jit_write_protect_np", I32, I32),
    sig!("pthread_join", I32, U64, P),
    // The destructor is guest code, called by libpthread on thread teardown.
    cbsig!([1], "pthread_key_create", I32, P, P),
    sig!("pthread_key_delete", I32, U64),
    // Signals a *host* thread, which is what a guest thread is here, so the
    // disposition it reaches is the one `shims::signal` installed.
    sig!("pthread_kill", I32, U64, I32),
    // The thread's Mach port name. One task, one port namespace — see
    // `mach_task_self_` in `shims::runtime`.
    sig!("pthread_mach_thread_np", U32, U64),
    sig!("pthread_main_np", I32),
    sig!("pthread_mutex_destroy", I32, P),
    sig!("pthread_mutex_init", I32, P, P),
    sig!("pthread_mutex_lock", I32, P),
    sig!("pthread_mutex_trylock", I32, P),
    sig!("pthread_mutex_unlock", I32, P),
    sig!("pthread_mutexattr_destroy", I32, P),
    sig!("pthread_mutexattr_init", I32, P),
    sig!("pthread_mutexattr_setprotocol", I32, P, I32),
    sig!("pthread_mutexattr_settype", I32, P, I32),
    // The initialiser is guest code, and `pthread_once` guarantees it runs
    // exactly once — a guarantee that survives thunking, since the thunk is
    // stable per guest address.
    cbsig!([1], "pthread_once", I32, P, P),
    sig!("pthread_rwlock_destroy", I32, P),
    sig!("pthread_rwlock_init", I32, P, P),
    sig!("pthread_rwlock_rdlock", I32, P),
    sig!("pthread_rwlock_unlock", I32, P),
    sig!("pthread_rwlock_wrlock", I32, P),
    sig!("pthread_self", U64),
    // Cancellation state and type are two `int`s in and the previous value out
    // through a pointer. Nothing here is a guest callback: cancellation runs
    // the *cleanup handlers*, which a guest pushes with `pthread_cleanup_push`
    // and which is not an import at all.
    sig!("pthread_set_qos_class_self_np", I32, U32, I32),
    sig!("pthread_setcancelstate", I32, I32, P),
    sig!("pthread_setcanceltype", I32, I32, P),
    sig!("pthread_setname_np", I32, P),
    sig!("pthread_setschedparam", I32, U64, I32, P),
    sig!("pthread_setspecific", I32, U64, P),
    // `sigset_t` is a `uint32_t` on both architectures — the fact
    // `struct sigaction` rests on — so the two masks are pointers to four
    // bytes the host reads and writes in place. This is `sigprocmask`'s
    // per-thread form, and a game blocks a signal around a worker thread.
    sig!("pthread_sigmask", I32, I32, P, P),
    sig!("pthread_threadid_np", I32, U64, P),
    sig!("putc", I32, I32, P),
    sig!("putchar", I32, I32),
    sig!("puts", I32, P),
    sig!("putwchar", I32, I32),
    sig!("pwrite", I64, I32, P, U64, I64),
    cbsig!([3], "qsort", V, P, U64, U64, P),
    cbsig!([4], "qsort_r", V, P, U64, U64, P, P),
    // `raise` and `kill` deliver to this process, so a guest handler installed
    // through [`crate::signal`] is reached exactly the way the kernel reaches
    // any other: asynchronously, on whichever thread the kernel picks.
    sig!("raise", I32, I32),
    sig!("rand", I32),
    sig!("random", I64),
    sig!("read", I64, I32, P, U64),
    sig!("readdir", P, P),
    sig!("readdir_r", I32, P, P, P),
    sig!("readlink", I64, P, P, U64),
    sig!("readlinkat", I64, I32, P, P, U64),
    sig!("readv", I64, I32, P, I32),
    sig!("realloc", P, P, U64),
    sig!("reallocf", P, P, U64),
    sig!("recv", I64, I32, P, U64, I32),
    sig!("recvfrom", I64, I32, P, U64, I32, P, P),
    sig!("recvmsg", I64, I32, P, I32),
    sig!("remove", I32, P),
    sig!("rename", I32, P, P),
    sig!("rewind", V, P),
    // `strrchr` under the name BSD gave it first. `libsteam_api.dylib` uses it
    // to take the directory off its own path while looking for the client.
    sig!("rindex", P, P, I32),
    sig!("rint", F64, F64),
    sig!("rintf", F32, F32),
    sig!("rmdir", I32, P),
    sig!("round", F64, F64),
    sig!("roundf", F32, F32),
    sig!("scalbn", F64, F64, I32),
    sig!("scalbnf", F32, F32, I32),
    vsig!("scanf", I32, P),
    sig!("sched_get_priority_max", I32, I32),
    sig!("sched_get_priority_min", I32, I32),
    sig!("sched_yield", I32),
    // The import is `_select$1050`, the variant with the 10.5 `fd_set`
    // semantics; arm64 has only the one, and the `$`-suffix fallback finds it.
    sig!("select", I32, I32, P, P, P, P),
    // The named-semaphore surface. `sem_open` is `open`'s shape exactly: a
    // variadic whose ellipsis is a fixed pair — a `mode_t` and an `unsigned` —
    // that arrives only with `O_CREAT` in the same flag word. So it is written
    // out the same way, with both optional arguments always present, and for
    // the same reason it is only sound because the name resolves to **this
    // runtime's** wrapper rather than the host's. See
    // [`crate::shims::openat`].
    sig!("sem_close", I32, P),
    sig!("sem_open", P, P, I32, U32, U32),
    sig!("sem_post", I32, P),
    sig!("sem_trywait", I32, P),
    sig!("sem_unlink", I32, P),
    sig!("sem_wait", I32, P),
    // System V semaphores, which Steam's `libtier0` uses to name a lock across
    // processes. `key_t` is a 32-bit `int` on both architectures, and `struct
    // sembuf` is six bytes — `sem_num` at 0, `sem_op` at 2, `sem_flg` at 4, all
    // fixed-width `short` — on both, compiled and compared rather than assumed;
    // see the note on `kevent`. `semctl` is *not* here: it is variadic, and
    // which of `int`, `struct semid_ds *` and `unsigned short *` its fourth
    // argument is depends on `cmd`, so there is nothing a fixed signature could
    // honestly say. It is refused by name until a program reaches it.
    sig!("semaphore_create", I32, U32, P, I32, I32),
    sig!("semaphore_destroy", I32, U32, U32),
    sig!("semaphore_signal", I32, U32),
    // The `mach_timespec_t` by value; see `clock_sleep` for why `U64` is the
    // right placement rather than an approximation of one.
    sig!("semaphore_timedwait", I32, U32, U64),
    sig!("semaphore_wait", I32, U32),
    sig!("semget", I32, I32, I32, I32),
    sig!("semop", I32, I32, P, U64),
    sig!("send", I64, I32, P, U64, I32),
    sig!("sendmsg", I64, I32, P, I32),
    sig!("sendto", I64, I32, P, U64, I32, P, U32),
    sig!("setbuf", V, P, P),
    sig!("setdomainname", I32, P, I32),
    sig!("setenv", I32, P, P, I32),
    sig!("setgid", I32, U32),
    sig!("setgrent", V),
    sig!("setgroups", I32, I32, P),
    sig!("sethostid", V, I64),
    sig!("sethostname", I32, P, I32),
    sig!("setjmp", I32, P),
    sig!("setlocale", P, I32, P),
    sig!("setpgid", I32, I32, I32),
    sig!("setpriority", I32, I32, U32, I32),
    sig!("setpwent", V),
    sig!("setrlimit", I32, I32, P),
    // `pid_t setsid(void)`. `setpgrp` is deliberately absent beside it: Darwin
    // declares it twice — `pid_t setpgrp(void)` and the obsolete `int
    // setpgrp(pid_t, pid_t)` — and which one a guest was compiled against is not
    // something this table can tell. A program that needs it will say so.
    sig!("setsid", I32),
    sig!("setsockopt", I32, I32, I32, I32, P, U32),
    sig!("settimeofday", I32, P, P),
    sig!("setuid", I32, U32),
    sig!("setusershell", V),
    sig!("setvbuf", I32, P, P, I32, U64),
    sig!("shutdown", I32, I32, I32),
    // Shimmed, like the `setjmp` family and for a related reason: the handler
    // is a guest function pointer that crosses out as a thunk and comes *back*
    // in `signal`'s result and `sigaction`'s `oldact`. See [`crate::signal`].
    sig!("sigaction", I32, I32, P, P),
    // Installed honestly and never used: this runtime delivers a signal on the
    // guest's own stack through a thunk, so an alternate stack the guest asks
    // for is one nothing will run on. Passing through means the *host's*
    // handler would honour it, which is the truthful answer to what was asked.
    sig!("sigaltstack", I32, P, P),
    sig!("siglongjmp", V, P, I32),
    sig!("signal", P, I32, P),
    sig!("sigpending", I32, P),
    sig!("sigprocmask", I32, I32, P, P),
    sig!("sigsetjmp", I32, P, I32),
    sig!("sin", F64, F64),
    sig!("sinf", F32, F32),
    sig!("sinh", F64, F64),
    sig!("sinhf", F32, F32),
    sig!("sleep", U32, U32),
    vsig!("snprintf", I32, P, U64, P),
    sig!("socket", I32, I32, I32, I32),
    sig!("socketpair", I32, I32, I32, I32, P),
    vsig!("sprintf", I32, P, P),
    sig!("sqrt", F64, F64),
    sig!("sqrtf", F32, F32),
    sig!("srand", V, U32),
    sig!("srandom", V, U32),
    vsig!("sscanf", I32, P, P),
    sig!("stat", I32, P, P),
    sig!("statfs", I32, P, P),
    sig!("statvfs", I32, P, P),
    sig!("stpcpy", P, P, P),
    sig!("strcasecmp", I32, P, P),
    sig!("strcat", P, P, P),
    sig!("strchr", P, P, I32),
    sig!("strcmp", I32, P, P),
    sig!("strcoll", I32, P, P),
    sig!("strcoll_l", I32, P, P, P),
    sig!("strcpy", P, P, P),
    sig!("strcspn", U64, P, P),
    sig!("strdup", P, P),
    sig!("strerror", P, I32),
    sig!("strerror_r", I32, I32, P, U64),
    sig!("strftime", U64, P, U64, P, P),
    sig!("strftime_l", U64, P, U64, P, P, P),
    sig!("strlcat", U64, P, P, U64),
    sig!("strlcpy", U64, P, P, U64),
    sig!("strlen", U64, P),
    sig!("strmode", V, I32, P),
    sig!("strncasecmp", I32, P, P, U64),
    sig!("strncat", P, P, P, U64),
    sig!("strncmp", I32, P, P, U64),
    sig!("strncpy", P, P, P, U64),
    sig!("strndup", P, P, U64),
    sig!("strnlen", U64, P, U64),
    sig!("strpbrk", P, P, P),
    // `strptime` writes into a guest `struct tm`, which is twelve `int`s and
    // two pointers on both architectures.
    sig!("strptime", P, P, P, P),
    sig!("strrchr", P, P, I32),
    sig!("strspn", U64, P, P),
    // `char *strsignal(int)`. A **pointer** return, and the reason to say so:
    // the table declaring `wcsstr` as returning an `int` is what M7.16 spent a
    // day on, and a signal-name lookup lives in a crash reporter, which is the
    // one place a truncated pointer turns a diagnostic into a second crash.
    sig!("strsignal", P, I32),
    // ## Vouchers
    //
    // A voucher is an opaque kernel object carried alongside a Mach message so
    // that work done on the receiving side is attributed — for QoS, for activity
    // tracing — to whoever asked for it. Everything here is a `voucher_t`, which
    // is an opaque pointer, or a `mach_msg_header_t *`, which is a header the
    // guest built in its own memory and the kernel reads directly.
    //
    // Passthrough is right and not merely convenient: a voucher names a
    // *thread's* current attribution, the thread is a real host thread, and the
    // message is going to the real kernel. There is nothing here whose meaning
    // differs between the two architectures. Crashpad's Mach port handshake is
    // what reaches them.
    sig!("voucher_mach_msg_set", I32, P),
    sig!("voucher_mach_msg_clear", V, P),
    sig!("voucher_mach_msg_adopt", P, P),
    sig!("voucher_mach_msg_revert", V, P),
    sig!("voucher_copy", P),
    sig!("voucher_copy_without_importance", P),
    sig!("voucher_adopt", P, P),
    sig!("voucher_release", V, P),
    sig!("voucher_retain", P, P),
    sig!("strstr", P, P, P),
    sig!("strtod", F64, P, P),
    sig!("strtod_l", F64, P, P, P),
    sig!("strtof", F32, P, P),
    sig!("strtof_l", F32, P, P, P),
    sig!("strtok", P, P, P),
    sig!("strtok_r", P, P, P, P),
    sig!("strtol", I64, P, P, I32),
    sig!("strtoll", I64, P, P, I32),
    sig!("strtonum", I64, P, I64, I64, P),
    sig!("strtoul", U64, P, P, I32),
    sig!("strtoull", U64, P, P, I32),
    sig!("strxfrm_l", U64, P, P, U64, P),
    sig!("swab", V, P, P, I64),
    sig!("symlink", I32, P, P),
    sig!("sync", V),
    sig!("sysconf", I64, I32),
    sig!("sysctl", I32, P, U32, P, P, P, U64),
    sig!("sysctlbyname", I32, P, P, P, P, U64),
    sig!("sysctlnametomib", I32, P, P, P),
    vsig!("syslog", V, I32, P),
    // The string is the guest's and the shell is the host's, which is exactly
    // what a guest re-launching itself through `open steam://` wants.
    sig!("system", I32, P),
    sig!("shm_open", I32, P, I32, I32),
    sig!("shm_unlink", I32, P),
    sig!("tan", F64, F64),
    sig!("tanf", F32, F32),
    sig!("tanh", F64, F64),
    sig!("tanhf", F32, F32),
    sig!("task_for_pid", I32, U32, I32, P),
    sig!("task_info", I32, U32, U32, P, P),
    sig!("task_threads", I32, U32, P, P),
    sig!("tcflush", I32, I32, I32),
    sig!("tcgetattr", I32, I32, P),
    sig!("tcsendbreak", I32, I32, I32),
    sig!("tcsetattr", I32, I32, I32, P),
    sig!("tempnam", P, P, P),
    sig!("tgetent", I32, P, P),
    sig!("tgetstr", P, P, P),
    sig!("tgoto", P, P, I32, I32),
    sig!("thread_abort", I32, U32),
    sig!("thread_abort_safely", I32, U32),
    // `thread_get_state` and `thread_set_state` pass through and will not do
    // what a guest asking for `x86_THREAD_STATE64` means by them. The thread is
    // a real arm64 thread running a Cranelift translation, so the kernel either
    // refuses the flavour or answers with this CPU's registers — and the guest's
    // x86-64 registers at that point are not recoverable, which is the same fact
    // `SA_SIGINFO` is refused for in [`crate::signal`]. The kernel's own
    // `KERN_INVALID_ARGUMENT` is a better diagnostic than a signature refusal
    // would be, because it names the flavour; what a runtime must not do is
    // synthesize an answer, and passing through does not.
    sig!("thread_get_state", I32, U32, I32, P, P),
    sig!("thread_info", I32, U32, U32, P, P),
    // The scheduling policies, which pass through and *do* mean what the guest
    // means by them — unlike the register-state pair above. A
    // `thread_policy_t` is a pointer to a flavour-specific structure of `int`s
    // (`THREAD_TIME_CONSTRAINT_POLICY` for an audio thread,
    // `THREAD_AFFINITY_POLICY` for a worker), and `count` is in units of
    // `integer_t`, so the structures are architecture-independent by
    // construction: there is no pointer or `long` in any of them.
    sig!("thread_policy_get", I32, U32, U32, P, P, P),
    sig!("thread_policy_set", I32, U32, U32, P, U32),
    // The exception-port family. A guest crash reporter installs a handler
    // here — Unity's does — and the ports are plain 32-bit names in arrays the
    // caller owns.
    //
    // `thread_swap_exception_ports` is deliberately absent: it takes **ten**
    // integer arguments, and eight is what one over-wide prototype can deliver.
    // Left undescribed so it refuses by name rather than being given a
    // signature that silently drops the last two, which are the out-parameters
    // a caller reads the old handlers back through.
    sig!("thread_get_exception_ports", I32, U32, U32, P, P, P, P, P),
    sig!("thread_set_exception_ports", I32, U32, U32, U32, I32, I32),
    sig!("thread_resume", I32, U32),
    sig!("thread_set_state", I32, U32, I32, P, U32),
    sig!("thread_suspend", I32, U32),
    sig!("time", I64, P),
    sig!("timegm", I64, P),
    // The other spellings; see `_tlv_bootstrap` and `_tlv_atexit` above.
    cbsig!([0], "tlv_atexit", V, P, P),
    sig!("tlv_bootstrap", P, P),
    sig!("tmpfile", P),
    sig!("tolower", I32, I32),
    sig!("toupper", I32, I32),
    // The output function is called once per character: `tputs` is how a
    // terminal-aware program emits a capability string, and `/bin/ls` reaches
    // it whenever its output is a terminal.
    cbsig!([2], "tputs", I32, P, I32, P),
    sig!("trunc", F64, F64),
    sig!("truncate", I32, P, I64),
    sig!("truncf", F32, F32),
    sig!("ttyname", P, I32),
    sig!("ttyname_r", I32, I32, P, U64),
    sig!("umask", U32, U32),
    sig!("uname", I32, P),
    sig!("ungetc", I32, I32, P),
    sig!("ungetwc", I32, I32, P),
    sig!("unlink", I32, P),
    sig!("unsetenv", I32, P),
    sig!("user_from_uid", P, U32, I32),
    sig!("usleep", I32, U32),
    sig!("utime", I32, P, P),
    sig!("utimes", I32, P, P),
    sig!("uuid_unparse_upper", V, P, P),
    // ## The `va_list` family
    //
    // Not variadic — each takes a `va_list` its caller already built — and still
    // impossible to forward, for the mirror image of the reason a variadic is:
    // the argument is a *guest* `va_list`, three fields and a register save area
    // laid out by the SysV AMD64 ABI, and the host's `vsnprintf` would read it
    // as whatever arm64's `va_list` happens to be. So these are shimmed and the
    // signatures exist to name them at an exit. See [`crate::shims::valist`].
    sig!("vasprintf", I32, P, P, P),
    sig!("vfprintf", I32, P, P, P),
    sig!("vprintf", I32, P, P),
    sig!("vsnprintf", I32, P, U64, P, P),
    // Shimmed. The wide form, and the one every Unreal Engine 3 string is built
    // by; see [`crate::shims::printf::vswprintf`].
    sig!("vswprintf", I32, P, U64, P, P),
    sig!("vsprintf", I32, P, P, P),
    sig!("wait", I32, P),
    sig!("wait3", I32, P, I32, P),
    sig!("waitpid", I32, I32, P, I32),
    vsig!("warn", V, P),
    vsig!("warnx", V, P),
    sig!("wcrtomb_l", U64, P, I32, P, P),
    sig!("wcscat", P, P, P),
    // The second argument is a `wchar_t`, which is a 32-bit integer and not a
    // pointer — the one member of this family whose scalar is easy to misread.
    sig!("wcschr", P, P, I32),
    sig!("wcscmp", I32, P, P),
    sig!("wcsncmp", I32, P, P, U64),
    // `wcscpy` and `wcsstr` return `wchar_t *`, and both were `I32` — the same
    // truncation `OSAtomicAdd32` above is about, in the other direction and one
    // step worse, because the value is an address rather than a count. A Hat in
    // Time's `FString::Replace` writes a NUL *through* `wcsstr`'s answer to cut
    // the haystack at the match: with a 32-bit return the pointer came back with
    // its top half gone, the null check passed, and the store faulted at an
    // address that was the low word of a real one.
    sig!("wcscpy", P, P, P),
    sig!("wcsstr", P, P, P),
    sig!("wcscoll_l", I32, P, P, P),
    sig!("wcslen", U64, P),
    sig!("wcsncat", P, P, P, U64),
    sig!("wcsncpy", P, P, P, U64),
    sig!("wcsnrtombs_l", U64, P, P, U64, U64, P, P),
    sig!("wcsrchr", P, P, I32),
    sig!("wcsrtombs", U64, P, P, U64, P),
    sig!("wcstod", F64, P, P),
    sig!("wcstof", F32, P, P),
    sig!("wcstol", I64, P, P, I32),
    sig!("wcstoll", I64, P, P, I32),
    sig!("wcstombs", U64, P, P, U64),
    sig!("wcstoul", U64, P, P, I32),
    sig!("wcstoull", U64, P, P, I32),
    sig!("wcsxfrm_l", U64, P, P, U64, P),
    sig!("wctob_l", I32, I32, P),
    sig!("wctomb", I32, P, I32),
    sig!("wcwidth", I32, I32),
    sig!("wmemchr", P, P, I32, U64),
    sig!("wmemcmp", I32, P, P, U64),
    sig!("write", I64, I32, P, U64),
    sig!("writev", I64, I32, P, I32),
    sig!("zError", P, I32),
    sig!("zlibVersion", P),
    // Metal symbols
    sig!("MTLCopyAllDevices", P),
    // ## CoreAudio's hardware layer, and the clock underneath it
    //
    // Roblox does not use an output `AudioUnit` the way a GameMaker runner
    // does. It talks to the *device* — `AudioObject` properties to enumerate
    // and configure, an `AudioDeviceIOProcID` to register a render function,
    // and `AudioDeviceStart`/`Stop` around it — which is the same shape one
    // level down. Everything here is a scalar, an `AudioObjectID`, or a
    // structure the host reads and writes in place through a pointer the guest
    // owns, so the whole family is passthrough except for the one function that
    // takes guest code.
    //
    // `AudioConvertHostTimeToNanos` and `AudioGetCurrentHostTime` are the pair
    // an audio clock is built out of: the host time base is `mach_absolute_time`
    // and its conversion is this machine's, which is the right answer precisely
    // because the guest is being paced by this machine's audio hardware.
    sig!("AudioConvertHostTimeToNanos", U64, U64),
    sig!("AudioGetCurrentHostTime", U64),
    // `AudioDeviceIOProcID` looks like a function pointer in the header — it is
    // typedef'd as one — and is not a callback argument anywhere but the
    // creating call. What `AudioDeviceCreateIOProcID` writes out through its
    // fourth parameter is a token CoreAudio minted, so the guest hands *the
    // host's own value* back to `Destroy`, `Start` and `Stop`. Marking it a
    // callback there would substitute a thunk for a token and unregister
    // nothing.
    cbsig!([1], "AudioDeviceCreateIOProcID", I32, U32, P, P, P),
    sig!("AudioDeviceDestroyIOProcID", I32, U32, P),
    sig!("AudioDeviceStart", I32, U32, P),
    sig!("AudioDeviceStop", I32, U32, P),
    sig!("AudioHardwareUnload", I32),
    sig!("AudioObjectHasProperty", BOOL, U32, P),
    sig!("AudioObjectIsPropertySettable", I32, U32, P, P),
    sig!("AudioObjectSetPropertyData", I32, U32, P, U32, P, U32, P),
    sig!("AudioFormatGetProperty", I32, U32, U32, P, P, P),
    // The converter family's remaining half. `AudioConverterNew` and
    // `FillComplexBuffer` are described above; these are the property calls
    // around them and the codec-specific constructor, whose `AudioClassDescription`
    // array is read through a pointer.
    sig!("AudioConverterNewSpecific", I32, P, P, U32, P, P),
    sig!("AudioConverterGetProperty", I32, P, U32, P, P),
    sig!("AudioConverterGetPropertyInfo", I32, P, U32, P, P),
    sig!("AudioConverterSetProperty", I32, P, U32, U32, P),
    // ## CoreMedia, CoreVideo and VideoToolbox
    //
    // Roblox decodes and encodes video — its own capture path and the H.264
    // stream a game may play — and the three frameworks travel together. Almost
    // all of it is handles and out-parameters. What is *not* here is the
    // `CMTime` family, which passes a twenty-four byte struct by value and
    // lives in [`crate::shims::coremedia`]; this is everything that does not.
    sig!(
        "CMBlockBufferCreateContiguous",
        I32,
        P,
        P,
        P,
        P,
        U64,
        U64,
        U32,
        P
    ),
    sig!(
        "CMBlockBufferCreateWithMemoryBlock",
        I32,
        P,
        P,
        U64,
        P,
        P,
        U64,
        U64,
        U32,
        P
    ),
    sig!("CMBlockBufferGetDataPointer", I32, P, U64, P, P, P),
    sig!("CMCopyDictionaryOfAttachments", P, P, P, U32),
    sig!("CMFormatDescriptionGetMediaSubType", U32, P),
    // Twelve parameters, four past AAPCS64's eighth, and the fourth is guest
    // code: `CMSampleBufferMakeDataReadyCallback` is called by CoreMedia when
    // something downstream needs the samples before the producer said they were
    // ready. A guest that passes null — the common case, and what a buffer built
    // from an already-decoded frame does — reaches none of that, and the thunk
    // substitution leaves null alone.
    cbsig!(
        [3],
        "CMSampleBufferCreate",
        I32,
        P,
        P,
        BOOL,
        P,
        P,
        P,
        I64,
        I64,
        P,
        I64,
        P,
        P
    ),
    sig!("CMSampleBufferDataIsReady", BOOL, P),
    sig!("CMSampleBufferIsValid", BOOL, P),
    sig!("CMSampleBufferGetDataBuffer", P, P),
    sig!("CMSampleBufferGetFormatDescription", P, P),
    sig!("CMSampleBufferGetImageBuffer", P, P),
    sig!("CMSampleBufferGetNumSamples", I64, P),
    sig!("CMSampleBufferGetSampleAttachmentsArray", P, P, BOOL),
    sig!(
        "CMVideoFormatDescriptionCreate",
        I32,
        P,
        U32,
        I32,
        I32,
        P,
        P
    ),
    sig!(
        "CMVideoFormatDescriptionCreateFromH264ParameterSets",
        I32,
        P,
        U64,
        P,
        P,
        I32,
        P
    ),
    sig!(
        "CMVideoFormatDescriptionCreateFromHEVCParameterSets",
        I32,
        P,
        U64,
        P,
        P,
        I32,
        P,
        P
    ),
    // `CMVideoDimensions` is two `int32_t` — **eight** bytes — which is the
    // aggregate both ABIs agree about completely: one INTEGER eightbyte in
    // `rax` under SysV AMD64, one eight-byte composite in `x0` under AAPCS64,
    // same two halves in the same order. So it is a `U64` return and not a
    // shim, unlike every other struct in this family. Its twenty-four byte
    // sibling `CMTime` is exactly why the distinction has to be checked rather
    // than assumed.
    sig!("CMVideoFormatDescriptionGetDimensions", U64, P),
    sig!(
        "CMVideoFormatDescriptionGetH264ParameterSetAtIndex",
        I32,
        P,
        U64,
        P,
        P,
        P,
        P
    ),
    sig!(
        "CMVideoFormatDescriptionGetHEVCParameterSetAtIndex",
        I32,
        P,
        U64,
        P,
        P,
        P,
        P
    ),
    sig!("CVMetalTextureCacheCreate", I32, P, P, P, P, P),
    sig!(
        "CVMetalTextureCacheCreateTextureFromImage",
        I32,
        P,
        P,
        P,
        P,
        U64,
        U64,
        U64,
        U64,
        P
    ),
    sig!("CVMetalTextureGetTexture", P, P),
    sig!("CVPixelBufferCreate", I32, P, U64, U64, U32, P, P),
    sig!("CVPixelBufferGetBaseAddressOfPlane", P, P, U64),
    sig!("CVPixelBufferGetBytesPerRowOfPlane", U64, P, U64),
    sig!("CVPixelBufferGetHeightOfPlane", U64, P, U64),
    sig!("CVPixelBufferGetWidthOfPlane", U64, P, U64),
    sig!("CVPixelBufferGetPixelFormatType", U32, P),
    sig!("CVPixelBufferPoolCreatePixelBuffer", I32, P, P, P),
    sig!("CVPixelBufferRelease", V, P),
    // VideoToolbox. The compression session's output callback is an ordinary
    // guest function pointer in the argument list; the *decompression* session's
    // is inside a record structure, which is why that one is a shim.
    cbsig!(
        [7],
        "VTCompressionSessionCreate",
        I32,
        P,
        I32,
        I32,
        U32,
        P,
        P,
        P,
        P,
        P,
        P
    ),
    sig!("VTCompressionSessionGetPixelBufferPool", P, P),
    sig!("VTCompressionSessionInvalidate", V, P),
    sig!("VTCompressionSessionPrepareToEncodeFrames", I32, P),
    sig!("VTCopyVideoEncoderList", I32, P, P),
    sig!(
        "VTDecompressionSessionCanAcceptFormatDescription",
        BOOL,
        P,
        P
    ),
    sig!("VTDecompressionSessionDecodeFrame", I32, P, P, U32, P, P),
    sig!("VTDecompressionSessionFinishDelayedFrames", I32, P),
    sig!("VTDecompressionSessionInvalidate", V, P),
    sig!("VTDecompressionSessionWaitForAsynchronousFrames", I32, P),
    sig!("VTIsHardwareDecodeSupported", BOOL, U32),
    sig!("VTRegisterSupplementalVideoDecoderIfAvailable", V, U32),
    sig!("VTSessionCopyProperty", I32, P, P, P, P),
    sig!("VTSessionSetProperty", I32, P, P, P),
    // ## FSEvents
    //
    // A directory watcher, which Roblox uses to notice when its own content
    // changes underneath it. `FSEventStreamCreate` takes the callback directly
    // and a *context* structure beside it — and that structure holds `retain`,
    // `release` and `copyDescription` function pointers, which a guest passing
    // a plain refcon leaves null. Null is what the substitution sees and what it
    // leaves alone; a guest that fills them in would need the same treatment
    // `CFRunLoopSourceCreate` is refused for, and would fail by name there
    // rather than here.
    cbsig!([1], "FSEventStreamCreate", P, P, P, P, P, U64, F64, U32),
    sig!("FSEventStreamSetDispatchQueue", V, P, P),
    sig!("FSEventStreamStart", BOOL, P),
    sig!("FSEventStreamStop", V, P),
    sig!("FSEventStreamFlushSync", V, P),
    sig!("FSEventStreamInvalidate", V, P),
    sig!("FSEventStreamRelease", V, P),
    // ## IOKit, and the rest of the framework odds and ends
    //
    // `IORegistryEntryIDMatching` and `IOServiceGetMatchingService` are how a
    // program finds one device rather than iterating; `IOHIDDeviceCreate` wraps
    // an `io_service_t` a match produced. All handles.
    sig!("IOHIDDeviceCreate", P, P, U32),
    sig!("IOHIDDeviceGetReport", I32, P, U32, I64, P, P),
    sig!("IORegistryEntryGetRegistryEntryID", I32, U32, P),
    sig!("IORegistryEntryIDMatching", P, U64),
    sig!("IOServiceGetMatchingService", U32, U32, P),
    sig!("CGColorGetTypeID", U64),
    sig!("CTFontGetTypeID", U64),
    sig!("CTRunGetTypeID", U64),
    sig!("CGEventSourceKeyState", BOOL, I32, U16),
    sig!("UTTypeConformsTo", BOOL, P, P),
    // `UInt8 LMGetKbdLast(void)` — a Carbon low-memory accessor, which on a
    // 64-bit system is an ordinary function reading a global the OS maintains.
    sig!("LMGetKbdLast", U8),
    // `CFDictionaryRef _CFCopySystemVersionDictionary(void)` — private CF, and
    // the same question [`crate::compat`] answers for the SDK gates: the version
    // it reports is *this machine's*, because that is what the guest is running
    // on.
    sig!("_CFCopySystemVersionDictionary", P),
    // ## libdispatch's function-pointer half
    //
    // The `_f` suffix is libdispatch's pre-blocks spelling: a context pointer
    // and a bare function, rather than a literal carrying both. That makes them
    // ordinary `cbsig!` entries and *not* [`crate::shims::block`]'s problem —
    // the thing being substituted is the argument itself.
    cbsig!([2], "dispatch_async_f", V, P, P, P),
    cbsig!([2], "dispatch_sync_f", V, P, P, P),
    cbsig!([3], "dispatch_after_f", V, U64, P, P, P),
    cbsig!([1], "dispatch_set_finalizer_f", V, P, P),
    sig!("dispatch_group_enter", V, P),
    sig!("dispatch_group_leave", V, P),
    sig!("dispatch_queue_get_label", P, P),
    // `dispatch_queue_create_with_target$V2` is the only spelling a modern
    // compiler emits, and the `$`-suffix fallback in [`lookup`] finds this
    // entry for it.
    sig!("dispatch_queue_create_with_target", P, P, P, P),
    // `void _os_log_impl(void *dso, os_log_t, os_log_type_t, const char *format,
    // uint8_t *buf, uint32_t size)` — **not** variadic, which is the surprise.
    // The compiler encodes the arguments into `buf` at the call site and this
    // takes the finished blob, so what crosses is a pointer to bytes the guest
    // laid out and a length. The encoding is a documented wire format rather
    // than a register convention, so it needs no translation.
    sig!("_os_log_impl", V, P, P, U8, P, P, U32),
    sig!("os_log_create", P, P, P),
    sig!("os_log_type_enabled", BOOL, P, U8),
    sig!("os_release", V, P),
    // ## Mach, and the ports a program keeps for itself
    //
    // The exception-port family is **not** here — it is in [`crate::signal`],
    // for `task_set_exception_ports`' reason. What is left is bookkeeping: reply
    // ports, port sets, dead-name notifications, and the two `mach_vm` calls
    // `RobloxCrashHandler` reads another task's memory with.
    sig!("mach_error_string", P, I32),
    sig!("mach_port_insert_member", I32, U32, U32, U32),
    sig!(
        "mach_port_request_notification",
        I32,
        U32,
        U32,
        I32,
        U32,
        U32,
        U32,
        P
    ),
    sig!("mach_vm_read", I32, U32, U64, U64, P, P),
    sig!("mach_vm_region_recurse", I32, U32, P, P, P, P, P),
    sig!("mig_dealloc_reply_port", V, U32),
    sig!("mig_get_reply_port", U32),
    sig!("mig_put_reply_port", V, U32),
    sig!("host_processor_info", I32, U32, I32, P, P, P),
    sig!("semaphore_signal_all", I32, U32),
    sig!("task_suspend", I32, U32),
    sig!("task_resume", I32, U32),
    sig!("pid_for_task", I32, U32, P),
    sig!("bootstrap_check_in", I32, U32, P, P),
    sig!("bootstrap_strerror", P, I32),
    sig!("sys_icache_invalidate", V, P, U64),
    // `int proc_get_wakemon_params(pid_t, int32_t *rate_hz, int32_t *flags)` —
    // libproc SPI with no header on this system, read out of the kernel's own
    // `proc_info` interface. Two out-parameters and a pid, which is the whole of
    // it; a program asks so it can tell whether the OS is watching its wakeup
    // rate.
    sig!("proc_get_wakemon_params", I32, I32, P, P),
    // ## The allocator's zone interface
    //
    // Imported by the **interposing mimalloc** rather than by Roblox itself
    // (see the roadmap's M7.17), which is what makes most of it passthrough: a
    // zone the guest got from `malloc_default_zone` is libsystem_malloc's own
    // structure, and calling through it is the host allocating for itself. The
    // two that hand the *host* a structure of **guest** function pointers —
    // `malloc_zone_register` and `malloc_get_all_zones` — are refused, because
    // that is the direction nothing here can fix.
    sig!("malloc_default_zone", P),
    sig!("malloc_default_purgeable_zone", P),
    sig!("malloc_create_zone", P, U64, U32),
    sig!("malloc_destroy_zone", V, P),
    sig!("malloc_zone_from_ptr", P, P),
    sig!("malloc_get_zone_name", P, P),
    sig!("malloc_set_zone_name", V, P, P),
    sig!("malloc_zone_malloc", P, P, U64),
    sig!("malloc_zone_calloc", P, P, U64, U64),
    sig!("malloc_zone_realloc", P, P, P, U64),
    sig!("malloc_zone_valloc", P, P, U64),
    sig!("malloc_zone_memalign", P, P, U64, U64),
    sig!("malloc_zone_free", V, P, P),
    sig!("malloc_zone_batch_malloc", U32, P, U64, P, U32),
    sig!("malloc_zone_batch_free", V, P, P, U32),
    sig!("malloc_zone_check", I32, P),
    sig!("malloc_zone_log", V, P, P),
    sig!("malloc_zone_print", V, P, I32),
    sig!("malloc_zone_print_ptr_info", V, P),
    sig!("malloc_zone_unregister", V, P),
    sig!("malloc_good_size", U64, U64),
    // `void *valloc(size_t)` and its private counterpart `vfree`, which
    // libsystem_malloc exports without a header. mimalloc imports the pair
    // together and they must stay together: a block from `valloc` is not one
    // `free` may take.
    sig!("valloc", P, U64),
    sig!("vfree", V, P),
    // `int malloc_jumpstart(int cookie)` — undocumented libsystem_malloc SPI
    // taking and returning an `int`, called once during allocator start-up.
    sig!("malloc_jumpstart", I32, I32),
    // The three halves of the allocator's `pthread_atfork` handlers, which
    // mimalloc calls directly because it installs its own. All `void (void)`.
    sig!("_malloc_fork_prepare", V),
    sig!("_malloc_fork_parent", V),
    sig!("_malloc_fork_child", V),
    // ## The remaining C library
    //
    // Ordinary scalars and pointers, each reached by Roblox and none of them
    // interesting on its own.
    sig!("_Exit", V, I32),
    sig!("__exp10", F64, F64),
    // `int __srget(FILE *)` and `int __swbuf(int, FILE *)` — the out-of-line
    // halves of the `getc`/`putc` macros, which a guest inlines and which
    // therefore *read the `FILE` structure's fields directly*. That works only
    // because `struct __sFILE` has the same layout on both architectures — every
    // member is a pointer or an `int` — so the `FILE` a guest got from the
    // host's `fopen` is one its own inlined macro can walk.
    sig!("__srget", I32, P),
    sig!("__swbuf", I32, I32, P),
    // `double __floattidf(__int128)`. A 128-bit integer argument is two integer
    // registers under SysV AMD64 and two under AAPCS64, in the same order, so
    // the two halves are written out as two `U64`s the way a `CFRange` is
    // written out as two `CFIndex`.
    sig!("__floattidf", F64, U64, U64),
    // `char *realpath(const char *restrict path, char *restrict resolved)`.
    // Roblox imports the `$DARWIN_EXTSN` spelling, which is the one that accepts
    // a null buffer and allocates; [`lookup`]'s `$`-suffix fallback finds this
    // entry for it, and the *address* it is bound to is the suffixed symbol the
    // host really exports — so the extension is honoured by the resolver rather
    // than approximated here.
    sig!("realpath", P, P, P),
    sig!("cfmakeraw", V, P),
    sig!("chown", I32, P, U32, U32),
    sig!("fchown", I32, I32, U32, U32),
    sig!("lchown", I32, P, U32, U32),
    sig!("fchmodat", I32, I32, P, U16, I32),
    sig!("link", I32, P, P),
    sig!("dirfd", I32, P),
    sig!("fsctl", I32, P, U64, P, U32),
    sig!("gethostuuid", I32, P, P),
    sig!("uuid_generate", V, P),
    sig!("setxattr", I32, P, P, P, U64, U32, I32),
    sig!("removexattr", I32, P, P, I32),
    sig!("sendfile", I32, I32, I32, I64, P, P, I32),
    sig!("connectx", I32, I32, P, U32, U32, P, U32, P, P),
    sig!("posix_spawnattr_setsigdefault", I32, P, P),
    sig!("pthread_mutexattr_setpolicy_np", I32, P, I32),
    sig!("pthread_rwlock_tryrdlock", I32, P),
    sig!("pthread_rwlock_trywrlock", I32, P),
    sig!("rand_r", I32, P),
    sig!("srandomdev", V),
    sig!("tzset", V),
    sig!("frexpf", F32, F32, P),
    sig!("res_9_init", I32),
    sig!("res_9_close", V),
];

/// A symbol with no public declaration anywhere on this system.
///
/// The milestone's "explicit unknown list", and the honest end of the refusal
/// spectrum: these are private SPI whose argument list cannot be read out of a
/// header, so the choice is to guess or to say so. Guessing is what the whole
/// module exists not to do.
const UNDECLARED: &str = "no public declaration; a guessed signature would fail silently";

/// The commonest refusal by far, and the least interesting: a symbol that is a
/// *variable*. An imported global is bound into a slot the guest loads from and
/// is never the target of a call, so there is nothing to describe. Cocoa and
/// CoreFoundation supply most of them — every `NSString *` notification name,
/// every `CFStringRef` property key — and a program that talks to a framework
/// imports dozens.
const DATA: &str = "a data import, read through its slot and never called";

/// A libc++ entry point whose whole purpose is to `throw`.
///
/// Describing the signature would be easy and calling it would be wrong: the
/// host's `throw` raises through the *host's* unwinder, which would walk out of
/// the JIT's frames rather than the guest's. Guest exceptions are virtualized
/// against the guest's own stack ([`crate::unwind`]), and there is no path from
/// one to the other. A guest reaching one of these is already failing; saying so
/// by name is strictly better than an unwind into recompiled code.
const THROWS: &str = "throws a C++ exception, which cannot be raised across the JIT boundary";

/// A struct by value, in a *C* function.
///
/// [`crate::abi::aggregate`] marshals these, and an Objective-C method reaches
/// it — because a method carries its own type encoding, which describes the
/// struct's members and their offsets. This table has no such vocabulary:
/// [`FunctionAbi::types`] is a list of scalars, and there is nowhere to write
/// down that an argument is four `double`s. Extending it is the work these
/// refusals are waiting for, and it is a change to the table's shape rather
/// than to the marshaller.
const AGGREGATE: &str =
    "passes or returns a struct by value, which this table has no way to describe";

/// Takes a `std::string` and is not a string operation.
///
/// libc++ lays `basic_string` out differently on the two architectures — see
/// [`crate::shims::cxxstring`], which is where that is written down and where
/// the string's own entry points are implemented against the *guest's* layout.
/// These are the calls that reach past that: the object they build belongs to
/// the host (a `runtime_error`'s message, a `random_device`'s token), so
/// answering them means more than reading a string correctly, and each is a work
/// item of its own rather than a line in that module. Refused by name, which is
/// what the shim exists to make possible — before it, this was a signature that
/// looked right and returned garbage.
const GUEST_STRING: &str =
    "takes a std::string, whose layout differs between x86-64 and arm64 macOS";

/// The [`crate::jmpbuf`] refusal, one level further out.
///
/// `getcontext` and friends save the *machine's* registers and stack pointer.
/// Forwarding one saves this runtime's arm64 frame, and `setcontext` would then
/// resume the JIT rather than the guest — the same wrongness `setjmp` has, and
/// without the excuse that a `jmp_buf`'s contents are private, because a
/// `ucontext_t` has a documented layout a guest may read.
const CONTEXT: &str =
    "saves or restores a machine context, which here is the host's and not the guest's";

/// The one scalar the two architectures spell differently.
///
/// x86-64 macOS's `long double` is x87's 80-bit extended format; arm64's is
/// IEEE binary128. A value has no common register and no lossless move between
/// the two, so a forwarded call would return a plausible number with the
/// precision the caller asked for silently gone.
const LONG_DOUBLE: &str =
    "takes or returns a long double, which is x87 80-bit on x86-64 and binary128 on arm64";

/// Symbols that are described and deliberately not callable, in no particular
/// order for the same reason as [`SIGNATURES`].
pub static REFUSED: &[(&str, &str)] = &[
    // The one CoreFoundation constructor whose *argument* is a table of
    // callbacks rather than a callback: `CFRunLoopSourceContext` holds seven
    // function pointers, all guest code, inside a structure the guest owns.
    // Substituting them means rewriting the structure, which is the same work
    // item a block argument is waiting for.
    (
        "CFRunLoopSourceCreate",
        "its context structure holds guest function pointers the host would call as arm64",
    ),
    // `CFRange rangeToSearch` is passed **by value**. SysV AMD64 classifies it
    // into the same two integer registers two `CFIndex` arguments would occupy,
    // so writing it out as a pair places correctly — and is still not what the
    // table may say, because `the_table_declares_the_parameters_these_functions_
    // really_have` compiles every entry against the real header and a five-
    // parameter function declared with six is a lie whether or not it works.
    // Refused until either the marshaller can carry an aggregate argument or
    // this gets the shim `CGEventCreateMouseEvent` got.
    ("CFStringFindWithOptions", AGGREGATE),
    // Seventeen parameters: an allocator and the sixteen bytes of a UUID. The
    // prototype is honest and the *marshaller* is the limit — five of the bytes
    // reach the integer registers and the other eleven need stack placement this
    // cannot yet do, which
    // `every_signature_is_one_the_marshaller_can_place` says out loud.
    (
        "CFUUIDGetConstantUUIDWithBytes",
        "seventeen parameters, more than the marshaller can place",
    ),
    ("CGRectContainsPoint", AGGREGATE),
    ("CGRectInset", AGGREGATE),
    // `NSApp` is data like the rest of this block, and the least obvious of
    // them: it reads as a function call in every program that uses it
    // (`[NSApp run]`), and it is a global `id` the application object is stored
    // in.
    ("NSApp", DATA),
    ("NSCalibratedRGBColorSpace", DATA),
    ("NSDefaultRunLoopMode", DATA),
    ("NSDeviceResolution", DATA),
    ("NSFontAttributeName", DATA),
    ("NSForegroundColorAttributeName", DATA),
    ("NSLocaleCountryCode", DATA),
    ("NSLocaleLanguageCode", DATA),
    ("NSLocalizedDescriptionKey", DATA),
    ("NSLocalizedFailureReasonErrorKey", DATA),
    ("NSOSStatusErrorDomain", DATA),
    ("NSPOSIXErrorDomain", DATA),
    ("NSParagraphStyleAttributeName", DATA),
    ("NSPasteboardTypeString", DATA),
    ("NSStringPboardType", DATA),
    ("NSURLAuthenticationMethodDefault", DATA),
    ("NSURLProtectionSpaceHTTPProxy", DATA),
    ("NSUnderlyingErrorKey", DATA),
    ("NSWindowDidMoveNotification", DATA),
    ("NSWindowDidResizeNotification", DATA),
    ("NSWindowWillCloseNotification", DATA),
    ("_DefaultRuneLocale", DATA),
    ("_NSConcreteGlobalBlock", DATA),
    ("_NSConcreteStackBlock", DATA),
    (
        "_ZNKSt3__120__vector_base_commonILb1EE20__throw_length_errorEv",
        THROWS,
    ),
    (
        "_ZNKSt3__121__basic_string_commonILb1EE20__throw_length_errorEv",
        THROWS,
    ),
    (
        "_ZNSt13runtime_errorC1ERKNSt3__112basic_stringIcNS0_11char_traitsIcEENS0_9allocatorIcEEEE",
        GUEST_STRING,
    ),
    (
        "_ZNSt3__113random_deviceC1ERKNS_12basic_stringIcNS_11char_traitsIcEENS_9allocatorIcEEEE",
        GUEST_STRING,
    ),
    ("_ZNSt3__120__throw_system_errorEiPKc", THROWS),
    ("_ZTISt12length_error", DATA),
    ("_ZTISt13runtime_error", DATA),
    ("_ZTISt9bad_alloc", DATA),
    // A `type_info` and the vtable a guest's own `type_info` objects point at.
    // Both are read through their slots and compared by address, which is what
    // makes a `catch` clause decidable at all — see [`crate::unwind`].
    ("_ZTISt9exception", DATA),
    ("_ZTIi", DATA),
    ("_ZTVN10__cxxabiv117__class_type_infoE", DATA),
    ("_ZTVN10__cxxabiv120__si_class_type_infoE", DATA),
    ("_ZTVN10__cxxabiv121__vmi_class_type_infoE", DATA),
    ("_ZTVSt12length_error", DATA),
    ("_ZTVSt9exception", DATA),
    // A *variable* holding a function pointer, not a function: libc calls it
    // after `explicit_bzero` so a debugger can watch. Read through its slot and
    // never called by the guest, which is what makes it data here.
    ("__explicit_bzero_hook", DATA),
    // The personality routine a frame's LSDA names. Read as an address and
    // never called: this runtime interprets the LSDA itself rather than handing
    // it to libunwind, which is the whole of [`crate::unwind`].
    (
        "__gxx_personality_v0",
        "a personality routine, named by an LSDA and never called",
    ),
    ("__mb_cur_max", DATA),
    (
        "__objc_personality_v0",
        "a personality routine, named by an LSDA and never called",
    ),
    ("__stack_chk_guard", DATA),
    ("__stderrp", DATA),
    ("__stdinp", DATA),
    ("__stdoutp", DATA),
    // `dispatch_get_main_queue()` is the *address* of this global rather than
    // a call, so the main queue a guest hands [`crate::shims::block`] is
    // libdispatch's own.
    ("_dispatch_main_q", DATA),
    (
        "dyld_stub_binder",
        "the lazy-binding entry point; every slot is bound eagerly",
    ),
    ("environ", DATA),
    ("getcontext", CONTEXT),
    ("humanize_number", UNDECLARED),
    ("inet_ntoa", AGGREGATE),
    ("kCFAllocatorDefault", DATA),
    ("kCFBooleanTrue", DATA),
    ("kCFBundleIdentifierKey", DATA),
    ("kCFCoreFoundationVersionNumber", DATA),
    ("kCFErrorDomainCFNetwork", DATA),
    ("kCFHTTPAuthenticationAccountDomain", DATA),
    ("kCFHTTPAuthenticationPassword", DATA),
    ("kCFHTTPAuthenticationSchemeBasic", DATA),
    ("kCFHTTPAuthenticationSchemeNTLM", DATA),
    ("kCFHTTPAuthenticationUsername", DATA),
    ("kCFHTTPVersion1_0", DATA),
    ("kCFHTTPVersion1_1", DATA),
    ("kCFNull", DATA),
    ("kCFProxyAutoConfigurationURLKey", DATA),
    ("kCFProxyHostNameKey", DATA),
    ("kCFProxyPortNumberKey", DATA),
    ("kCFProxyTypeHTTP", DATA),
    ("kCFProxyTypeKey", DATA),
    ("kCFProxyTypeSOCKS", DATA),
    ("kCFRunLoopDefaultMode", DATA),
    ("kCFStreamErrorDomainHTTP", DATA),
    ("kCFStreamPropertyHTTPAttemptPersistentConnection", DATA),
    ("kCFStreamPropertyHTTPProxy", DATA),
    ("kCFStreamPropertyHTTPProxyHost", DATA),
    ("kCFStreamPropertyHTTPProxyPort", DATA),
    ("kCFStreamPropertyHTTPRequestBytesWrittenCount", DATA),
    ("kCFStreamPropertyHTTPResponseHeader", DATA),
    ("kCFStreamPropertyHTTPSProxyHost", DATA),
    ("kCFStreamPropertyHTTPSProxyPort", DATA),
    ("kCFStreamPropertySOCKSProxy", DATA),
    ("kCFStreamPropertySOCKSProxyHost", DATA),
    ("kCFStreamPropertySOCKSProxyPort", DATA),
    ("kCFStreamPropertySSLSettings", DATA),
    ("kCFStreamSSLCertificates", DATA),
    ("kCFStreamSSLPeerName", DATA),
    ("kCFStreamSSLValidatesCertificateChain", DATA),
    ("kCFTypeArrayCallBacks", DATA),
    ("kUTTagClassFilenameExtension", DATA),
    ("kUTTagClassMIMEType", DATA),
    ("ldiv", AGGREGATE),
    ("makecontext", CONTEXT),
    ("mbr_identifier_translate", UNDECLARED),
    ("optarg", DATA),
    ("opterr", DATA),
    ("optind", DATA),
    ("optopt", DATA),
    (
        "pthread_cancel",
        "cancellation unwinds a stack the host's unwinder never saw (M5)",
    ),
    ("setcontext", CONTEXT),
    // ## `long double` is not one type
    //
    // The only family in this table where the two architectures disagree about
    // an *ordinary scalar*. On x86-64 macOS a `long double` is the x87 80-bit
    // extended format, sixteen bytes of storage with eleven of them meaningful,
    // passed on the stack and returned in `st(0)`. On arm64 it is IEEE
    // binary128, passed and returned in a `v` register. Neither is a widening of
    // the other: the exponent ranges differ, the significands differ, and there
    // is no register the value can simply be moved between.
    //
    // Narrowing through `double` would answer most calls plausibly and lose the
    // eleven bits that are the entire reason a program asked for the type. So
    // these are refused rather than approximated, which is the same choice
    // [`crate::shims::math`] makes in the other direction for the cases it
    // *can* compute exactly. Roblox reaches them through its C library's
    // number formatting; nothing it does depends on the extra precision, which
    // is a fact about Roblox rather than about the conversion.
    ("powl", LONG_DOUBLE),
    ("fmal", LONG_DOUBLE),
    ("ldexpl", LONG_DOUBLE),
    ("frexpl", LONG_DOUBLE),
    ("strtold", LONG_DOUBLE),
    // `pid_t audit_token_to_pid(audit_token_t)` — thirty-two bytes of
    // `audit_token_t` **by value**. SysV AMD64 classifies it MEMORY and puts it
    // on the caller's stack; AAPCS64 has the caller copy it and pass a pointer.
    // Both are "in memory" and they are still not the same thing, so this is
    // the aggregate refusal rather than a signature that happens to work.
    ("audit_token_to_pid", AGGREGATE),
    // `void IODispatchCalloutFromMessage(void *unused, mach_msg_header_t *msg,
    // void *reference)` — the IOKit notification pump. Its whole job is to read
    // a function pointer *out of the message* and call it, and that pointer is
    // whatever the guest registered with `IOServiceAddInterestNotification`. The
    // host would call it as arm64. This is `CFRunLoopSourceCreate`'s problem
    // reached through a message instead of a structure, and it needs the same
    // answer: a callback the substitution can find.
    (
        "IODispatchCalloutFromMessage",
        "calls a guest function pointer carried inside a Mach message the host would read",
    ),
    // `void malloc_zone_register(malloc_zone_t *)` — hands libsystem_malloc a
    // structure of **guest** function pointers and asks it to allocate through
    // them. Every other call in the zone family goes the safe way round: the
    // zone is the host's and the guest is calling into it. This one is the
    // interposing allocator offering *itself* to the host, and the roadmap's
    // M7.17 records that mimalloc reaches the guest's allocator through
    // `__DATA,__interpose` instead — which is the path that works and the reason
    // nothing has needed this yet.
    (
        "malloc_zone_register",
        "registers a zone of guest function pointers the host would call as arm64",
    ),
    // `kern_return_t malloc_get_all_zones(task_t, memory_reader_t *,
    // vm_address_t **, unsigned *)` — the same shape once removed. The reader is
    // guest code the host calls back into for every zone it walks, and the
    // addresses it answers with describe *another task's* address space. A
    // thunk would carry the call; nothing would make the answers mean anything
    // on this side.
    (
        "malloc_get_all_zones",
        "walks another task's zones through a guest reader callback",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every signature in the table can actually be called.
    ///
    /// The table and [`super::marshal`] are written independently — one says
    /// what a function's arguments are, the other says which shapes it can
    /// place — and nothing forces them to agree. A signature the marshaller
    /// refuses is not a compile error and not a wrong answer: it is a call that
    /// fails by name at run time, months later, in whichever program first
    /// reaches it.
    ///
    /// `UCKeyTranslate` is why this exists. Ten integer arguments, two of them
    /// past AAPCS64's eighth, is the first entry to use more than one
    /// stack slot, and whether it is marshallable turns on Apple's packing rule
    /// — both overflowing arguments happen to be pointers, and a `UInt32` in
    /// either position would silently have to be refused instead.
    #[test]
    fn every_signature_is_one_the_marshaller_can_place() {
        let refused: Vec<&str> = SIGNATURES
            .iter()
            .filter(|abi| !abi.variadic && !crate::abi::marshal::is_marshallable(abi))
            .map(|abi| abi.name)
            .collect();
        assert!(
            refused.is_empty(),
            "described but not callable: {refused:?}"
        );
    }

    /// A name appears once, which is the one thing [`index`] cannot notice.
    ///
    /// Order stopped mattering when the lookup became a map; duplication did
    /// not. Two entries for one name means one of them is dead — and the dead
    /// one is whichever a reader is not looking at, so it stays wrong quietly.
    #[test]
    fn signatures_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for abi in SIGNATURES {
            assert!(seen.insert(abi.name), "{} is described twice", abi.name);
        }
    }

    /// Every entry declares the number of parameters the function really has.
    ///
    /// The bug this exists for is invisible by construction. `CFStringGetCString`
    /// had three parameters where the function has four; the missing one was the
    /// *last*, so the host read whatever the guest left in `x3` as the encoding,
    /// answered `false`, and produced an empty string. Nothing faulted, nothing
    /// was refused, and Unity built a plugin path out of the emptiness — see the
    /// roadmap's M7.13. A trailing parameter is exactly the one a reader
    /// transcribing a header drops, and exactly the one nothing notices.
    ///
    /// So the check is not a reading of the headers, it is a *compile* against
    /// them: each entry becomes a call with as many arguments as the table
    /// claims, and the C compiler is asked whether that is the right number.
    /// Anything it does not declare — the Objective-C runtime, GL, private CF —
    /// simply produces no diagnostic and is skipped, which is the honest scope:
    /// this proves what it can reach and says nothing about the rest.
    ///
    /// Skips rather than fails when there is no compiler, as every fixture test
    /// here does.
    #[test]
    fn the_table_declares_the_parameters_these_functions_really_have() {
        // A struct passed by value is *split* across registers by this table —
        // a `CFRange` is two `CFIndex`, a `double complex` is two `double` — so
        // these declare more arguments than the prototype has, deliberately.
        // Listed rather than pattern-matched, so a new one has to be thought
        // about.
        const SPLIT_AGGREGATES: &[&str] = &[
            "CFArrayApplyFunction",
            "CFStringGetBytes",
            "cabs",
            // A `CGPoint` written out as its two `double`s. Invisible to this
            // check until CoreGraphics.h joined [`HEADERS`], which is the point
            // of adding a header: the entries it newly reaches are the ones
            // nothing was verifying.
            "CGDisplayMoveCursorToPoint",
            "CGWarpMouseCursorPosition",
        ];

        let mut source = String::from(HEADERS);
        source.push_str("void probe(void) {\n");
        let mut at = HEADERS.lines().count() + 1;
        let mut lines = std::collections::HashMap::new();
        for abi in SIGNATURES {
            // A `$`-suffixed spelling is not a C identifier, and a variadic's
            // last "parameter" is the ellipsis its shim consumes.
            if abi.name.contains('$') || abi.variadic || SPLIT_AGGREGATES.contains(&abi.name) {
                continue;
            }
            let args = vec!["0"; abi.types.len() - 1].join(",");
            source.push_str(&format!("  {}({});\n", abi.name, args));
            at += 1;
            lines.insert(at, abi.name);
        }
        source.push_str("}\n");

        let dir = std::env::temp_dir().join("rosetta3-arity");
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("skipping: no temp dir");
            return;
        }
        let path = dir.join("probe.c");
        if std::fs::write(&path, &source).is_err() {
            eprintln!("skipping: cannot write the probe");
            return;
        }
        let Ok(out) = std::process::Command::new("cc")
            .args(["-fsyntax-only", "-Wno-everything", "-fno-caret-diagnostics"])
            .arg(&path)
            .output()
        else {
            eprintln!("skipping: no C compiler");
            return;
        };

        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut wrong = Vec::new();
        for line in stderr.lines() {
            let Some(rest) = line.split("probe.c:").nth(1) else {
                continue;
            };
            if !rest.contains("too few arguments") && !rest.contains("too many arguments") {
                continue;
            }
            let Some(number) = rest.split(':').next().and_then(|n| n.parse::<usize>().ok()) else {
                continue;
            };
            if let Some(name) = lines.get(&number) {
                let how = match rest.contains("too few") {
                    true => "too few",
                    false => "too many",
                };
                wrong.push(format!("{name} ({how})"));
            }
        }
        assert!(
            wrong.is_empty(),
            "these entries declare the wrong number of parameters: {}",
            wrong.join(", ")
        );
    }

    /// What the arity probe includes, which is every header this table's C
    /// entries are declared in. A symbol that is not declared by any of them is
    /// skipped rather than guessed at.
    const HEADERS: &str = "\
#include <CoreFoundation/CoreFoundation.h>
#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <CoreAudio/HostTime.h>
#include <CoreGraphics/CoreGraphics.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreServices/CoreServices.h>
#include <CoreText/CoreText.h>
#include <CoreVideo/CoreVideo.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/hid/IOHIDLib.h>
#include <VideoToolbox/VideoToolbox.h>
#include <bsm/libbsm.h>
#include <dirent.h>
#include <dispatch/dispatch.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <malloc/malloc.h>
#include <os/log.h>
#include <servers/bootstrap.h>
#include <sys/xattr.h>
#include <termios.h>
#include <uuid/uuid.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <fts.h>
#include <glob.h>
#include <iconv.h>
#include <langinfo.h>
#include <locale.h>
#include <math.h>
#include <netdb.h>
#include <poll.h>
#include <pthread.h>
#include <pwd.h>
#include <sched.h>
#include <semaphore.h>
#include <setjmp.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>
#include <wchar.h>
#include <wctype.h>
#include <zlib.h>
";

    #[test]
    fn refusals_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in REFUSED {
            assert!(seen.insert(*name), "{name} is refused twice");
        }
    }

    #[test]
    fn a_symbol_is_never_both_known_and_refused() {
        let refused: std::collections::HashSet<&str> =
            REFUSED.iter().map(|(name, _)| *name).collect();
        for abi in SIGNATURES {
            assert!(
                !refused.contains(abi.name),
                "{} is both described and refused",
                abi.name
            );
        }
    }

    #[test]
    fn the_leading_underscore_is_stripped_exactly_once() {
        assert!(matches!(lookup(0, "_strlen"), Lookup::Known(_)));
        assert!(matches!(lookup(0, "___stdoutp"), Lookup::Refused(_)));
        assert!(matches!(lookup(0, "_stdoutp"), Lookup::Unknown));
    }

    #[test]
    fn an_x86_only_variant_falls_back_to_the_plain_symbol() {
        // The same fallback machish applies when resolving the address; arm64
        // never had `$INODE64`, so what is known about the variant is whatever
        // is known about the plain symbol — here, that it takes a comparator.
        assert_eq!(lookup(0, "_fts_open"), lookup(0, "_fts_open$INODE64"));
        // `fts_open` is a signature now that the thunk exists, and the point
        // of this assertion is unchanged: the `$INODE64` spelling finds
        // whatever the plain name has to say.
        assert!(
            matches!(lookup(0, "_fts_open$INODE64"), Lookup::Known(abi) if abi.name == "fts_open")
        );
        assert_eq!(lookup(0, "_strtod$UNIX2003"), lookup(0, "_strtod"));
    }

    #[test]
    fn objc_metadata_is_refused_as_data_rather_than_reported_unknown() {
        // One name per class an image mentions, so this cannot be a list. The
        // distinction that matters is refused-versus-unknown: unknown is the
        // audit's word for a symbol nobody has described, and these are
        // described exactly — they are never called.
        for name in [
            "_OBJC_CLASS_$_NSObject",
            "_OBJC_METACLASS_$_NSObject",
            "_OBJC_IVAR_$_Dog._weight",
            "_OBJC_EHTYPE_id",
            "__objc_empty_cache",
            "___CFConstantStringClassReference",
        ] {
            assert!(matches!(lookup(0, name), Lookup::Refused(_)), "{name}");
        }
        // Not a blanket "anything starting with OBJC": the entry points are
        // functions with signatures, and swallowing one here would hide it.
        assert!(matches!(lookup(0, "_objc_msgSend"), Lookup::Unknown));
    }

    #[test]
    fn a_variadic_carries_its_fixed_arguments_and_the_marker() {
        let Lookup::Known(printf) = lookup(0, "_printf") else {
            panic!("no _printf")
        };
        assert!(printf.variadic);
        assert_eq!(printf.params(), &[T::Pointer, T::VariadicList]);
    }
}
