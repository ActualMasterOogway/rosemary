#!/usr/bin/env python3
"""Fold a `ROSEMARY_PROFILE_OUT` histogram into guest functions.

The histogram ranks single instructions, which spreads one hot function over
dozens of rows. Kickstart links Mono statically, so its runtime is all one
image and `atos` names every row in a single call. Rows in no image are code
the guest wrote itself - for Mono, its JIT output - and are summed as such.

    tools/monobench/profile-symbols.py <histogram> <guest binary> [top]

Only the standard library is used, as in `tools/guest-symbolicate.py`.
"""

import collections
import os
import subprocess
import sys

GENERATED = "[guest-generated code: Mono JIT output]"


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    histogram, binary = sys.argv[1], sys.argv[2]
    top = int(sys.argv[3]) if len(sys.argv) > 3 else 25
    image = os.path.basename(binary)

    rows = []
    with open(histogram) as f:
        for line in f:
            if line.startswith("#"):
                continue
            kind, count, _share, _va, located = line.rstrip("\n").split("\t")
            if kind == "instruction":
                rows.append((int(count), located))

    def offset(located):
        name, _, off = located.partition("+")
        return off if name == image and off else None

    offsets = sorted({o for _, loc in rows if (o := offset(loc))})
    names = {}
    if offsets:
        # `-l 0` because the histogram's offsets already include the image's
        # preferred base.
        result = subprocess.run(
            ["atos", "-arch", "x86_64", "-o", binary, "-l", "0x0", *offsets],
            capture_output=True,
            text=True,
            check=True,
        )
        for off, sym in zip(offsets, result.stdout.splitlines()):
            names[off] = sym.split(" (in ")[0]

    totals = collections.Counter()
    for count, located in rows:
        off = offset(located)
        if off:
            totals[names.get(off, located)] += count
        elif located == "-":
            totals[GENERATED] += count
        else:
            totals[located.partition("+")[0]] += count

    total = sum(totals.values()) or 1
    for name, count in totals.most_common(top):
        print(f"{100 * count / total:6.2f}%  {name}")


if __name__ == "__main__":
    main()
