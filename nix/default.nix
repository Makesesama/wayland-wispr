# wayland-wispr package. callPackage-style so it works from the flake, an
# overlay, or `pkgs.callPackage ./nix { }`.
#
#   vulkanSupport  build whisper.cpp with the Vulkan backend
#                  (`transcription.whisper.device = "gpu"`); needs a Vulkan
#                  driver at runtime (hardware.graphics on NixOS).
#   ggmlCpuFlags   GGML_* CMake defines for the CPU backend, see ggml-cpu.nix.
#                  Override e.g. with `{ GGML_NATIVE = "OFF"; }` for a
#                  baseline x86_64 build.
{
  lib,
  stdenv,
  rustPlatform,
  pkg-config,
  cmake,
  shaderc,
  wrapGAppsHook4,
  gtk4,
  gtk4-layer-shell,
  glib,
  alsa-lib,
  pipewire,
  libxkbcommon,
  vulkan-headers,
  vulkan-loader,
  callPackage,
  vulkanSupport ? false,
  ggmlCpuFlags ? import ./ggml-cpu.nix stdenv.hostPlatform,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name + lib.optionalString vulkanSupport "-vulkan";
  inherit (cargoToml.package) version;

  # Only what cargo needs: keeps dev/models (GiBs) and target/ out of the store
  # and avoids rebuilds when docs or nix files change. The example/dev configs
  # and fixtures are `include_str!`ed by tests.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../config.example.toml
      ../dev/config.toml
      ../dev/cleanup-fixtures.toml
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  buildFeatures = lib.optional vulkanSupport "whisper-vulkan";

  nativeBuildInputs = [
    pkg-config
    # whisper-rs-sys drives CMake itself; see dontUseCmakeConfigure.
    cmake
    # libclang + include paths for whisper-rs-sys and pipewire-sys bindgen.
    rustPlatform.bindgenHook
    wrapGAppsHook4
  ]
  ++ lib.optional vulkanSupport shaderc; # glslc for ggml's Vulkan shaders

  buildInputs = [
    gtk4
    gtk4-layer-shell
    glib
    alsa-lib
    pipewire
    libxkbcommon
  ]
  ++ lib.optionals vulkanSupport [
    vulkan-headers
    vulkan-loader
  ];

  dontUseCmakeConfigure = true;

  env = ggmlCpuFlags;

  passthru = {
    inherit vulkanSupport;
    # Pinned whisper.cpp models, e.g. `wayland-wispr.models."small-q5_1"`.
    models = callPackage ./models.nix { };
  };

  meta = {
    description = cargoToml.package.description;
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "wayland-wispr";
  };
}
