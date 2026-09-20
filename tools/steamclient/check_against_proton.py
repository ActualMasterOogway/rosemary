#!/usr/bin/env python3
"""Check the generated bridge against Proton's lsteamclient.

Proton generates the same interfaces with libclang, so its checked-in output
is an independent reading of the same headers. This compares the two, method
by method, and reports anything that is not one of the two differences that
are expected by construction:

  * Proton gives a non-virtual destructor a vtable slot (harmless under the
    MSVC ABI it targets); the Itanium ABI gives it none.
  * MSVC emits same-name overloads in reverse declaration order, so Proton
    reorders them; Itanium keeps declaration order.

Usage:
    check_against_proton.py --proton DIR [--manifest FILE]

DIR is a Proton checkout's lsteamclient directory. With no --manifest the
interfaces are parsed straight from the SDKs under DIR.
"""

import argparse
import glob
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import gen_steamclient

PARAMS_STRUCT = re.compile(
    r'^struct (?P<name>(?!wow64_)\w+)_params\n\{\n(?P<body>.*?)\n\};', re.M | re.S)


def load_proton_vtables(proton):
    """version -> [method name per vtable slot], from the generated win sources."""
    vtables = {}
    for path in glob.glob(os.path.join(proton, "winISteam*.c")):
        text = open(path, errors='replace').read()
        for m in re.finditer(r'__ASM_VTABLE\(win(ISteam\w+)_(\S+),\n(.*?)\s*\);',
                             text, re.S):
            names = re.findall(r'VTABLE_ADD_FUNC\(win\w+?_%s_(\w+)\)' % re.escape(m[2]),
                               m[3])
            vtables[m[2]] = names
    return vtables


def load_proton_params(proton):
    """Proton's params struct name -> [field names], minus u_iface and _ret."""
    path = os.path.join(proton, "unixlib_generated.h")
    params = {}
    for m in PARAMS_STRUCT.finditer(open(path, errors='replace').read()):
        fields = []
        for line in m['body'].split('\n'):
            # Function- and array-pointer fields name themselves inside the
            # declarator: "void (*f)(int)", "char (*errMsg)[1024]".
            field = re.match(r'\s*.*?\(\s*\*\s*(?:\w+\s+)?(\w+)\s*\)\s*[(\[]', line)
            if not field:
                field = re.match(r'\s*[^;]*?(\w+)\s*(\[[^\]]*\])?\s*;\s*$', line)
            # _str is a Proton-side scratch field for string out-parameters.
            if field and field[1] not in ('u_iface', '_ret', '_str'):
                fields.append(field[1])
        params[m['name']] = fields
    return params


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--proton", required=True,
                        help="a Proton checkout's lsteamclient directory")
    parser.add_argument("--manifest",
                        help="steamclient_manifest.json (default: parse the SDKs)")
    args = parser.parse_args()

    if args.manifest:
        manifest = json.load(open(args.manifest))
        ours = {i["version"]: i for i in manifest["interfaces"]}
    else:
        interfaces, _ = gen_steamclient.collect(args.proton)
        ours = {i.version: {
            "version": i.version,
            "methods": [{"name": m.name, "params": [{"name": a.name} for a in m.args]}
                        for m in i.methods if not m.is_destructor],
        } for i in interfaces}

    vtables = load_proton_vtables(args.proton)
    proton_params = load_proton_params(args.proton)

    shared = sorted(set(ours) & set(vtables))
    print(f"{len(ours)} interface versions generated, "
          f"{len(vtables)} in Proton, {len(shared)} shared")

    problems = 0
    checked = 0
    for version in shared:
        mine = [m["name"] for m in ours[version]["methods"]]
        # Drop Proton's destructor slot and its overload renaming before
        # comparing: both are MSVC ABI artefacts, documented above.
        theirs = [re.sub(r'_\d+$', '', n) for n in vtables[version]
                  if not n.startswith('destructor')]
        if sorted(mine) != sorted(theirs):
            only_mine = sorted(set(mine) - set(theirs))
            only_theirs = sorted(set(theirs) - set(mine))
            print(f"  {version}: method sets differ "
                  f"(only here: {only_mine}, only Proton: {only_theirs})")
            problems += 1
            continue
        if mine != theirs:
            # Same methods, different order: only same-name overloads may move,
            # and MSVC can move one clear across the class. Everything else has
            # to line up once the overloaded names are taken out.
            movers = {n for n in mine if mine.count(n) > 1}
            if ([n for n in mine if n not in movers] !=
                    [n for n in theirs if n not in movers]):
                print(f"  {version}: vtable order differs beyond overloads")
                problems += 1
                continue

        for method in ours[version]["methods"]:
            theirs_params = proton_params.get(method.get("unique_name") or "")
            if theirs_params is None:
                continue
            checked += 1
            # Both sides invent a name for unnamed parameters; only the
            # position matters, so those compare equal.
            def names(fields):
                return ['_' if re.match(r'^_(arg\d+|[a-z])$', f) else f
                        for f in fields]

            mine_params = [p["name"] for p in method["params"]]
            if names(mine_params) != names(theirs_params):
                print(f"  {version}.{method['name']}: parameters differ\n"
                      f"      here:   {mine_params}\n"
                      f"      Proton: {theirs_params}")
                problems += 1

    print(f"{checked} method signatures compared, {problems} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
