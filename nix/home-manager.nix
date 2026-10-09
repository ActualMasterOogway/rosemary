# Home Manager module: `programs.rosemary.steam.enable = true;` installs
# Rosemary into Steam as a compatibility tool.
{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.rosemary;
  inherit (pkgs.stdenv.hostPlatform) system;
in
{
  options.programs.rosemary = {
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.rosemary;
      defaultText = lib.literalExpression "rosemary.packages.\${system}.rosemary";
      description = "The Rosemary package.";
    };

    steam = {
      enable = lib.mkEnableOption "Rosemary as a Steam compatibility tool";

      package = lib.mkOption {
        type = lib.types.package;
        default = self.lib.${system}.steamCompat cfg.package;
        defaultText = lib.literalExpression "the tools/steam-compat tool built for `package`";
        description = "The compatibility tool; its Rosemary/ directory is linked into Steam.";
      };

      root = lib.mkOption {
        type = lib.types.str;
        default = ".local/share/Steam";
        description = "Steam root, relative to the home directory.";
      };
    };
  };

  config = lib.mkIf cfg.steam.enable {
    home.file."${cfg.steam.root}/compatibilitytools.d/Rosemary".source =
      "${cfg.steam.package}/Rosemary";
  };
}
