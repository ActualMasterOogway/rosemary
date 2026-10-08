{
  description = "Rosemary: run macOS applications across architectures";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems =
        [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ (import rust-overlay) ];
      };
    in {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          # nixpkgs' rustc can trail the 1.99 this tree needs.
          toolchain = pkgs.rust-bin.stable.latest.minimal;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          steamclient = pkgs.callPackage ./nix/steamclient.nix { };
          rosemary = buildType:
            pkgs.callPackage ./nix/package.nix {
              inherit rustPlatform buildType steamclient;
            };
        in {
          inherit steamclient;
          default = rosemary "release";
          rosemary = rosemary "release";
          rosemary-debug = rosemary "debug";
        });

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          inherit (pkgs) lib;

          toolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
            targets = lib.optional pkgs.stdenv.hostPlatform.isDarwin "x86_64-apple-darwin";
          };

          runtimeLibs = [ pkgs.sdl3 pkgs.curl pkgs.zlib ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.libglvnd
              pkgs.vulkan-loader
              # OpenAL: dlopened by the OpenAL.framework bridge.
              pkgs.openal
              # webkit: loaded with dlopen, so it has to be on the library path.
              pkgs.webkitgtk_6_0
              # webkit: the compositor WebKitGTK's windows are shown in.
              pkgs.libxkbcommon
            ];
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.lldb
              # Supply Nix system header paths to clangd, before clang-unwrapped.
              pkgs.clang-tools
              toolchain
              pkgs.pkg-config
              # Cargo fetches microlift and the Cranelift fork over git.
              pkgs.git
              pkgs.python3 # tools/guest-symbolicate.py
              # metal_translate
              pkgs.llvm
              # helpful for debugging metal
              pkgs.vkdevicechooser
              pkgs.vulkan-validation-layers
              pkgs.renderdoc
              # tools/steamclient: cross-compiles the guest-facing SteamClient
              # dylib to Mach-O without a macOS SDK.
              pkgs.llvmPackages.clang-unwrapped
              pkgs.lld
            ];

            buildInputs = runtimeLibs;

            # WebKitGTK's network stack finds TLS through a GIO module.
            GIO_EXTRA_MODULES = lib.optionalString pkgs.stdenv.hostPlatform.isLinux
              "${pkgs.glib-networking}/lib/gio/modules";

            LD_LIBRARY_PATH = lib.optionalString pkgs.stdenv.hostPlatform.isLinux
              (lib.makeLibraryPath runtimeLibs);
            DYLD_FALLBACK_LIBRARY_PATH = lib.optionalString pkgs.stdenv.hostPlatform.isDarwin
              (lib.makeLibraryPath runtimeLibs);
            VK_LAYER_PATH="${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
          };
        });
    };
}
