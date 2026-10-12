# Installing Rosemary

Rosemary supports a variety of distributions and package formats. This document goes through each supported method.

## Nix

Rosemary ships a flake for `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, and `aarch64-darwin`. It exposes `packages.<system>.default` (alias `rosemary`) and a Home Manager module. There is no overlay or NixOS module.

### Try it without installing

```sh
nix run github:microlift/rosemary -- /path/to/application
```

### Flake input

Add Rosemary to your `flake.nix` inputs:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rosemary.url = "github:microlift/rosemary";
  };
}
```

### NixOS or nix-darwin

Add the package to your system packages:

```nix
{ pkgs, inputs, ... }:
{
  environment.systemPackages = [
    inputs.rosemary.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

### Home Manager

Import the module and add the package. The module itself only manages the Steam compatibility tool, so the binary still goes in `home.packages`:

```nix
{ pkgs, inputs, ... }:
{
  imports = [ inputs.rosemary.homeManagerModules.default ];

  home.packages = [ config.programs.rosemary.package ];
}
```

(Add `config` to the function arguments: `{ config, pkgs, inputs, ... }:`.)

### Steam compatibility tool

To install Rosemary as a Steam compatibility tool, enable it in Home Manager:

```nix
programs.rosemary.steam.enable = true;
```

This links the tool into `~/.local/share/Steam/compatibilitytools.d/Rosemary`. If Steam lives elsewhere, set `programs.rosemary.steam.root` (path relative to `$HOME`). Restart Steam, then select **Rosemary** under a game's compatibility settings.

### Options

| Option | Default | Description |
| --- | --- | --- |
| `programs.rosemary.package` | `rosemary.packages.<system>.rosemary` | The Rosemary package |
| `programs.rosemary.steam.enable` | `false` | Install as a Steam compatibility tool |
| `programs.rosemary.steam.package` | built from `package` | The Steam tool derivation |
| `programs.rosemary.steam.root` | `.local/share/Steam` | Steam root, relative to `$HOME` |

> **Note:** Rosemary pins its own Rust toolchain (via `rust-overlay` and `crane`). Avoid `inputs.rosemary.inputs.nixpkgs.follows = "nixpkgs"` unless you want to rebuild against your own nixpkgs revision.
