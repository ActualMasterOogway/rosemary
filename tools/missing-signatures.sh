#!/bin/zsh
# What a bundle will ask for that this runtime cannot yet describe.
#
# `rosetta3 <exe> --audit` answers the same question for one *dependency
# graph*, and under-reports twice over: it does not see the plugins a guest
# `dlopen`s at run time, and it counts data imports — which need no signature at
# all — alongside functions. Both matter. Subnautica's audit reports 811
# undescribed imports; the real work queue is a third of that, and the
# difference is bundle-internal symbols, `NSZeroRect`-style data, and entry
# points no plugin on this machine can resolve anyway.
#
# So this walks every Mach-O in the bundle instead, subtracts what the bundle
# defines for itself, subtracts what `abi::table` and `shims` already name, and
# then asks the *host* which of the remainder are functions — by resolving each
# with `dlsym` and finding the section it lands in. A symbol in a section
# carrying `S_ATTR_*_INSTRUCTIONS` is code and needs a signature; anything else
# is a data bind that native passthrough already satisfies.
#
#   tools/missing-signatures.sh /path/to/Some.app
#
# Prints three lists: FUNC (the work queue), DATA (nothing to do), and ABSENT
# (no host implementation under that exact name — usually a `$INODE64`-style
# variant the resolver strips later, occasionally a genuinely missing library).
#
# Requires a C compiler, which this project already assumes for its fixtures.

set -e
app=${1:?usage: missing-signatures.sh /path/to/Some.app}
root=${0:a:h:h}
work=$(mktemp -d)
trap 'rm -rf $work' EXIT

# Every Mach-O in the bundle, wherever it lives: a plugin is loaded by the guest
# and never appears in the executable's own dependency list.
binaries=()
while IFS= read -r f; do
  file -b "$f" 2>/dev/null | grep -q 'Mach-O' && binaries+=("$f")
done < <(find "$app" -type f \( -perm +111 -o -name '*.dylib' -o -name '*.bundle' -o -name '*.so' \) 2>/dev/null)

for f in $binaries; do
  nm -arch x86_64 -u "$f" 2>/dev/null | sed 's/^ *//'
done | sort -u | sed 's/^_//' > $work/undefined

# A symbol one bundle library imports and another exports never reaches the
# host, so it is not this table's problem.
for f in $binaries; do
  nm -arch x86_64 -gU "$f" 2>/dev/null | awk '$2 ~ /^[TDSBI]$/ {print $3}'
done | sort -u | sed 's/^_//' > $work/defined

# C++ mangled names go to the guest's own libc++; the Objective-C runtime and
# `dyld_stub_binder` are answered elsewhere by construction.
grep -vE '^(_?_Z|objc_|_objc_|OBJC_|dyld_stub_binder)' $work/undefined > $work/candidates

{
  grep -ohE '(sig!|vsig!|cbsig!)[[:space:]]*\([[:space:]]*(\[[^]]*\][[:space:]]*,[[:space:]]*)?"[A-Za-z_0-9$]+"' \
    "$root"/crates/rosetta3/src/abi/table.rs | grep -oE '"[A-Za-z_0-9$]+"$' | tr -d '"'
  grep -rhoE '"[A-Za-z_][A-Za-z_0-9$]*"' "$root"/crates/rosetta3/src/shims/ | tr -d '"'
} | sort -u > $work/known

comm -23 $work/candidates $work/defined | comm -23 - $work/known > $work/unknown

cat > $work/probe.c <<'PROBE'
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <dlfcn.h>
#include <mach-o/dyld.h>
#include <mach-o/loader.h>

/* Which section owns `p`, and does it hold instructions?
 *
 * `mach_vm_region` cannot answer this: the shared cache coalesces mappings, so
 * the protection it reports for a text address is the enclosing region's and
 * not the section's. The image's own load commands are exact. */
static int is_func(void *p) {
    Dl_info di;
    if (!dladdr(p, &di) || !di.dli_fbase) return -1;
    const struct mach_header_64 *mh = (const struct mach_header_64 *)di.dli_fbase;
    intptr_t slide = 0;
    for (uint32_t i = 0; i < _dyld_image_count(); i++)
        if ((const void *)_dyld_get_image_header(i) == (const void *)mh) {
            slide = _dyld_get_image_vmaddr_slide(i);
            break;
        }
    const struct load_command *lc = (const struct load_command *)(mh + 1);
    for (uint32_t i = 0; i < mh->ncmds; i++) {
        if (lc->cmd == LC_SEGMENT_64) {
            const struct segment_command_64 *sg = (const struct segment_command_64 *)lc;
            const struct section_64 *se = (const struct section_64 *)(sg + 1);
            for (uint32_t j = 0; j < sg->nsects; j++, se++) {
                uintptr_t lo = (uintptr_t)se->addr + slide, hi = lo + (uintptr_t)se->size;
                if ((uintptr_t)p >= lo && (uintptr_t)p < hi)
                    return (se->flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS)) ? 1 : 0;
            }
        }
        lc = (const struct load_command *)((const char *)lc + lc->cmdsize);
    }
    return -1;
}

int main(void) {
    /* `RTLD_DEFAULT` searches loaded images only, and nothing has loaded
     * AppKit or Metal just because a guest links against them — the same
     * fallback `NativeDylibProvider` needs, for the same reason. */
    static const char *frameworks[] = {
        "AppKit", "Metal", "MetalKit", "QuartzCore", "IOKit", "CoreVideo",
        "AVFoundation", "OpenGL", "GameController", "Security", "AudioToolbox",
        "CoreAudio", "SystemConfiguration", "CoreMedia", "Carbon", "IOSurface",
        "CoreImage", "ForceFeedback", "CoreGraphics", "Foundation", "CoreText",
        "CFNetwork", "ImageIO", "OpenAL", "VideoToolbox", "MediaToolbox", 0
    };
    char path[256];
    for (int i = 0; frameworks[i]; i++) {
        snprintf(path, sizeof path,
                 "/System/Library/Frameworks/%s.framework/%s",
                 frameworks[i], frameworks[i]);
        dlopen(path, RTLD_LAZY | RTLD_GLOBAL);
    }
    dlopen("/usr/lib/libxml2.dylib", RTLD_LAZY | RTLD_GLOBAL);
    dlopen("/usr/lib/libz.dylib", RTLD_LAZY | RTLD_GLOBAL);

    char line[512];
    while (fgets(line, sizeof line, stdin)) {
        line[strcspn(line, "\n")] = 0;
        if (!*line) continue;
        void *p = dlsym(RTLD_DEFAULT, line);
        if (!p) { printf("ABSENT\t%s\n", line); continue; }
        int k = is_func(p);
        printf("%s\t%s\n", k == 1 ? "FUNC" : k == 0 ? "DATA" : "UNKNOWN", line);
    }
    return 0;
}
PROBE

cc -O1 -o $work/probe $work/probe.c
$work/probe < $work/unknown > $work/probed

for kind in FUNC DATA ABSENT UNKNOWN; do
  n=$(grep -c "^$kind	" $work/probed || true)
  [ "$n" = 0 ] && continue
  print -r -- "### $kind ($n)"
  grep "^$kind	" $work/probed | cut -f2
  print
done
