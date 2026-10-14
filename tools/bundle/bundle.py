#!/usr/bin/env python3
"""Vendor a Nix-built Rosemary's libraries into a relocatable tree.

Starts from the real binary and follows its libraries two ways:

* DT_NEEDED, which the dynamic loader resolves at start-up, and
* names the code *might* `dlopen` later. Most of Rosemary's dependencies are
  opened that way, so they are invisible to DT_NEEDED. Those are found by
  scanning each ELF for `libfoo.so.N` strings and resolving them the way the
  Nix build intended: through the file's own RUNPATH, which nixpkgs fills in
  with exactly the libraries a package dlopens.

Each name is then classified (see nix/bundle/policy.nix): glibc's own, left to
the host, or vendored. Vendored files are copied to `lib/`, their RUNPATH
rewritten to `$ORIGIN`, so no Nix store path is needed at run time. A report,
`share/doc/rosemary/SOURCES.yaml`, says where every file came from.

Executables can be vendored too (`executables`), such as WebKitGTK's helper
processes: they get the host's loader as their interpreter and a RUNPATH that
reaches `lib/`. Libraries that are loaded by path, not soname, such as GIO
modules, go where `extra_libraries` says.

Usage: bundle.py --config CONFIG.json --out TREE
"""

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import sys
from collections import defaultdict

STORE = "/nix/store"

SHT_DYNAMIC = 6
SHT_GNU_VERNEED = 0x6FFFFFFE
PT_INTERP = 3
DT_NEEDED, DT_SONAME, DT_RPATH, DT_RUNPATH = 1, 14, 15, 29

# A soname inside a binary. Rust string literals are packed with no separator,
# so this must stop where the next `lib` starts rather than at a delimiter.
SONAME = re.compile(rb"lib[A-Za-z0-9_+-]+(?:\.[0-9]+)*\.so(?:\.[0-9]+)*")
STORE_REF = re.compile(rb"/nix/store/[0-9a-z]{32}-[A-Za-z0-9._+?=-]+")
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|notice|copyright|authors)", re.I)

NIX32 = "0123456789abcdfghijklmnpqrsvwxyz"


class Elf:
    """The little of an ELF64 file this needs."""

    def __init__(self, path):
        self.path = path
        with open(path, "rb") as f:
            self.data = f.read()
        d = self.data
        self.valid = d[:4] == b"\x7fELF" and d[4:6] == b"\x02\x01"
        self.needed, self.rpath, self.soname = [], [], None
        self.interpreter = None
        self.verneed = []
        if self.valid:
            self._parse()

    def _sections(self):
        d = self.data
        (shoff,) = struct.unpack_from("<Q", d, 0x28)
        entsize, count = struct.unpack_from("<HH", d, 0x3A)
        for i in range(count):
            yield struct.unpack_from("<IIQQQQIIQQ", d, shoff + i * entsize)

    def _string(self, table, at):
        end = self.data.index(b"\0", table + at)
        return self.data[table + at : end].decode()

    def _parse(self):
        d = self.data
        sections = list(self._sections())
        for _, kind, _, _, offset, size, link, info, _, _ in sections:
            strings = sections[link][4] if link < len(sections) else 0
            if kind == SHT_DYNAMIC:
                for tag, value in struct.iter_unpack("<qQ", d[offset : offset + size]):
                    if tag == 0:
                        break
                    if tag == DT_NEEDED:
                        self.needed.append(self._string(strings, value))
                    elif tag == DT_SONAME:
                        self.soname = self._string(strings, value)
                    elif tag in (DT_RPATH, DT_RUNPATH):
                        self.rpath += self._string(strings, value).split(":")
            elif kind == SHT_GNU_VERNEED:
                at = offset
                for _ in range(info):
                    _, cnt, _, aux, nxt = struct.unpack_from("<HHIII", d, at)
                    a = at + aux
                    for _ in range(cnt):
                        _, _, _, name, anext = struct.unpack_from("<IHHII", d, a)
                        self.verneed.append(self._string(strings, name))
                        a += anext
                    at += nxt
        (phoff,) = struct.unpack_from("<Q", d, 0x20)
        phentsize, phnum = struct.unpack_from("<HH", d, 0x36)
        for i in range(phnum):
            kind, _, offset, _, _, filesz = struct.unpack_from(
                "<IIQQQQ", d, phoff + i * phentsize
            )
            if kind == PT_INTERP:
                self.interpreter = d[offset : offset + filesz].rstrip(b"\0").decode()

    def search_dirs(self):
        origin = os.path.dirname(self.path)
        return [p.replace("$ORIGIN", origin).replace("${ORIGIN}", origin) for p in self.rpath if p]

    def dlopen_names(self):
        return sorted({m.group().decode() for m in SONAME.finditer(self.data)})

    def glibc_requirement(self):
        found = []
        for name in self.verneed:
            m = re.fullmatch(r"GLIBC_(\d+)\.(\d+)(?:\.\d+)?", name)
            if m:
                found.append((int(m[1]), int(m[2])))
        return max(found, default=None)


def fmt_version(v):
    return f"{v[0]}.{v[1]}" if v else None


def matches(name, patterns):
    return any(fnmatch.fnmatchcase(name, p) for p in patterns)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def store_root(path):
    """/nix/store/<hash>-<name> for any path inside it, else None."""
    if not path.startswith(STORE + "/"):
        return None
    return "/".join(path.split("/")[:4])


def nix32_to_sri(text):
    """`sha256:<nix base32>` as registration files write it, to `sha256-<base64>`."""
    algo, _, digits = text.partition(":")
    value = 0
    for ch in digits:
        value = value * 32 + NIX32.index(ch)
    size = {"sha256": 32, "sha512": 64, "sha1": 20}[algo]
    # Nix writes the base32 digits most significant first, bytes least first.
    raw = value.to_bytes(size, "little")
    import base64

    return f"{algo}-{base64.b64encode(raw).decode()}"


def read_registration(path):
    """store path -> {nar_hash, nar_size}, from a closureInfo `registration`."""
    out = {}
    if not path or not os.path.exists(path):
        return out
    lines = open(path).read().split("\n")
    i = 0
    while i + 4 < len(lines) and lines[i].startswith(STORE):
        p, digest, size, _deriver, refs = lines[i : i + 5]
        out[p] = {"nar_hash": nix32_to_sri(digest), "nar_size": int(size)}
        i += 5 + int(refs)
    return out


# --- YAML -------------------------------------------------------------------


def _scalar(v):
    return not isinstance(v, (dict, list)) or not v


def _inline(v):
    if isinstance(v, dict):
        return "{}"
    if isinstance(v, list):
        return "[]"
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    # A JSON string is a valid YAML double-quoted scalar.
    return json.dumps(str(v))


def _key(k):
    return k if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", k) else _inline(k)


def to_yaml(v, indent=0):
    pad = " " * indent
    out = []
    if isinstance(v, dict):
        for k, val in v.items():
            if _scalar(val):
                out.append(f"{pad}{_key(k)}: {_inline(val)}")
            else:
                out.append(f"{pad}{_key(k)}:")
                out += to_yaml(val, indent + 2)
    else:
        for item in v:
            if _scalar(item):
                out.append(f"{pad}- {_inline(item)}")
            else:
                sub = to_yaml(item, indent + 2)
                sub[0] = pad + "- " + sub[0][indent + 2 :]
                out += sub
    return out


# --- The bundle -------------------------------------------------------------


class Lib:
    def __init__(self, real, dest="lib"):
        self.real = real
        self.dest = dest
        self.aliases = set()
        self.wanted_by = set()
        self.how = set()


class Bundler:
    def __init__(self, cfg, out):
        self.cfg, self.out = cfg, out
        self.policy = cfg["policy"]
        self.libs = {}
        # Every name a vendored library answers to -> its real path.
        self.by_name = {}
        self.queue = []
        self.host = defaultdict(set)
        self.glibc_names = defaultdict(set)
        self.unresolved = defaultdict(set)
        self.errors = []
        self.requirements = []
        self.needs = {}
        # Real path -> absolute DT_NEEDED entries to replace with their basename.
        self.absolute_needed = defaultdict(set)
        # Real path -> destination directory, for `executables`.
        self.executables = {}

    def classify(self, name):
        if matches(name, self.policy["glibc"]):
            return "glibc"
        # Wanted from the host, but a copy is shipped for hosts that have none.
        if matches(name, self.policy.get("fallback", [])):
            return "fallback"
        if matches(name, self.policy["host"]):
            return "host"
        return "vendor"

    def resolve(self, name, elf):
        for d in elf.search_dirs() + [os.path.dirname(elf.path)] + self.cfg["search"]:
            candidate = os.path.join(d, name)
            if os.path.exists(candidate):
                return candidate
        return None

    def want(self, name, elf, wanted_by, hard):
        target = None
        if os.path.isabs(name):
            # nixpkgs links the odd library by its store path. The copy is
            # found by its file name instead.
            self.absolute_needed[os.path.realpath(elf.path)].add(name)
            target, name = name, os.path.basename(name)
        kind = self.classify(name)
        wanting = self.libs.get(os.path.realpath(elf.path))
        if kind == "host" and hard and wanting and wanting.dest == "lib-fallback":
            kind = "fallback"
        if kind == "glibc":
            self.glibc_names[name].add(wanted_by)
            return
        if kind == "host":
            self.host[name].add(wanted_by)
            # A library that cannot load without the host's X11 or PipeWire is
            # a library that does not load on a host without them. dlopen is
            # how these are meant to be reached; see policy.nix.
            if hard and elf.path != self.cfg["binary"]:
                self.errors.append(
                    f"{wanted_by} is linked against {name}, which is left to the host: "
                    "build it to load that at run time (SDL_DEPS_SHARED, ALSOFT_DLOPEN...), "
                    "or move the name to policy.fallback"
                )
            return
        found = target if target and os.path.exists(target) else None
        # A name already vendored is that library, even where this one's own
        # RUNPATH would find another build of it (or none): the loader will
        # only ever see the one in the bundle.
        found = found or self.by_name.get(name) or self.resolve(name, elf)
        if not found:
            if kind == "fallback":
                self.host[name].add(wanted_by)
            elif hard:
                self.errors.append(f"{wanted_by} needs {name}, which is nowhere in its closure")
            else:
                self.unresolved[name].add(wanted_by)
            return
        real = os.path.realpath(found)
        dest = "lib-fallback" if kind == "fallback" else "lib"
        real = self.by_name.get(os.path.basename(real), real)
        self.by_name.setdefault(name, real)
        self.by_name.setdefault(os.path.basename(real), real)
        lib = self.libs.get(real)
        if lib is None:
            lib = self.libs[real] = Lib(real, dest)
            self.queue.append(real)
        if name != os.path.basename(real):
            lib.aliases.add(name)
        lib.wanted_by.add(wanted_by)
        lib.how.add("DT_NEEDED" if hard else "dlopen")

    def visit(self, path, label):
        elf = Elf(path)
        if not elf.valid:
            return
        self.requirements.append(elf.glibc_requirement())
        for name in elf.needed:
            self.want(name, elf, label, True)
        for name in elf.dlopen_names():
            # A library's own SONAME is a string in it, too.
            if name not in elf.needed and name != elf.soname:
                self.want(name, elf, label, False)

    def discover(self):
        binary = self.cfg["binary"]
        self.visit(binary, "rosemary")
        for extra in self.cfg.get("extra_libraries", []):
            real = os.path.realpath(extra["path"])
            if real not in self.libs:
                self.libs[real] = Lib(real, extra.get("dest", "lib"))
                self.libs[real].how.add("extra library")
                self.queue.append(real)
        for extra in self.cfg.get("executables", []):
            real = os.path.realpath(extra["path"])
            self.executables[real] = extra["dest"]
            self.visit(real, os.path.basename(real))
        # Names the code builds at run time, so no scan can see them.
        probe = Elf(binary)
        for name in self.cfg.get("extra_sonames", []):
            self.want(name, probe, "extra_sonames", True)
        while self.queue:
            real = self.queue.pop()
            self.visit(real, os.path.basename(real))

    # -- copying --

    def patchelf(self, *args):
        subprocess.run(["patchelf", *args], check=True)

    def install_libs(self):
        taken = {}
        files = {}
        for real, lib in sorted(self.libs.items()):
            base = os.path.basename(real)
            lib_dir = os.path.join(self.out, lib.dest)
            os.makedirs(lib_dir, exist_ok=True)
            if (lib.dest, base) in taken:
                self.errors.append(f"{real} and {taken[lib.dest, base]} are both {base}")
                continue
            taken[lib.dest, base] = real
            dest = os.path.join(lib_dir, base)
            shutil.copyfile(real, dest)
            os.chmod(dest, 0o755)
            original = sha256(real)
            elf = Elf(dest)
            needs = elf.glibc_requirement() if elf.valid else None
            if elf.valid:
                self.requirements.append(needs)
                self.needs[f"{lib.dest}/{base}"] = needs
                self.patchelf("--set-rpath", self.rpath_for(lib.dest), dest)
                self.replace_absolute_needed(real, dest)
                for added in self.cfg.get("add_needed", {}).get(elf.soname or base, []):
                    # Ahead of everything else it needs, so its symbols come
                    # first in this library's lookup scope.
                    self.patchelf("--add-needed", added, dest)
                # An executable that got in here would need an interpreter,
                # so say so rather than ship it. A library with one of its
                # own (libcap prints its version) still has a soname.
                if elf.interpreter and not elf.soname:
                    self.errors.append(f"{real} is an executable")
            for alias in sorted(lib.aliases):
                link = os.path.join(lib_dir, alias)
                if not os.path.lexists(link):
                    os.symlink(base, link)
            files[real] = {
                "path": f"{lib.dest}/{base}",
                "symlinks": [f"{lib.dest}/{a}" for a in sorted(lib.aliases)],
                "original_sha256": original,
                "sha256": sha256(dest),
                "needs_glibc": fmt_version(needs),
                "found_by": sorted(lib.how),
                "wanted_by": sorted(lib.wanted_by),
            }
        return files

    def rpath_for(self, dest):
        """`$ORIGIN`, then the way from `dest` to `lib/` if that is elsewhere."""
        if dest == "lib":
            return "$ORIGIN"
        return "$ORIGIN:$ORIGIN/" + os.path.relpath("lib", dest)

    def replace_absolute_needed(self, real, dest):
        for name in sorted(self.absolute_needed.get(real, ())):
            self.patchelf("--replace-needed", name, os.path.basename(name), dest)

    def install_executables(self):
        files = {}
        for real, dest_dir in sorted(self.executables.items()):
            out_dir = os.path.join(self.out, dest_dir)
            os.makedirs(out_dir, exist_ok=True)
            base = os.path.basename(real)
            dest = os.path.join(out_dir, base)
            shutil.copyfile(real, dest)
            os.chmod(dest, 0o755)
            original = sha256(real)
            elf = Elf(dest)
            needs = elf.glibc_requirement()
            self.requirements.append(needs)
            self.needs[f"{dest_dir}/{base}"] = needs
            self.patchelf(
                "--set-interpreter", self.cfg["interpreter"],
                "--set-rpath", "$ORIGIN/" + os.path.relpath("lib", dest_dir),
                dest,
            )
            self.replace_absolute_needed(real, dest)
            files[real] = {
                "path": f"{dest_dir}/{base}",
                "symlinks": [],
                "original_sha256": original,
                "sha256": sha256(dest),
                "needs_glibc": fmt_version(needs),
                "found_by": ["executable"],
                "wanted_by": [],
            }
        return files

    def install_glibc(self):
        g = self.cfg["glibc"]
        dest = os.path.join(self.out, "glibc/lib")
        os.makedirs(dest, exist_ok=True)
        files = []
        for name in sorted(os.listdir(g["lib"])):
            if not matches(name, g["keep"]):
                continue
            src = os.path.join(g["lib"], name)
            if os.path.isdir(src):
                continue
            target = os.path.join(dest, name)
            shutil.copyfile(src, target)
            os.chmod(target, 0o755)
            elf = Elf(target)
            # Its own RUNPATH names the store; the bundle directory is on the
            # search path already.
            if elf.valid and any(p.startswith(STORE) for p in elf.rpath):
                self.patchelf("--set-rpath", "$ORIGIN", target)
            files.append({"path": f"glibc/lib/{name}", "sha256": sha256(target)})
        gconv = os.path.join(g["lib"], "gconv")
        if os.path.isdir(gconv):
            shutil.copytree(gconv, os.path.join(dest, "gconv"), copy_function=shutil.copyfile, dirs_exist_ok=True)
            # copytree keeps the store's read-only directory modes.
            for root, dirs, names in os.walk(os.path.join(dest, "gconv")):
                os.chmod(root, 0o755)
                for n in names:
                    os.chmod(os.path.join(root, n), 0o644)
            files.append({"path": "glibc/lib/gconv/", "sha256": None})
        return files

    # -- verification --

    def verify(self):
        """Every library in the tree finds each DT_NEEDED somewhere it will be."""
        have = set()
        for d in ("lib", "lib-fallback", "glibc/lib"):
            p = os.path.join(self.out, d)
            if os.path.isdir(p):
                have |= set(os.listdir(p))
        paths = [os.path.join(self.out, "libexec/rosemary")]
        for d in ("lib", "lib-fallback"):
            if os.path.isdir(os.path.join(self.out, d)):
                paths += [os.path.join(self.out, d, n) for n in sorted(os.listdir(os.path.join(self.out, d)))]
        paths += [os.path.join(self.out, lib.dest, os.path.basename(real)) for real, lib in self.libs.items()
                  if lib.dest not in ("lib", "lib-fallback")]
        paths += [os.path.join(self.out, d, os.path.basename(real)) for real, d in self.executables.items()]
        for path in paths:
            if not os.path.isfile(path) or os.path.islink(path):
                continue
            elf = Elf(path)
            if not elf.valid:
                continue
            for name in elf.needed:
                if os.path.isabs(name):
                    self.errors.append(f"{os.path.relpath(path, self.out)} needs {name} by path")
                elif name not in have and self.classify(name) in ("vendor", "fallback"):
                    self.errors.append(f"{os.path.relpath(path, self.out)} needs {name}, which is not in the bundle")
            for ref in (p for p in elf.rpath if p.startswith(STORE)):
                self.errors.append(f"{os.path.relpath(path, self.out)} still has RUNPATH {ref}")

    def store_references(self):
        """Files that still mention the Nix store, which only matter if a path is opened."""
        found = {}
        for root, _, names in os.walk(self.out):
            for n in names:
                p = os.path.join(root, n)
                if os.path.islink(p):
                    continue
                refs = sorted(
                    r
                    for r in {m.group().decode() for m in STORE_REF.finditer(open(p, "rb").read())}
                    # nukeReferences leaves this hash where it removed a path.
                    if not r.startswith(STORE + "/" + "e" * 32 + "-")
                )
                if refs:
                    found[os.path.relpath(p, self.out)] = refs
        return found

    # -- report --

    def package_for(self, path):
        root = store_root(path)
        meta = self.cfg["metadata"].get(root, {})
        return root, meta

    def licenses(self, root, name):
        found = []
        dest = os.path.join(self.out, "share/doc/rosemary/licenses", name)
        candidates = [root]
        for sub in ("share/doc", "share/licenses"):
            base = os.path.join(root, sub)
            if os.path.isdir(base):
                candidates += [os.path.join(base, d) for d in sorted(os.listdir(base))]
                candidates.append(base)
        for d in candidates:
            if not os.path.isdir(d):
                continue
            for n in sorted(os.listdir(d)):
                p = os.path.join(d, n)
                if LICENSE_FILE.match(n) and os.path.isfile(p) and n not in found:
                    os.makedirs(dest, exist_ok=True)
                    shutil.copyfile(p, os.path.join(dest, n))
                    found.append(n)
        return [f"share/doc/rosemary/licenses/{name}/{n}" for n in found]

    def report(self, files, glibc_files, required, references):
        registration = read_registration(self.cfg.get("registration"))
        packages = {}

        def entry(root):
            if root not in packages:
                meta = self.cfg["metadata"].get(root, {})
                name = meta.get("pname") or re.sub(r"^[0-9a-z]{32}-", "", os.path.basename(root))
                packages[root] = {
                    "name": name,
                    "version": meta.get("version"),
                    "store_path": root,
                    "output": meta.get("output"),
                    "nar": registration.get(root),
                    "license": meta.get("license", []),
                    "homepage": meta.get("homepage"),
                    "source": meta.get("source"),
                    "patches": meta.get("patches", []),
                    "license_files": [],
                    "files": [],
                }
                if root not in self.cfg["metadata"]:
                    packages[root]["metadata"] = "store path only: not reachable from the declared inputs"
            return packages[root]

        for real, info in sorted(files.items()):
            entry(store_root(real))["files"].append({**info, "origin": real})
        g = self.cfg["glibc"]
        gp = entry(g["store_path"])
        gp["files"] = glibc_files
        gp["role"] = "bundled glibc, used when the host's is older than required_glibc"
        for root, p in packages.items():
            p["license_files"] = self.licenses(root, p["name"]) if os.path.isdir(root) else []
        for root, p in packages.items():
            if root not in self.cfg["metadata"]:
                self.errors.append(f"no metadata for {root}")

        return {
            "schema": 1,
            "generated_by": "tools/bundle/bundle.py",
            "rosemary": {
                "version": self.cfg.get("version"),
                "store_path": store_root(self.cfg["binary"]),
            },
            "nixpkgs": self.cfg.get("nixpkgs"),
            "glibc": {
                "bundled": g["version"],
                "required": fmt_version(required),
                # The files that set the floor: a host older than this gets
                # the bundled glibc.
                "required_by": sorted(p for p, v in self.needs.items() if required and v == required),
                "host_mode_when": "host glibc defines symbol versions up to at least `required`",
            },
            "packages": sorted(packages.values(), key=lambda p: (p["name"], p["store_path"])),
            "host_libraries": [
                {"soname": n, "wanted_by": sorted(w)} for n, w in sorted(self.host.items())
            ],
            "glibc_libraries": sorted(self.glibc_names),
            "unresolved_dlopen": [
                {"soname": n, "wanted_by": sorted(w)} for n, w in sorted(self.unresolved.items())
            ],
            "store_references": references,
        }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--config", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    cfg = json.load(open(args.config))

    b = Bundler(cfg, args.out)
    b.discover()
    files = b.install_libs()
    files.update(b.install_executables())
    glibc_files = b.install_glibc()
    b.verify()
    b.needs["libexec/rosemary"] = Elf(cfg["binary"]).glibc_requirement()
    required = max((r for r in b.requirements if r), default=None)

    conf = [f"# generated by tools/bundle/bundle.py"]
    if required:
        conf.append(f"required_glibc={required[0]}.{required[1]}")
    conf += cfg.get("conf", [])
    existing = os.path.join(args.out, "share/rosemary/bundle.conf")
    if os.path.exists(existing):
        conf = [open(existing).read().rstrip("\n")] + conf
    with open(existing, "w") as f:
        f.write("\n".join(conf) + "\n")

    references = b.store_references()
    # The launcher and the real binary are what runs before anything can be
    # redirected; a store path in either is a bug.
    for must_be_clean in ("bin/rosemary", "libexec/rosemary"):
        if must_be_clean in references:
            b.errors.append(f"{must_be_clean} mentions the store: {references[must_be_clean]}")
    doc = b.report(files, glibc_files, required, references)

    out_doc = os.path.join(args.out, "share/doc/rosemary")
    os.makedirs(out_doc, exist_ok=True)
    with open(os.path.join(out_doc, "SOURCES.yaml"), "w") as f:
        f.write("\n".join(to_yaml(doc)) + "\n")

    for e in b.errors:
        print(f"bundle.py: error: {e}", file=sys.stderr)
    print(
        f"bundle.py: {len(b.libs)} libraries, {len(glibc_files)} glibc files, "
        f"{len(b.host)} left to the host, required glibc "
        f"{fmt_version(required)}",
        file=sys.stderr,
    )
    return 1 if b.errors else 0


if __name__ == "__main__":
    sys.exit(main())
