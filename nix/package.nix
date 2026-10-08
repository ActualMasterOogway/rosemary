{ lib
, stdenv
, rustPlatform
, pkg-config
, makeBinaryWrapper
, patchelf
, curl
, zlib
, sdl3
, libglvnd
, vulkan-loader
, openal
, webkitgtk_6_0
, libxkbcommon
, wayland
, glib-networking
, steamclient
  # "release" or "debug"; debug keeps the extra checks that catch native-guest bugs.
, buildType ? "release"
}:

let
  # Everything here is dlopened by soname at runtime rather than linked, so the
  # linker never records it; it reaches the binary through RUNPATH instead.
  runtimeLibs = [ sdl3 curl zlib ]
    ++ lib.optionals stdenv.hostPlatform.isLinux [
      libglvnd
      vulkan-loader
      openal
      webkitgtk_6_0
      libxkbcommon
      wayland
    ];
in
rustPlatform.buildRustPackage {
  pname = "rosemary" + lib.optionalString (buildType == "debug") "-debug";
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # Git dependencies are pinned by rev in Cargo.lock; fetching them with
    # builtins.fetchGit avoids a hash to bump on every microlift update.
    allowBuiltinFetchGit = true;
  };

  inherit buildType;
  cargoBuildFlags = [ "--package" "rosemary" "--bin" "rosemary" ];

  # The suite needs x86_64 Mach-O fixtures, a GPU and a display.
  doCheck = false;

  # Keep debug info for gdb/lldb and guest symbolication in a debug build.
  dontStrip = buildType == "debug";

  nativeBuildInputs = [ pkg-config makeBinaryWrapper ]
    ++ lib.optional stdenv.hostPlatform.isLinux patchelf;
  buildInputs = runtimeLibs;

  postFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibs} $out/bin/rosemary
    # WebKitGTK's network stack finds TLS through a GIO module.
    wrapProgram $out/bin/rosemary \
      --prefix GIO_EXTRA_MODULES : ${glib-networking}/lib/gio/modules \
      --set-default ROSEMARY_STEAM_CLIENT ${steamclient}/lib/steamclient.dylib
  '' + lib.optionalString stdenv.hostPlatform.isDarwin ''
    wrapProgram $out/bin/rosemary \
      --set-default ROSEMARY_STEAM_CLIENT ${steamclient}/lib/steamclient.dylib \
      --prefix DYLD_FALLBACK_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
  '';

  meta = {
    description = "Run macOS applications across architectures and operating systems";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "rosemary";
    platforms = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
  };
}
