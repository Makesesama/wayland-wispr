# whisper.cpp ggml models from Hugging Face, pinned by nix/models.json
# (regenerate with nix/update-models.sh). Each attribute is a fixed-output
# derivation producing the model file itself, named like upstream
# (`ggml-<name>.bin`), so it can be linked into a models directory as-is.
#
#   (pkgs.callPackage ./models.nix { })."large-v3-turbo-q5_0"
{ lib, fetchurl }:
let
  pinned = lib.importJSON ./models.json;
in
lib.mapAttrs (
  name: model:
  fetchurl {
    name = model.file;
    url = "https://huggingface.co/${pinned.repo}/resolve/${pinned.rev}/${model.file}";
    inherit (model) sha256;
    passthru = {
      modelName = name;
      inherit (model) size;
    };
    meta = {
      description = "whisper.cpp ggml model ${name}";
      homepage = "https://huggingface.co/${pinned.repo}";
      license = lib.licenses.mit;
    };
  }
) pinned.models
