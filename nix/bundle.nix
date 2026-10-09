{ lib
, stdenv
, runCommand
, python3
, patchelf
, closureInfo
, writeText
, rosemary
, fhs
, nixpkgsInfo ? null
  # Ship WebKitGTK too. It is a few hundred MB with helper processes of its own
  # that need more than library files, so by default the host's is used if any.
, withWebkit ? false
  # Sonames the code builds at run time, which no scan of the binary can see.
, extraSonames ? [ ]
}:

# fhs plus everything it needs from outside: its libraries, found by following
# DT_NEEDED and the sonames it might dlopen (tools/bundle/bundle.py), and a
# glibc for hosts whose own is too old. Nothing here is a Nix store path at run
# time, but the files were all built by Nix, and share/doc/rosemary/SOURCES.yaml
# says which.
let
  libc = stdenv.cc.libc;
  gccLib = stdenv.cc.cc.lib;

  # package.nix declares what Rosemary links and dlopens; this is that list.
  runtimeLibs = lib.filter (p: withWebkit || (p.pname or "") != "webkitgtk") rosemary.runtimeLibs;
  providers = runtimeLibs ++ [ gccLib ];

  policy = import ./bundle/policy.nix { inherit withWebkit; };
  metadata = import ./bundle/metadata.nix { inherit lib; } (providers ++ [ libc ]);

  config = writeText "rosemary-bundle.json" (builtins.toJSON {
    binary = "${rosemary}/bin/.rosemary-wrapped";
    version = rosemary.version;
    nixpkgs = nixpkgsInfo;
    # The libraries SDL and friends open by name are in their build inputs, not
    # on any RUNPATH once they load them at run time. Looked in only for names
    # the policy wants vendored.
    search = map (p: "${lib.getLib p}/lib") (providers ++ lib.concatMap
      (p: lib.filter lib.isDerivation (p.buildInputs or [ ])) providers);
    inherit policy metadata;
    extra_sonames = extraSonames;
    registration = "${closureInfo { rootPaths = providers ++ [ libc ]; }}/registration";
    glibc = {
      lib = "${lib.getLib libc}/lib";
      store_path = "${lib.getLib libc}";
      version = libc.version;
      # What a process needs from glibc itself. Leaves out the debugging and
      # auditing libraries, and libidn/locale data nothing here uses.
      keep = [
        "ld-linux*" "ld64.so.*" "libc.so.*" "libm.so.*" "libdl.so.*"
        "libpthread.so.*" "librt.so.*" "libutil.so.*" "libresolv.so.*"
        "libanl.so.*" "libnss_files.so.*" "libnss_dns.so.*" "libmvec.so.*"
      ];
    };
  });
in
runCommand "rosemary-bundle-${rosemary.version}"
  {
    nativeBuildInputs = [ python3 patchelf ];
    __structuredAttrs = true;
    # The libraries are full of their own build paths (glibc's gconv and locale
    # directories, say). Those are never opened: the launcher points the loader
    # at the bundle instead. Listing them as run-time dependencies would
    # defeat the point.
    unsafeDiscardReferences.out = true;
    passthru = { inherit config policy; };
  }
  ''
    cp -r ${fhs} $out
    chmod -R u+w $out
    python3 ${../tools/bundle/bundle.py} --config ${config} --out $out
  ''
