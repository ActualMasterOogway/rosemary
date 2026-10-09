{ lib, stdenv, runCommand, fetchurl, squashfsTools, bundle, rosemary }:

# The bundle as an AppImage (type 2): the upstream runtime with a squashfs of
# the tree appended. AppRun is the launcher, so the glibc choice happens on the
# host at every start, not at build time.
let
  arch = stdenv.hostPlatform.parsed.cpu.name;
  runtime = fetchurl {
    url = "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-${arch}";
    hash = {
      x86_64 = "sha256-L8qLRDySUQ8Ug6iD9gBhrQm0a5eLJjHIB82HOkfsJg0=";
      aarch64 = "sha256-AMvfz5F8xsD/bTNH1Z4Moff0Wm3xpCig1tinhmTYdEQ=";
    }.${arch};
  };
in
runCommand "Rosemary-${rosemary.version}-${arch}.AppImage"
  { nativeBuildInputs = [ squashfsTools ]; }
  ''
    mkdir AppDir
    cp -r ${bundle}/. AppDir/
    chmod -R u+w AppDir
    cp ${../packaging/appimage/rosemary.desktop} AppDir/rosemary.desktop
    cp ${../packaging/appimage/rosemary.svg} AppDir/rosemary.svg
    ln -s rosemary.svg AppDir/.DirIcon
    ln -s bin/rosemary AppDir/AppRun

    # zstd is what the runtime is built for. mksquashfs takes its timestamps
    # from SOURCE_DATE_EPOCH, which the Nix builder sets.
    mksquashfs AppDir image.squashfs -root-owned -noappend -quiet \
      -comp zstd -Xcompression-level 19 -b 1M
    cat ${runtime} image.squashfs > $out
    chmod +x $out
  ''
