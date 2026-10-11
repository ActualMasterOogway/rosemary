{ lib, stdenv, runCommand, gnutar, gzip, zstd, bundle, rosemary }:

# The Steam compatibility tool as a directory (`tree`) and as the archives to
# attach to a release (`archive`), both built on the bundle: extract the
# archive into compatibilitytools.d and Steam offers "Rosemary (macOS)".
let
  arch = stdenv.hostPlatform.parsed.cpu.name;
  name = "rosemary-steam-tool-${rosemary.version}-${arch}";

  tree = runCommand "rosemary-steam-tool-${rosemary.version}"
    {
      __structuredAttrs = true;
      # The libraries mention their own build paths; see bundle.nix.
      unsafeDiscardReferences.out = true;
    }
    ''
      mkdir -p $out
      cp -r ${bundle} $out/Rosemary
      chmod -R u+w $out/Rosemary
      src=${../tools/steam-compat}
      cp $src/compatibilitytool.vdf $src/toolmanifest.vdf $out/Rosemary/
      install -m755 $src/rosemary-steam $out/Rosemary/rosemary-steam
      cp ${../packaging/steam/README.txt} $out/Rosemary/README.txt
    '';

  archive = runCommand name
    { nativeBuildInputs = [ gnutar gzip zstd ]; }
    ''
      mkdir -p $out
      # Store files are read-only; the archive should extract into something
      # the user can delete.
      cp -r ${tree} tree
      chmod -R u+w tree
      cd tree
      for ext in gz zst; do
        tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
          -cf - Rosemary \
          | { if [ $ext = gz ]; then gzip -9n; else zstd -19 -T0 -q; fi; } \
          > $out/${name}.tar.$ext
      done
      cd $out
      sha256sum * > SHA256SUMS
    '';
in
tree // { inherit archive; }
