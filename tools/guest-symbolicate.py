#!/usr/bin/env python3
"""Resolve an Instruments trace's JIT addresses to guest instructions.

Instruments symbolicates a host address against the *loaded images*, and a
translated region is in none of them: every one is its own Cranelift module in
its own anonymous mapping. So a frame in translated code - which is where a
working translation spends most of its time - comes back as a bare hexadecimal
address, and the half of the profile that matters is blank. There is no Darwin
equivalent of a jitdump for Instruments to read, so the join is done afterwards
and this is the thing that does it.

The left-hand side is `ROSETTA3_PROFILE_MAP` from the *same run*, written by
`rosetta3::profile::write_map`. Addresses only mean something within the run
that produced them; `tools/guest-trace.sh` records both together for that
reason.

    tools/guest-symbolicate.py <trace> <map> [--xpath XPATH] [--top N]

How a JIT frame is recognised: an Instruments `<frame>` inside a real image
carries a `<binary>` child naming it, and one that could not be symbolicated
does not. That is the whole test, and it holds across every backtrace-bearing
schema - `time-profile`, `cpu-counters`, `processor-trace` - because the
backtrace representation is shared between them.

Only the standard library is used, because a diagnostic that needs installing
is a diagnostic that does not get run.
"""

import argparse
import bisect
import collections
import os
import re
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

XCTRACE_FALLBACK = "/Applications/Xcode.app/Contents/Developer/usr/bin/xctrace"

# Schemas that carry backtraces of *runtime addresses*, best first. Asking the
# table of contents which of them a trace actually has beats hard-coding one per
# template.
BACKTRACE_SCHEMAS = (
    "cpu-counters-profile",
    "counters-profile",
    "time-profile",
    "time-sample",
)

# Processor Trace is deliberately not in that list, and the reason is a measured
# one rather than an oversight - see `docs/PROFILING.md`. Its tables do not
# carry runtime addresses at all. A location in `processor-trace-intervals` is a
# `<text-location>` holding a **binary UUID and a Mach-O vm offset**, which is a
# symbol resolved at record time and not an address this map could be joined
# against. In a 1.5-million-row trace of a translated program, all 6010 distinct
# locations carried a UUID and exactly one was the all-zero sentinel - which is
# where everything Instruments could not place, including every translation,
# ends up indistinguishably.
PROCESSOR_TRACE_SCHEMAS = ("processor-trace-intervals", "processor-trace-points")


def xctrace():
    """The `xctrace` that *works*, which is not the same as the one on `PATH`.

    `/usr/bin/xctrace` is a shim that exits with "requires Xcode" when the
    selected developer directory is the Command Line Tools, which is the
    ordinary state of a machine. Presence is not the test; running it is.
    """
    for candidate in (shutil.which("xctrace"), XCTRACE_FALLBACK):
        if not candidate or not os.access(candidate, os.X_OK):
            continue
        probe = subprocess.run([candidate, "version"], capture_output=True)
        if probe.returncode == 0:
            return candidate
    sys.exit("no working xctrace: install Xcode (the app, not just the Command Line Tools)")


def run(args):
    done = subprocess.run(args, capture_output=True, text=True)
    if done.returncode != 0:
        sys.exit(f"{' '.join(args)}:\n{done.stderr.strip()}")
    return done.stdout


# --------------------------------------------------------------- the map ----


class Map:
    """Host address ranges and what each was lifted from.

    Two levels, because the map has two. `I` lines are one guest instruction
    each and are what a branch trace deserves; `R` lines are one region and are
    what is there when `ROSETTA3_PROFILE_MAP_INSTRUCTIONS` was not set. A
    lookup prefers the instruction and falls back to the region, so a map
    recorded either way answers.
    """

    def __init__(self, path):
        self.starts, self.ends, self.what = [], [], []
        self.rstarts, self.rends, self.rwhat = [], [], []
        rows, rrows = [], []
        with open(path) as f:
            for line in f:
                if line.startswith("#") or not line.strip():
                    continue
                parts = line.split()
                if parts[0] == "I" and len(parts) >= 5:
                    lo, hi, va, name = parts[1], parts[2], parts[3], parts[4]
                    rows.append((int(lo, 16), int(hi, 16), (int(va, 16), name)))
                elif parts[0] == "R" and len(parts) >= 6:
                    lo, hi, va, name = parts[1], parts[2], parts[3], parts[5]
                    rrows.append((int(lo, 16), int(hi, 16), (int(va, 16), name)))
        for rows_, s, e, w in ((rows, self.starts, self.ends, self.what),
                               (rrows, self.rstarts, self.rends, self.rwhat)):
            rows_.sort()
            for lo, hi, payload in rows_:
                s.append(lo)
                e.append(hi)
                w.append(payload)
        if not rows and not rrows:
            sys.exit(f"{path}: no R or I lines - was ROSETTA3_PROFILE_MAP set?")

    @staticmethod
    def _find(starts, ends, what, addr):
        i = bisect.bisect_right(starts, addr) - 1
        if i < 0 or addr >= ends[i]:
            return None
        return what[i]

    def lookup(self, addr):
        """`(guest_va, image+offset, exact)` for a host address, or None.

        Tried at `addr` and at `addr - 1`: Instruments reports a *return*
        address for every frame but the leaf, and a return address is one past
        the call - which for the last instruction of a range lands in the next
        one, or past the end.
        """
        for probe in (addr, addr - 1):
            hit = self._find(self.starts, self.ends, self.what, probe)
            if hit:
                return hit[0], hit[1], True
            hit = self._find(self.rstarts, self.rends, self.rwhat, probe)
            if hit:
                return hit[0], hit[1], False
        return None


# ------------------------------------------------------------- the trace ----


def tables(trace):
    """Backtrace-bearing table schemas this trace actually has."""
    toc = run([xctrace(), "export", "--input", trace, "--toc"])
    return [s for s in re.findall(r'schema="([^"]+)"', toc)]


def export(trace, xpath):
    out = os.path.join(tempfile.mkdtemp(prefix="guest-symbolicate."), "table.xml")
    run([xctrace(), "export", "--input", trace, "--xpath", xpath, "--output", out])
    return out


def frames(xml_path):
    """Yield `(leaf, addr, symbolicated)` for every frame of every backtrace.

    Streamed rather than parsed whole: a Processor Trace export of anything
    real is hundreds of megabytes, and a tool that needs all of it in memory is
    a tool that gets killed instead of answering.

    Instruments orders a backtrace leaf-first, so the first frame of each is
    where the sample actually was - the distinction between self time and
    inclusive time, which for "is this translation hot" is the whole question.
    """
    for event, element in ET.iterparse(xml_path, events=("end",)):
        if element.tag != "backtrace":
            continue
        for i, frame in enumerate(element.findall("frame")):
            addr = frame.get("addr")
            if addr is not None:
                # A frame inside a loaded image carries the image; one that
                # Instruments could not place does not, and that is the test.
                named = frame.find("binary") is not None
                yield i == 0, int(addr, 16), named
        element.clear()


# ------------------------------------------------------------ the report ----


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("trace")
    ap.add_argument("map")
    ap.add_argument("--xpath", help="table to read, if the guess is wrong")
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--run", type=int, default=1)
    args = ap.parse_args()

    guest = Map(args.map)

    xpath = args.xpath
    if not xpath:
        have = tables(args.trace)
        schema = next((s for s in BACKTRACE_SCHEMAS if s in have), None)
        if not schema and any(s in have for s in PROCESSOR_TRACE_SCHEMAS):
            sys.exit(
                "this is a Processor Trace, and it cannot be joined to a guest map.\n"
                "Its locations are (binary UUID, Mach-O offset) pairs resolved when the\n"
                "trace was recorded, not runtime addresses - so there is nothing here to\n"
                "look up, and every translation is folded into one unnamed sentinel.\n"
                "Record 'Time Profiler' or 'CPU Counters' to join, and use\n"
                "ROSETTA3_PROFILE=1 for guest-attributed time. See docs/PROFILING.md.")
        if not schema:
            sys.exit("no backtrace-bearing table in this trace; pass --xpath. "
                     f"It has: {', '.join(sorted(set(have)))}")
        xpath = f'/trace-toc/run[@number="{args.run}"]/data/table[@schema="{schema}"]'
        print(f"reading table schema '{schema}'", file=sys.stderr)

    self_time = collections.Counter()
    inclusive = collections.Counter()
    names = {}
    totals = collections.Counter()
    unmatched = collections.Counter()
    approximate = 0

    for leaf, addr, named in frames(export(args.trace, xpath)):
        totals["frames"] += 1
        if named:
            totals["in an image"] += 1
            continue
        totals["unsymbolicated"] += 1
        hit = guest.lookup(addr)
        if hit is None:
            unmatched[addr] += 1
            continue
        va, name, exact = hit
        approximate += not exact
        totals["resolved to guest"] += 1
        names[va] = name
        inclusive[va] += 1
        if leaf:
            self_time[va] += 1

    if not totals["frames"]:
        sys.exit("the exported table had no backtraces - try another --xpath")

    print()
    for key in ("frames", "in an image", "unsymbolicated", "resolved to guest"):
        print(f"  {totals[key]:>9}  {key}")
    if approximate:
        print(f"  {approximate:>9}  resolved only to a region "
              f"(run with ROSETTA3_PROFILE_MAP_INSTRUCTIONS=1 for instructions)")
    if unmatched:
        # Not a failure to hide. An unsymbolicated address in no translation is
        # a host library's own generated code, or a trace and a map from
        # different runs - and those two want opposite responses.
        top = ", ".join(f"{a:#x}" for a, _ in unmatched.most_common(3))
        print(f"  {sum(unmatched.values()):>9}  unsymbolicated and in no "
              f"translation ({len(unmatched)} distinct, e.g. {top})")

    denominator = max(sum(self_time.values()), 1)
    print(f"\n  guest instructions by self time ({denominator} sample(s))")
    for va, n in self_time.most_common(args.top):
        print(f"    {100.0 * n / denominator:>6.2f}%  {va:#018x}  {names[va]}")

    denominator = max(sum(inclusive.values()), 1)
    print(f"\n  guest instructions by inclusive time ({denominator} frame(s))")
    for va, n in inclusive.most_common(args.top):
        print(f"    {100.0 * n / denominator:>6.2f}%  {va:#018x}  {names[va]}")
    print()


if __name__ == "__main__":
    main()
