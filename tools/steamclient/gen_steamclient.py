#!/usr/bin/env python3
"""Generate the guest-facing SteamClient dylib sources.

Rosetta 3 bridges a macOS guest's Steam calls to the host's native Steam
client, the way Proton's lsteamclient bridges a Windows guest's. This script
generates the guest half: a macOS dylib that publishes every historical
Steamworks interface with the vtable layout the guest expects, and forwards
each call to the host through a single dispatch entry point.

Unlike Proton, nothing here converts data. A macOS x86-64 (or arm64) guest and
a Linux host share the Itanium C++ ABI and the same struct layouts, so only the
vtable shape has to be reproduced. Types the bridge does not need to understand
cross as opaque pointers; the eleven types that are passed or returned by value
decide a calling convention and are declared in support/rosetta3_steam.h.

Usage:
    gen_steamclient.py --sdk-root DIR [--out DIR] [--sdk VERSION ...]

DIR holds the unpacked SDKs as steamworks_sdk_<version> subdirectories; a
checkout of Proton's lsteamclient works as-is.
"""

import argparse
import json
import os
import re
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import sdkparse
from sdkparse import ParseError

SUPPORT_FILES = ["rosetta3_steam.h", "steamclient_main.cpp"]

# Guest spelling and size for every scalar that reaches a signature. Sizes are
# LP64, which both a macOS guest and a Linux host agree on.
PRIMITIVES = {
    "void": ("void", 0),
    "bool": ("bool", 1),
    "char": ("char", 1),
    "signed char": ("int8_t", 1),
    "unsigned char": ("uint8_t", 1),
    "short": ("int16_t", 2),
    "short int": ("int16_t", 2),
    "unsigned short": ("uint16_t", 2),
    "unsigned short int": ("uint16_t", 2),
    "int": ("int32_t", 4),
    "signed int": ("int32_t", 4),
    "unsigned": ("uint32_t", 4),
    "unsigned int": ("uint32_t", 4),
    "long": ("int64_t", 8),
    "long int": ("int64_t", 8),
    "unsigned long": ("uint64_t", 8),
    "unsigned long int": ("uint64_t", 8),
    "long long": ("int64_t", 8),
    "long long int": ("int64_t", 8),
    "unsigned long long": ("uint64_t", 8),
    "unsigned long long int": ("uint64_t", 8),
    "__int64": ("int64_t", 8),
    "unsigned __int64": ("uint64_t", 8),
    "float": ("float", 4),
    "double": ("double", 8),
    "size_t": ("size_t", 8),
    "intptr_t": ("intptr_t", 8),
    "uintptr_t": ("uintptr_t", 8),
    "wchar_t": ("int32_t", 4),
}

# Types crossing by value. Their layout is reproduced in rosetta3_steam.h; the
# sizes here let the generator record parameter offsets for the host side.
BY_VALUE_TYPES = {
    "CSteamID": 8,
    "CGameID": 8,
    "SteamIPAddress_t": 24,
    "InputAnalogActionData_t": 16,
    "InputDigitalActionData_t": 2,
    "InputMotionData_t": 40,
    "SteamPartyBeaconLocation_t": 16,
    "RemoteStorageUpdatePublishedFileRequest_t": 64,
}

# Older SDKs spell three of them with a Controller prefix; the newer headers
# #define the old names to the new ones, so both resolve to one layout.
BY_VALUE_ALIASES = {
    "ControllerAnalogActionData_t": "InputAnalogActionData_t",
    "ControllerDigitalActionData_t": "InputDigitalActionData_t",
    "ControllerMotionData_t": "InputMotionData_t",
}

POINTER_SIZE = 8

# How a value reaches the host's x86-64 SysV callee. The guest side never needs
# this - the compiler classifies for it - but the host side calls through a
# pointer it only learns at runtime, so the classification has to be recorded
# rather than recomputed from a type it no longer has.
#
#   INTEGER  one general-purpose register, `size` bytes wide
#   SSE      one vector register, 4 or 8 bytes
#   INTEGER2 two general-purpose registers, for a 16-byte aggregate
#   MEMORY   passed as a stack copy, returned through a hidden pointer
CLASS_VOID = 0
CLASS_INTEGER = 1
CLASS_SSE = 2
CLASS_INTEGER2 = 3
CLASS_MEMORY = 4

# The by-value types, classified. SysV decides this from the field types of
# each eightbyte, and for these eight the answer is fixed: an eightbyte holding
# any integer field is INTEGER, and anything over 16 bytes is MEMORY.
BY_VALUE_CLASSES = {
    "CSteamID": CLASS_INTEGER,
    "CGameID": CLASS_INTEGER,
    # 24 bytes.
    "SteamIPAddress_t": CLASS_MEMORY,
    # {int32, float, float, bool}: both eightbytes hold an integer field.
    "InputAnalogActionData_t": CLASS_INTEGER2,
    "InputDigitalActionData_t": CLASS_INTEGER,
    # 40 bytes.
    "InputMotionData_t": CLASS_MEMORY,
    "SteamPartyBeaconLocation_t": CLASS_INTEGER2,
    # 64 bytes.
    "RemoteStorageUpdatePublishedFileRequest_t": CLASS_MEMORY,
}


# Non-method dispatch points, in the order the generated enum assigns them.
BUILTIN_DISPATCH = [
    "init",
    "shutdown",
    "create_interface",
    "is_known_interface",
    "notify_missing_interface",
    "get_callback",
    "free_last_callback",
    "get_api_call_result",
    "release_thread_local_memory",
    "breakpad_minidump_init",
    "breakpad_set_appid",
    "breakpad_set_steamid",
    "breakpad_write_minidump_set_comment",
    "breakpad_write_minidump_with_build_id",
]


class Type:
    """A parameter or return type, resolved to something the bridge can emit."""

    def __init__(self, kind, spelling, size, source):
        self.kind = kind          # scalar | pointer | byvalue | void
        self.spelling = spelling  # what the generated C++ says
        self.size = size
        self.source = source      # the SDK's own spelling, for the manifest

    def declare(self, name):
        """A declaration of `name` with this type, spaced like C++."""
        return f"{self.spelling}{'' if self.spelling.endswith('*') else ' '}{name}"

    @property
    def abi_class(self):
        if self.kind == "void":
            return CLASS_VOID
        if self.kind == "byvalue":
            return BY_VALUE_CLASSES[self.spelling]
        if self.spelling in ("float", "double"):
            return CLASS_SSE
        return CLASS_INTEGER

    def as_json(self):
        return {"kind": self.kind, "type": self.spelling, "size": self.size,
                "abi_class": self.abi_class, "sdk_type": self.source}


VOID = Type("void", "void", 0, "void")


def resolve_type(sdk, spelling):
    """Map an SDK type onto the guest-facing type with the same ABI."""
    source = re.sub(r'\s+', ' ', spelling).strip()
    text = source

    for _ in range(16):
        if '*' in text or '&' in text:
            return Type("pointer", "void *", POINTER_SIZE, source)
        text = re.sub(r'\b(const|volatile|struct|class|enum|union)\b', ' ', text)
        text = re.sub(r'\s+', ' ', text).strip()
        if text in PRIMITIVES:
            spelled, size = PRIMITIVES[text]
            if spelled == "void":
                return VOID
            return Type("scalar", spelled, size, source)
        if text == sdkparse.FUNCTION_POINTER:
            return Type("pointer", "void *", POINTER_SIZE, source)
        if text in sdk.enums:
            # A C++ enum with no fixed underlying type is int-sized either way.
            return Type("scalar", "int32_t", 4, source)
        resolved = BY_VALUE_ALIASES.get(text, text)
        if resolved in BY_VALUE_TYPES:
            return Type("byvalue", resolved, BY_VALUE_TYPES[resolved], source)
        if text in sdk.typedefs:
            text = sdk.typedefs[text]
            continue
        break

    raise ParseError(
        f"SDK {sdk.version}: no ABI mapping for type {source!r}. Add it to "
        f"BY_VALUE_TYPES (with a layout in support/rosetta3_steam.h) if it "
        f"crosses by value.")


class Arg:
    def __init__(self, name, type, offset):
        self.name = name
        self.type = type
        self.offset = offset


class BoundMethod:
    """A parsed method with its types resolved and its params struct laid out."""

    def __init__(self, sdk, iface, method, index, unique_name, cpp_name=None):
        self.name = method.name
        self.cpp_name = cpp_name or method.name
        self.unique_name = unique_name
        self.index = index
        self.is_destructor = method.is_destructor
        if self.is_destructor:
            self.ret, self.args, self.params_size = VOID, [], 0
            self.iface_version = None
            return

        self.ret = resolve_type(sdk, method.ret)
        # Params are packed: interface handle, return slot, then arguments.
        offset = POINTER_SIZE
        if self.ret.kind != "void":
            offset += self.ret.size
        self.ret_offset = POINTER_SIZE
        self.args = []
        for i, param in enumerate(method.params):
            type = resolve_type(sdk, param.type)
            if type.kind == "void":
                raise ParseError(f"{iface.full_name}.{self.name}: void parameter")
            self.args.append(Arg(param.name or f"_arg{i}", type, offset))
            offset += type.size
        self.params_size = offset
        self.iface_version = self.returned_interface_version(iface)

    def returned_interface_version(self, iface):
        """The version string a returned interface pointer must be wrapped as.

        A method handing back another interface hands back a pointer into the
        *host's* client. Returning it unchanged would leave the guest making
        vtable calls on host code, which is the one thing this bridge exists to
        prevent - so the pointer is wrapped in a guest object of the version the
        caller asked for, the way Proton's `create_win_interface` wraps it.
        """
        if self.ret.kind != "pointer":
            return None
        if not (self.ret.source.startswith("ISteam") or "GetISteam" in self.name):
            return None
        # The one interface returned by a method that names no version: there
        # has only ever been one, and Proton hardcodes it here too.
        if self.name.startswith("CreateFakeUDPPort"):
            return '"SteamNetworkingFakeUDPPort001"'
        for arg in self.args:
            if arg.name == "pchVersion":
                return arg.name
        raise ParseError(
            f"{iface.full_name}.{self.name} returns {self.ret.source} but names "
            f"no interface version, so the pointer cannot be wrapped")

    def signature(self, class_name=None):
        args = ', '.join(a.type.declare(a.name) for a in self.args)
        scope = f"{class_name}::" if class_name else ""
        return f"{self.ret.spelling} {scope}{self.cpp_name}({args})"

    def as_json(self, id):
        return {
            "id": id,
            "name": self.name,
            "cpp_name": self.cpp_name,
            "unique_name": self.unique_name,
            "vtable_index": self.index,
            "params_struct": f"steam_params_{self.unique_name}",
            "params_size": self.params_size,
            "iface_offset": 0,
            "returns_interface": self.iface_version is not None,
            "ret": None if self.ret.kind == "void" else
                   dict(self.ret.as_json(), offset=self.ret_offset),
            "params": [dict(a.type.as_json(), name=a.name, offset=a.offset)
                       for a in self.args],
        }


class BoundInterface:
    def __init__(self, sdk, iface):
        self.name = iface.name
        self.version = iface.version
        self.sdkver = iface.sdkver
        self.methods = []
        seen = {}
        for index, method in enumerate(iface.methods):
            if method.is_destructor:
                self.methods.append(BoundMethod(sdk, iface, method, index, None))
                continue
            # Overloads that differ only in a pointee type collapse to the same
            # signature once pointers become void *, so later ones are renamed.
            # The vtable is indexed, not looked up by name, and the SDK name is
            # kept in the manifest.
            seen[method.name] = seen.get(method.name, 0) + 1
            suffix = '' if seen[method.name] == 1 else f"_{seen[method.name]}"
            unique = f"{self.name}_{self.version}_{method.name}{suffix}"
            self.methods.append(BoundMethod(sdk, iface, method, index, unique,
                                            method.name + suffix))

    @property
    def class_name(self):
        return f"{self.name}_{self.version}"


def collect(sdk_root, only=None):
    """Bind every interface version, newest SDK defining it winning."""
    sdks = sdkparse.discover_sdks(sdk_root)
    if only:
        wanted = set(only)
        sdks = [s for s in sdks if s[0] in wanted]
        missing = wanted - {s[0] for s in sdks}
        if missing:
            raise ParseError(f"no such SDK under {sdk_root}: {sorted(missing)}")
    if not sdks:
        raise ParseError(f"no steamworks_sdk_* directories under {sdk_root}")

    interfaces = {}
    for version, path in sdks:
        sdk = sdkparse.load_sdk(version, path)
        for iface in sdk.interfaces.values():
            interfaces[iface.version] = BoundInterface(sdk, iface)
    return [v for _, v in sorted(interfaces.items())], [s[0] for s in sdks]


HEADER = "/* Generated by tools/steamclient/gen_steamclient.py. Do not edit. */\n"


def write_methods_header(out, interfaces):
    ids = {}
    with open(os.path.join(out, "steamclient_methods.h"), "w") as f:
        f.write(HEADER)
        f.write("\n#ifndef ROSETTA3_STEAMCLIENT_METHODS_H\n")
        f.write("#define ROSETTA3_STEAMCLIENT_METHODS_H\n\n")
        f.write("enum steam_dispatch_method\n{\n")
        next_id = 0
        for name in BUILTIN_DISPATCH:
            f.write(f"    STEAM_DISPATCH_{name.upper()} = {next_id},\n")
            next_id += 1
        f.write("\n")
        for iface in interfaces:
            for method in iface.methods:
                if method.is_destructor:
                    continue
                ids[method.unique_name] = next_id
                f.write(f"    STEAM_METHOD_{method.unique_name} = {next_id},\n")
                next_id += 1
        f.write(f"\n    STEAM_DISPATCH_METHOD_COUNT = {next_id},\n")
        f.write("};\n\n#endif\n")
    return ids


def write_interface_source(out, name, interfaces, ids):
    path = os.path.join(out, f"iface_{name}.cpp")
    with open(path, "w") as f:
        f.write(HEADER)
        f.write('\n#include "rosetta3_steam.h"\n')
        f.write('#include "steamclient_methods.h"\n\n')

        for iface in interfaces:
            klass = iface.class_name
            f.write(f"/* {iface.name} {iface.version} (Steamworks SDK "
                    f"{iface.sdkver}) */\n\n")

            f.write("#pragma pack(push, 1)\n")
            for method in iface.methods:
                if method.is_destructor:
                    continue
                f.write(f"struct steam_params_{method.unique_name}\n{{\n")
                f.write("    void *iface;\n")
                if method.ret.kind != "void":
                    f.write(f"    {method.ret.declare('_ret')};\n")
                for arg in method.args:
                    f.write(f"    {arg.type.declare(arg.name)};\n")
                f.write("};\n")
            f.write("#pragma pack(pop)\n\n")

            f.write(f"struct {klass} : steam_iface\n{{\n")
            for method in iface.methods:
                if method.is_destructor:
                    f.write(f"    virtual ~{klass}() {{}}\n")
                    continue
                f.write(f"    virtual {method.signature()}\n    {{\n")
                f.write(f"        steam_params_{method.unique_name} p = {{}};\n")
                f.write("        p.iface = this->u_iface;\n")
                for arg in method.args:
                    f.write(f"        p.{arg.name} = {arg.name};\n")
                f.write(f"        rosetta3_steamclient_dispatch("
                        f"STEAM_METHOD_{method.unique_name}, &p);\n")
                if method.iface_version:
                    f.write(f"        return steam_wrap_interface("
                            f"{method.iface_version}, p._ret);\n")
                elif method.ret.kind != "void":
                    f.write("        return p._ret;\n")
                f.write("    }\n")
            f.write("};\n\n")

            f.write(f"static_assert(sizeof({klass}) <= STEAM_IFACE_STORAGE,\n"
                    f"              \"{klass} does not fit the interface pool\");\n\n")
            f.write(f"void *steam_create_{klass}(void *storage, void *u_iface)\n{{\n")
            f.write(f"    {klass} *iface = new (storage) {klass}();\n")
            f.write("    iface->u_iface = u_iface;\n")
            f.write("    return iface;\n}\n\n")


def write_registry(out, interfaces):
    with open(os.path.join(out, "steamclient_registry.cpp"), "w") as f:
        f.write(HEADER)
        f.write('\n#include "rosetta3_steam.h"\n\n')
        for iface in interfaces:
            f.write(f"extern void *steam_create_{iface.class_name}"
                    f"(void *storage, void *u_iface);\n")
        f.write("\nconst struct steam_iface_entry steam_iface_table[] =\n{\n")
        for iface in interfaces:
            f.write(f'    {{"{iface.version}", &steam_create_{iface.class_name}}},\n')
        f.write("};\n\n")
        f.write("const unsigned int steam_iface_table_size =\n"
                "    sizeof(steam_iface_table) / sizeof(steam_iface_table[0]);\n")


# The compact table the host half reads at run time, so that the two sides
# cannot disagree about a vtable index or a parameter offset: both come out of
# this one generation.
#
# JSON would do the same job, but it is five megabytes of it and the runtime has
# no JSON reader; this is one sequential walk with no allocation per method.
#
#   magic "R3SC", u32 version, u32 method_count
#   then one record per dispatch id, in id order:
#     u16 vtable_index   0xffff for an id that is not an interface method
#     u16 iface_offset
#     u16 ret_offset
#     u8  ret_class      0 for void
#     u8  ret_size
#     u8  param_count
#     u8  reserved
#     param_count * { u8 class, u8 size, u16 offset }
DISPATCH_TABLE_VERSION = 1
NOT_A_METHOD = 0xffff


def write_dispatch_table(out, interfaces, ids):
    import struct

    by_id = {}
    for iface in interfaces:
        for method in iface.methods:
            if not method.is_destructor:
                by_id[ids[method.unique_name]] = method

    count = len(BUILTIN_DISPATCH) + len(by_id)
    if sorted(by_id) != list(range(len(BUILTIN_DISPATCH), count)):
        raise ParseError("dispatch ids are not the contiguous range after the builtins")

    body = bytearray()
    for id in range(count):
        method = by_id.get(id)
        if method is None:
            body += struct.pack("<HHHBBBB", NOT_A_METHOD, 0, 0, CLASS_VOID, 0, 0, 0)
            continue
        ret = method.ret
        void = ret.kind == "void"
        body += struct.pack(
            "<HHHBBBB",
            method.index, 0, 0 if void else method.ret_offset,
            ret.abi_class, ret.size, len(method.args), 0)
        for arg in method.args:
            body += struct.pack("<BBH", arg.type.abi_class, arg.type.size, arg.offset)

    with open(os.path.join(out, "steamclient_dispatch.bin"), "wb") as f:
        f.write(b"R3SC")
        f.write(struct.pack("<II", DISPATCH_TABLE_VERSION, count))
        f.write(body)


def write_manifest(out, interfaces, ids, sdk_versions, sources):
    manifest = {
        "generator": "tools/steamclient/gen_steamclient.py",
        "abi": "itanium-lp64",
        "dispatch_table_version": DISPATCH_TABLE_VERSION,
        "sdk_versions": sdk_versions,
        "builtin_dispatch": {name: i for i, name in enumerate(BUILTIN_DISPATCH)},
        "by_value_types": BY_VALUE_TYPES,
        "sources": sources,
        "interfaces": [
            {
                "name": iface.name,
                "version": iface.version,
                "sdk_version": iface.sdkver,
                "class": iface.class_name,
                "methods": [m.as_json(ids[m.unique_name])
                            for m in iface.methods if not m.is_destructor],
                "has_virtual_destructor":
                    any(m.is_destructor for m in iface.methods),
            }
            for iface in interfaces
        ],
    }
    with open(os.path.join(out, "steamclient_manifest.json"), "w") as f:
        json.dump(manifest, f, indent=1)
        f.write("\n")


BUILD_SH = r'''#!/bin/sh
# Generated by tools/steamclient/gen_steamclient.py. Do not edit.
#
# Builds the guest-facing SteamClient dylib. Nothing here needs a macOS SDK:
# the sources are freestanding, and the one symbol they leave undefined,
# rosetta3_steamclient_dispatch, is resolved by Rosetta 3 when the guest loads
# the dylib.
set -eu

CLANG="${CLANG:-clang++}"
# LD64, not LD: build environments commonly set LD to the host linker.
LD64="${LD64:-ld64.lld}"
LIPO="${LIPO:-llvm-lipo}"
ARCHS="${ARCHS:-%(archs)s}"
OUT="${OUT:-steamclient.dylib}"
OBJDIR="${OBJDIR:-obj}"
JOBS="${JOBS:-%(jobs)s}"

CXXFLAGS="-std=c++17 -Os -fno-exceptions -fno-rtti -fno-threadsafe-statics"
CXXFLAGS="$CXXFLAGS -ffreestanding -nostdinc -nostdinc++ -fvisibility=hidden"
CXXFLAGS="$CXXFLAGS -fno-stack-protector -Wall -Wno-unused-parameter"

SOURCES="%(sources)s"

slices=""
for arch in $ARCHS; do
    case "$arch" in
        arm64) version=11.0 ;;
        *) version=10.13 ;;
    esac
    dir="$OBJDIR/$arch"
    rm -rf "$dir"
    mkdir -p "$dir"

    printf 'compiling %%s slice\n' "$arch"
    running=0
    for src in $SOURCES; do
        {
            "$CLANG" -target "$arch-apple-macos$version" $CXXFLAGS \
                -c "$src" -o "$dir/${src%%.cpp}.o" || echo "$src" >> "$dir/failed"
        } &
        running=$((running + 1))
        if [ "$running" -ge "$JOBS" ]; then wait; running=0; fi
    done
    wait
    if [ -s "$dir/failed" ]; then
        printf 'failed to compile:\n' >&2
        cat "$dir/failed" >&2
        exit 1
    fi

    printf 'linking %%s slice\n' "$arch"
    # -undefined dynamic_lookup: the dispatch symbol comes from the host.
    "$LD64" -dylib -arch "$arch" -platform_version macos "$version" "$version" \
        -install_name "@rpath/steamclient.dylib" \
        -undefined dynamic_lookup -dead_strip \
        -o "$dir/steamclient.dylib" "$dir"/*.o
    slices="$slices $dir/steamclient.dylib"
done

# shellcheck disable=SC2086
set -- $slices
if [ "$#" -gt 1 ]; then
    "$LIPO" -create -output "$OUT" "$@"
else
    cp "$1" "$OUT"
fi
printf 'built %%s (%%s)\n' "$OUT" "$ARCHS"
'''


def write_build_script(out, sources, archs):
    path = os.path.join(out, "build.sh")
    with open(path, "w") as f:
        f.write(BUILD_SH % {
            "sources": ' '.join(sources),
            "archs": ' '.join(archs),
            "jobs": os.cpu_count() or 4,
        })
    os.chmod(path, 0o755)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sdk-root", required=True,
                        help="directory holding steamworks_sdk_<version> trees")
    parser.add_argument("--out", default=os.path.join(here, "generated"),
                        help="output directory (default: tools/steamclient/generated)")
    parser.add_argument("--sdk", action="append", dest="sdks", metavar="VERSION",
                        help="limit to this SDK version (repeatable)")
    parser.add_argument("--arch", action="append", dest="archs", metavar="ARCH",
                        help="slice to build, default x86_64 and arm64")
    args = parser.parse_args()

    interfaces, sdk_versions = collect(args.sdk_root, args.sdks)

    os.makedirs(args.out, exist_ok=True)
    for name in SUPPORT_FILES:
        shutil.copyfile(os.path.join(here, "support", name),
                        os.path.join(args.out, name))

    ids = write_methods_header(args.out, interfaces)

    by_class = {}
    for iface in interfaces:
        by_class.setdefault(iface.name, []).append(iface)
    for name, group in sorted(by_class.items()):
        write_interface_source(args.out, name, group, ids)

    write_registry(args.out, interfaces)

    sources = ["steamclient_main.cpp", "steamclient_registry.cpp"]
    sources += [f"iface_{name}.cpp" for name in sorted(by_class)]
    write_manifest(args.out, interfaces, ids, sdk_versions, sources)
    write_dispatch_table(args.out, interfaces, ids)
    write_build_script(args.out, sources, args.archs or ["x86_64", "arm64"])

    methods = sum(len(i.methods) for i in interfaces)
    print(f"{len(sdk_versions)} SDKs -> {len(interfaces)} interface versions, "
          f"{methods} methods, {len(sources)} sources in {args.out}")


if __name__ == "__main__":
    main()
