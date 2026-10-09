# Development shell: `nix develop` (flake) or `nix-shell nix/shell.nix`.
{
  pkgs ? import <nixpkgs> { },
}:
pkgs.mkShell (
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

    packages = with pkgs; [
      # Protocol inspection only; the app uses native clipboard/input APIs.
      wayland-utils
      # nix/update-models.sh
      curl
      jq
    ];

    RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

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
  # SIMD flags for whisper.cpp, shared with the package (see ggml-cpu.nix).
  # After changing these: `cargo clean -p whisper-rs-sys`.
  // import ./ggml-cpu.nix pkgs.stdenv.hostPlatform
)
