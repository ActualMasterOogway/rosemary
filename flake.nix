{
  description = "Rosetta 3 development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };
          inherit (pkgs) lib;

          toolchain = pkgs.rust-bin.stable."1.98.0".default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
            targets = lib.optional pkgs.stdenv.isDarwin "x86_64-apple-darwin";
          };

          runtimeLibs =
            [
              pkgs.sdl3
              pkgs.curl
              pkgs.zlib
            ]
            ++ lib.optionals pkgs.stdenv.isLinux [
              pkgs.libglvnd
              pkgs.libGLU
              pkgs.vulkan-loader
            ];
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.lldb
              toolchain
              pkgs.pkg-config
              # Cargo fetches microlift and the Cranelift fork over git.
              pkgs.git
              pkgs.python3 # tools/guest-symbolicate.py
              # metal_translate
              pkgs.llvm
            ];

            buildInputs = runtimeLibs;

            LD_LIBRARY_PATH = lib.optionalString pkgs.stdenv.isLinux (
              lib.makeLibraryPath runtimeLibs
            );
            DYLD_FALLBACK_LIBRARY_PATH = lib.optionalString pkgs.stdenv.isDarwin (
              lib.makeLibraryPath runtimeLibs
            );
          };
        }
      );
    };
}
