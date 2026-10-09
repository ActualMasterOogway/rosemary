{ lib, stdenv, rust-bin }:

# crates/launcher, linked statically against musl: it has to start on hosts
# whose glibc is too old for the bundle, or that have none.
let
  target = "${stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
  toolchain = rust-bin.stable.latest.minimal.override { targets = [ target ]; };
in
stdenv.mkDerivation {
  pname = "rosemary-launcher";
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  src = lib.fileset.toSource {
    root = ../crates/launcher;
    fileset = ../crates/launcher/src;
  };

  nativeBuildInputs = [ toolchain ];

  dontConfigure = true;
  dontFixup = true;

  # No dependencies, so rustc alone is enough and no vendoring is needed.
  buildPhase = ''
    runHook preBuild
    rustc --edition 2024 --crate-name rosemary_launcher --crate-type bin \
      --target ${target} -C opt-level=s -C panic=abort -C strip=symbols \
      -C target-feature=+crt-static src/main.rs -o rosemary-launcher
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 rosemary-launcher $out/bin/rosemary-launcher
    runHook postInstall
  '';

  meta = {
    description = "Chooses the host or bundled glibc and starts a packaged Rosemary";
    license = with lib.licenses; [ mit asl20 ];
    platforms = [ "x86_64-linux" "aarch64-linux" ];
    mainProgram = "rosemary-launcher";
  };
}
