{ lib
, stdenv
, fetchzip
, python3
, llvmPackages
, lld
}:

let
  # Only the Steamworks SDK headers under lsteamclient/ are used.
  proton = fetchzip {
    url = "https://github.com/ValveSoftware/Proton/archive/refs/tags/proton-11.0-2.zip";
    hash = "sha256-LghANNbut7QVW4ysxE6tpY64uDC3YAlpqz9BzAO5p3w=";
  };
in
stdenv.mkDerivation {
  pname = "rosemary-steamclient";
  version = "proton-11.0-2";

  src = lib.fileset.toSource {
    root = ../tools/steamclient;
    fileset = lib.fileset.unions [
      ../tools/steamclient/gen_steamclient.py
      ../tools/steamclient/sdkparse.py
      ../tools/steamclient/support
    ];
  };

  nativeBuildInputs = [ python3 llvmPackages.clang-unwrapped llvmPackages.bintools-unwrapped lld ];

  # Cross-compiles to Mach-O with freestanding sources; no host toolchain wrapper.
  dontConfigure = true;
  dontFixup = true;

  buildPhase = ''
    runHook preBuild
    python3 gen_steamclient.py --sdk-root ${proton}/lsteamclient --out generated
    cd generated
    CLANG=clang++ LD64=ld64.lld LIPO=llvm-lipo JOBS=$NIX_BUILD_CORES sh build.sh
    cd ..
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm644 generated/steamclient.dylib $out/lib/steamclient.dylib
    # The host half reads the table from beside the dylib.
    install -Dm644 generated/steamclient_dispatch.bin $out/lib/steamclient_dispatch.bin
    runHook postInstall
  '';

  meta = {
    description = "Guest-facing SteamClient dylib bridging macOS guests to the host Steam client";
    license = with lib.licenses; [ mit asl20 ];
    platforms = lib.platforms.unix;
  };
}
