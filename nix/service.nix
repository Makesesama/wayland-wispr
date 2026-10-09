# home-manager module: user service, config file, and whisper models.
#
#   imports = [ wayland-wispr.homeManagerModules.default ];
#   services.wayland-wispr = {
#     enable = true;
#     settings.transcription.whisper.model = "small-q5_1"; # fetched automatically
#   };
#
# Bind `wayland-wispr toggle` (and `insert`, `cancel`, …) in the window manager.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.wayland-wispr;
  tomlFormat = pkgs.formats.toml { };

  knownModels = lib.attrNames (lib.importJSON ./models.json).models;
  fetchedModels = pkgs.callPackage ./models.nix { };

  # Mirrors the app's defaults (src/config.rs) to pick the model to fetch.
  whisper = cfg.settings.transcription.whisper or { };
  backend = cfg.settings.transcription.backend or "whisper";
  configuredModel = whisper.model or "base-q5_1";
  # Models are linked into the app's default lookup directory; an explicit
  # model_path / models_dir bypasses it.
  usesDefaultModelsDir = !(whisper ? model_path) && !(whisper ? models_dir);

  modelsSubdir = "wayland-wispr/models";
  modelFiles =
    lib.listToAttrs (map (name: lib.nameValuePair "ggml-${name}.bin" fetchedModels.${name}) cfg.models)
    // lib.mapAttrs' (name: path: lib.nameValuePair "ggml-${name}.bin" path) cfg.extraModels;

  configFile = tomlFormat.generate "wayland-wispr-config.toml" cfg.settings;
  # Rejects unknown keys and invalid values at build time instead of in a
  # restart loop. Model files are only reported, not required, here.
  checkedConfig =
    pkgs.runCommand "wayland-wispr-config.toml"
      {
        nativeBuildInputs = [ cfg.package ];
        preferLocalBuild = true;
      }
      ''
        wayland-wispr --config ${configFile} check-config
        cp ${configFile} $out
      '';
in
{
  options.services.wayland-wispr = {
    enable = lib.mkEnableOption "wayland-wispr, a Wayland dictation overlay";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./default.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./nix/default.nix { }";
      example = lib.literalExpression "wayland-wispr.packages.\${pkgs.stdenv.hostPlatform.system}.wayland-wispr-vulkan";
      description = ''
        The wayland-wispr package. Use a build with `vulkanSupport = true`
        (flake output `wayland-wispr-vulkan`) for
        `transcription.whisper.device = "gpu"`.
      '';
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = { };
      example = lib.literalExpression ''
        {
          transcription.whisper = {
            model = "large-v3-turbo-q5_0";
            device = "gpu";
            languages = [ "de" "en" ];
            beam_size = 5;
          };
          cleanup.model = "qwen2.5:7b-instruct";
          injection.paste_chord = "ctrl+shift+v";
        }
      '';
      description = ''
        Written to {file}`$XDG_CONFIG_HOME/wayland-wispr/config.toml` and
        validated at build time; see `config.example.toml` for all keys.
        When empty, no file is written and an existing one is left alone.
        Do not put secrets here; use {option}`environmentFile` with
        `cleanup.api_key_env`.
      '';
    };

    models = lib.mkOption {
      type = lib.types.listOf (lib.types.enum knownModels);
      default = lib.optional (
        backend == "whisper" && usesDefaultModelsDir && lib.elem configuredModel knownModels
      ) configuredModel;
      defaultText = lib.literalMD ''
        the configured `transcription.whisper.model` (app default `base-q5_1`),
        unless `model_path` or `models_dir` is set
      '';
      example = [
        "large-v3-turbo-q5_0"
        "small-q5_1"
      ];
      description = ''
        whisper.cpp models to fetch (pinned in `nix/models.json`) and link
        into {file}`$XDG_DATA_HOME/wayland-wispr/models`, where the app looks
        up `transcription.whisper.model` by name. Extra models are handy for
        `wayland-wispr bench --model <name>`. Manually downloaded models in
        that directory keep working.
      '';
    };

    extraModels = lib.mkOption {
      type = lib.types.attrsOf lib.types.path;
      default = { };
      example = lib.literalExpression ''{ "large-v3-de" = ./ggml-large-v3-de.bin; }'';
      description = ''
        Additional ggml model files by name, linked as `ggml-<name>.bin`
        next to {option}`models` (taking precedence on name clashes).
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "%h/.config/wayland-wispr/secrets.env";
      description = ''
        Runtime file with `KEY=value` lines for the service, e.g. the API key
        named by `cleanup.api_key_env`. Not copied to the Nix store.
      '';
    };

    systemdTarget = lib.mkOption {
      type = lib.types.str;
      default = "graphical-session.target";
      description = ''
        Target the service is started with. The compositor session must
        import `WAYLAND_DISPLAY` into the systemd user environment.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    warnings =
      lib.optional (modelFiles != { } && !usesDefaultModelsDir) ''
        services.wayland-wispr: transcription.whisper.model_path/models_dir is set, so the
        models linked into $XDG_DATA_HOME/${modelsSubdir} are not used.
      ''
      ++
        lib.optional
          (
            backend == "whisper"
            && usesDefaultModelsDir
            && !(modelFiles ? "ggml-${configuredModel}.bin")
            && !(lib.hasSuffix ".bin" configuredModel)
          )
          ''
            services.wayland-wispr: whisper model `${configuredModel}` is not provided by
            services.wayland-wispr.models or extraModels; it must exist in
            $XDG_DATA_HOME/${modelsSubdir} or the service fails to start.
          ''
      ++ lib.optional ((whisper.device or "cpu") == "gpu" && !(cfg.package.vulkanSupport or true)) ''
        services.wayland-wispr: transcription.whisper.device = "gpu" needs a package built
        with vulkanSupport = true (flake output wayland-wispr-vulkan).
      '';

    home.packages = [ cfg.package ];

    xdg.configFile."wayland-wispr/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = checkedConfig;
    };

    xdg.dataFile = lib.mapAttrs' (
      file: source: lib.nameValuePair "${modelsSubdir}/${file}" { inherit source; }
    ) modelFiles;

    systemd.user.services.wayland-wispr = {
      Unit = {
        Description = "wayland-wispr dictation overlay";
        PartOf = [ cfg.systemdTarget ];
        After = [
          cfg.systemdTarget
          "pipewire.service"
        ];
        ConditionEnvironment = "WAYLAND_DISPLAY";
        # Config and models are read once at startup.
        X-Restart-Triggers =
          lib.optional (cfg.settings != { }) "${checkedConfig}" ++ map toString (lib.attrValues modelFiles);
        # A missing model or compositor is not fixed by retrying forever.
        StartLimitIntervalSec = 60;
        StartLimitBurst = 5;
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} run";
        Restart = "on-failure";
        RestartSec = 3;
      }
      // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      };
      Install.WantedBy = [ cfg.systemdTarget ];
    };
  };
}
