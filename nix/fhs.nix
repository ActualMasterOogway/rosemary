{ lib, stdenv, runCommand, patchelf, rosemary, launcher, steamclient }:

# Rosemary laid out the way a distribution would install it, with nothing from
# the Nix store referenced: the binary asks the host's loader for the host's
# libraries. This is the first stage of the portable packages; the bundle stage
# adds the libraries and a glibc to it.
#
#   bin/rosemary                    the launcher (crates/launcher)
#   libexec/rosemary                the real binary
#   share/rosemary/bundle.conf      what the launcher reads
#   share/rosemary/steamclient/     the guest-facing Steam bridge
let
  hostLoader =
    if stdenv.hostPlatform.isAarch64 then "/lib/ld-linux-aarch64.so.1"
    else "/lib64/ld-linux-x86-64.so.2";
in
runCommand "rosemary-fhs-${rosemary.version}"
  {
    nativeBuildInputs = [ patchelf ];
    passthru = { inherit hostLoader; };
    # The point of this stage. A reference here is a hard failure.
    allowedReferences = [ ];
  }
  ''
    install -Dm755 ${rosemary}/bin/.rosemary-wrapped $out/libexec/rosemary
    patchelf \
      --set-interpreter ${hostLoader} \
      --set-rpath '$ORIGIN/../lib' \
      $out/libexec/rosemary

    install -Dm755 ${launcher}/bin/rosemary-launcher $out/bin/rosemary

    install -Dm644 ${steamclient}/lib/steamclient.dylib \
      $out/share/rosemary/steamclient/steamclient.dylib
    install -Dm644 ${steamclient}/lib/steamclient_dispatch.bin \
      $out/share/rosemary/steamclient/steamclient_dispatch.bin

    cat > $out/share/rosemary/bundle.conf <<'EOF'
    default_env.ROSEMARY_STEAM_CLIENT=@ROOT@/share/rosemary/steamclient/steamclient.dylib
    EOF
  ''
