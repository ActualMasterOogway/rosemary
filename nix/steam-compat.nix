{ lib, runCommand, rosemary }:

# The Steam compatibility tool from tools/steam-compat, with the store path of
# `rosemary` baked in. The tool directory sits at $out/Rosemary so the output
# works both as programs.steam.extraCompatPackages and as a symlink target in
# compatibilitytools.d.
runCommand "rosemary-steam-compat" { } ''
  dest=$out/Rosemary
  mkdir -p $dest
  src=${../tools/steam-compat}
  cp $src/compatibilitytool.vdf $src/toolmanifest.vdf $dest/
  install -m755 $src/rosemary-steam $dest/rosemary-steam
  echo "ROSEMARY_BIN='${lib.getExe' rosemary "rosemary"}'" > $dest/rosemary-steam.env
''
