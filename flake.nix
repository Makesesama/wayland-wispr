{
  description = "Wayland-native dictation overlay";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        wayland-wispr = pkgs.callPackage ./nix/default.nix { };
        wayland-wispr-vulkan = wayland-wispr.override { vulkanSupport = true; };
        default = wayland-wispr;
      });

      # Pinned whisper.cpp models: `nix build '.#whisperModels."small-q5_1"'`.
      legacyPackages = forAllSystems (pkgs: {
        whisperModels = pkgs.callPackage ./nix/models.nix { };
      });

      overlays.default = final: _prev: {
        wayland-wispr = final.callPackage ./nix/default.nix { };
      };

      homeManagerModules = {
        wayland-wispr =
          { pkgs, lib, ... }:
          {
            imports = [ ./nix/service.nix ];
            # Build against this flake's pinned nixpkgs by default.
            services.wayland-wispr.package =
              lib.mkDefault
                self.packages.${pkgs.stdenv.hostPlatform.system}.wayland-wispr;
          };
        default = self.homeManagerModules.wayland-wispr;
      };

      devShells = forAllSystems (pkgs: {
        default = import ./nix/shell.nix { inherit pkgs; };
      });

      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.wayland-wispr;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
