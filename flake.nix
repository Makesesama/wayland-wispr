{
  description = "Rust development environment for a Wayland dictation overlay";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell (
            {
              nativeBuildInputs = with pkgs; [
                cargo
                rustc
                rustfmt
                clippy
                rust-analyzer
                pkg-config
                cmake
                ninja
                clang
              ];

              buildInputs = with pkgs; [
                gtk4
                gtk4-layer-shell
                glib
                gsettings-desktop-schemas
                adwaita-icon-theme
                alsa-lib
                pipewire
                wayland
                libxkbcommon
                openssl
                llvmPackages.libclang
                # whisper.cpp Vulkan backend (`--features whisper-vulkan`).
                vulkan-headers
                vulkan-loader
                shaderc # glslc
              ];

              # Protocol inspection only; the app uses native clipboard/input APIs.
              packages = with pkgs; [ wayland-utils ];

              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            }
            # Nix's cc-wrapper strips -march=native, so ggml's default native
            # build silently ends up without SIMD (~10x slower on CPU). Request
            # x86-64-v3 explicitly; whisper-rs-sys forwards GGML_* to CMake.
            # After changing these: `cargo clean -p whisper-rs-sys`.
            // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isx86_64 {
              GGML_NATIVE = "OFF";
              GGML_AVX = "ON";
              GGML_AVX2 = "ON";
              GGML_FMA = "ON";
              GGML_F16C = "ON";
              GGML_BMI2 = "ON";
            }
            // {

              # Rust Wayland clients may load these libraries at runtime via dlopen.
              shellHook = ''
                export LD_LIBRARY_PATH="${
                  pkgs.lib.makeLibraryPath [
                    pkgs.wayland
                    pkgs.libxkbcommon
                    pkgs.vulkan-loader
                  ]
                }''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
                export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"

                # Iterate with the repo's dev config instead of ~/.config.
                # Set when entered from the repo root (direnv always does this).
                if [ -z "''${WAYLAND_WISPR_CONFIG:-}" ] && [ -f "$PWD/dev/config.toml" ]; then
                  export WAYLAND_WISPR_CONFIG="$PWD/dev/config.toml"
                fi
              '';
            }
          );
        }
      );
    };
}
