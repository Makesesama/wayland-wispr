# wayland-wispr

<p align="center">
  <img src=".assets/overlay.png" alt="wayland-wispr recording overlay: a small pill with a red dot, a live waveform and the elapsed time">
</p>

**Wayland-native, local-first dictation.** Press a key, speak, press it again,
and the text appears in the focused window.

wayland-wispr is a small resident process with a status overlay. You control it
from a keybinding in your compositor or window manager:

1. **Record.** It captures your microphone through PipeWire, or ALSA as a
   fallback. A pill shows a live waveform and the elapsed time.
2. **Transcribe.** whisper.cpp transcribes the recording **on your machine**,
   on the CPU or on the GPU through Vulkan. It recognises German and English,
   or any language Whisper supports.
3. **Polish (optional).** An LLM server that you run yourself (Ollama,
   llama-server, …) removes filler words and stutters. It also applies
   self-corrections ("at two, actually three" becomes "at three"), fixes
   punctuation and turns spoken "new line" or "Komma" into formatting. If
   the server fails, the raw transcript is used.
4. **Insert.** The result goes to the clipboard and is typed into the focused
   application through a virtual keyboard. Typing works with any keyboard
   layout and doesn't depend on the application's paste shortcut.

Recordings are never written to disk, and no network requests are made unless
you point cleanup at a remote server. `status` never includes transcript text.

> **Status:** usable end to end with local transcription and local LLM cleanup.
> A remote speech-to-text API backend is not implemented yet.

> **AI notice:** this project was created with AI assistance (Claude Opus 5.5).

## Contents

- [Requirements](#requirements)
- [Setup with Nix](#setup-with-nix)
- [Guide](#guide)
- [Configuration](#configuration)
- [Command reference](#command-reference)
- [Development](#development)

## Requirements

- A Wayland compositor that provides these protocols. wlroots-based compositors
  such as river, sway and Hyprland do. **GNOME/Mutter does not.**
  - `zwlr_layer_shell_v1` for the overlay
  - `zwp_virtual_keyboard_manager_v1` for typing and paste chords
  - `ext_data_control_manager_v1` or `zwlr_data_control_manager_v1` for the
    clipboard

  To check your compositor, run `wayland-wispr probe` (or
  `wayland-info | grep -E 'layer_shell|virtual_keyboard|data_control'`).
- PipeWire or ALSA for microphone input.
- For GPU transcription: a Vulkan driver, e.g. `hardware.graphics.enable = true`
  on NixOS. A discrete GPU runs `large-v3-turbo` in about 0.3 s per clip.
  Small models run fine on the CPU.
- For cleanup (optional): an OpenAI-compatible `/chat/completions` server.
  Ollama is the default.

## Setup with Nix

The flake supports `x86_64-linux` and `aarch64-linux`. It provides:

| Output | Contents |
| --- | --- |
| `packages.<system>.wayland-wispr` (`default`) | CPU build |
| `packages.<system>.wayland-wispr-vulkan` | Build with Vulkan GPU support for whisper.cpp |
| `homeManagerModules.wayland-wispr` (`default`) | User service, config file and models |
| `overlays.default` | Adds `pkgs.wayland-wispr` |
| `legacyPackages.<system>.whisperModels.<name>` | Pinned whisper.cpp ggml models |
| `devShells.<system>.default` | Development shell |

### Try it

```sh
flake=git+ssh://git@github.com/Makesesama/wayland-wispr.git
nix run $flake -- probe     # does the compositor support everything?

# the app never downloads models; fetch the default one with Nix (or curl)
mkdir -p ~/.local/share/wayland-wispr/models
ln -s "$(nix build --no-link --print-out-paths "$flake#whisperModels.\"base-q5_1\"")" \
  ~/.local/share/wayland-wispr/models/ggml-base-q5_1.bin

nix run $flake -- run       # terminal 1: overlay + resident process
nix run $flake -- toggle    # terminal 2: start, speak, run again to stop
```

### 1. Add the flake input

```nix
# flake.nix
inputs.wayland-wispr = {
  url = "git+ssh://git@github.com/Makesesama/wayland-wispr.git";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

### 2. Enable the home-manager module

```nix
# home-manager configuration
{ inputs, pkgs, ... }:
{
  imports = [ inputs.wayland-wispr.homeManagerModules.default ];

  services.wayland-wispr = {
    enable = true;

    # Vulkan build for device = "gpu"; omit for the CPU-only package.
    package = inputs.wayland-wispr.packages.${pkgs.stdenv.hostPlatform.system}.wayland-wispr-vulkan;

    settings = {
      transcription.whisper = {
        model = "large-v3-turbo-q5_0"; # fetched and linked automatically
        device = "gpu";
        languages = [ "de" "en" ];
        beam_size = 5;
      };
      cleanup.model = "qwen2.5:7b-instruct";
    };
  };
}
```

On a CPU-only machine, use the default package and a small model, for example
`model = "small-q5_1"` with `device = "cpu"`.

The module does the following:

- Installs the package into `home.packages`.
- Writes `settings` to `~/.config/wayland-wispr/config.toml`. The file is
  **validated at build time** with `wayland-wispr check-config`, so a typo
  fails `home-manager switch` instead of the service. With empty `settings`
  no file is written.
- Fetches the configured whisper model (pinned hash) and links it into
  `~/.local/share/wayland-wispr/models/`.
- Runs `wayland-wispr run` as the systemd user service
  `wayland-wispr.service`, started with `graphical-session.target`. A change
  to the config, a theme or a model restarts the service.

### 3. Export `WAYLAND_DISPLAY` to systemd

The service only starts when `WAYLAND_DISPLAY` is set in the systemd user
environment. Run this from your compositor's init, e.g. in the river init
script, before `graphical-session.target` is reached:

```sh
systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP
# or: dbus-update-activation-environment --systemd WAYLAND_DISPLAY XDG_CURRENT_DESKTOP
```

### 4. Bind keys

wayland-wispr does not grab global shortcuts. Bind its commands in your
compositor or window manager:

```sh
# river: toggle mode
riverctl map normal Super D      spawn 'wayland-wispr toggle'
riverctl map normal Super+Shift D spawn 'wayland-wispr cancel'

# river: push-to-talk instead (hold to record, release to process)
riverctl map         normal Super D spawn 'wayland-wispr start'
riverctl map -release normal Super D spawn 'wayland-wispr stop'
```

```ini
# sway
bindsym $mod+d exec wayland-wispr toggle
bindsym $mod+Shift+d exec wayland-wispr cancel

# Hyprland
bind = SUPER, D, exec, wayland-wispr toggle
bind = SUPER SHIFT, D, exec, wayland-wispr cancel
```

### 5. LLM cleanup with Ollama (optional)

The home-manager module doesn't start an LLM server. Use the NixOS
`services.ollama` module in the **system** configuration. It can pull models
declaratively:

```nix
# NixOS configuration
services.ollama = {
  enable = true;
  # ollama-vulkan (most GPUs), ollama-rocm (AMD), ollama-cuda (NVIDIA) or
  # ollama-cpu. The plain `ollama` package is CPU-only unless
  # nixpkgs.config.rocmSupport/cudaSupport is set.
  package = pkgs.ollama-vulkan;
  loadModels = [ "qwen2.5:7b-instruct" ]; # must match cleanup.model
  # environmentVariables.OLLAMA_KEEP_ALIVE = "30m"; # default unloads after 5 min
};
hardware.graphics.enable = true; # Vulkan driver, also for wayland-wispr-vulkan
```

The cleanup defaults (`http://127.0.0.1:11434/v1`, `qwen2.5:7b-instruct`)
already point at this server.

- The first rebuild pulls the model in the background. Follow the progress
  with `journalctl -u ollama-model-loader`, then test the setup with
  `wayland-wispr cleanup "ähm also das ist, äh, ein Test"`.
- No service ordering is needed. Requests are made per dictation, and
  `cleanup.preload` loads the model while you speak.
- Keep Ollama on loopback. `host = "0.0.0.0"` exposes an unauthenticated API
  to the network.
- Without a usable GPU, choose a small model (e.g. `qwen2.5:3b-instruct`) or
  set `cleanup.enabled = false`.

### Without home-manager

Install the package (or use `overlays.default`). Then put a config at
`~/.config/wayland-wispr/config.toml` (see
[config.example.toml](config.example.toml)) and a model in
`~/.local/share/wayland-wispr/models/`. Start `wayland-wispr run` from your
compositor's autostart.

### Home-manager module options

| Option | Default | Description |
| --- | --- | --- |
| `enable` | `false` | Enable the service. |
| `package` | this flake's `wayland-wispr` | Use `wayland-wispr-vulkan` for `device = "gpu"`. The module warns if they don't match. |
| `settings` | `{ }` | The [configuration](#configuration) as a Nix attrset. Validated at build time. Nix paths (e.g. themes) are copied to the store. |
| `models` | the configured `transcription.whisper.model` | Pinned whisper.cpp models to fetch and link into the models directory, e.g. extra ones for `bench`. Names from [nix/models.json](nix/models.json). |
| `extraModels` | `{ }` | Your own ggml files by name: `{ "large-v3-de" = ./ggml-large-v3-de.bin; }` → `ggml-large-v3-de.bin`. |
| `environmentFile` | `null` | Runtime `KEY=value` file for the service, e.g. the token named by `cleanup.api_key_env`. Not copied to the store. |
| `systemdTarget` | `"graphical-session.target"` | Target the service is started with. |

Available pinned models: `tiny`, `base`, `small`, `medium`, `large-v1`,
`large-v2`, `large-v3`, `large-v3-turbo`, in quantised variants (`-q5_0`/`-q5_1`,
`-q8_0`). For tiny through medium there are also English-only `.en`
variants. `nix/update-models.sh` regenerates the list.

Don't put secrets in `settings`, because it ends up in the world-readable Nix
store. Use `environmentFile` instead.

## Guide

### Dictating

| State | Overlay | What happens |
| --- | --- | --- |
| Recording | red dot, waveform, elapsed time | `toggle`/`stop` ends it. It stops by itself after `recording.max_seconds`. |
| Processing | steps: transcribing → polishing | `cancel` aborts at any time. |
| Ready | "Ready", or "Ready (not cleaned up)" | The text is copied and typed into the window that is focused **now**. |
| Error | error message | E.g. "no speech" for recordings that are too short or too quiet. |

Use `insert --raw` or `copy --raw` when the cleaned text is wrong: they use
the original transcript. With `injection.on_ready = "none"` nothing happens
automatically, and the result waits for an explicit `copy` or `insert`.
That is useful when you want to choose the target window first.

### Choosing a whisper model

| Machine | Suggested settings | Notes |
| --- | --- | --- |
| Discrete GPU | `model = "large-v3-turbo-q5_0"`, `device = "gpu"`, `beam_size = 5` | Vulkan build; ~0.3 s per clip, ~0.9 GiB VRAM |
| CPU only, or little VRAM | `model = "small-q5_1"` or `"base-q5_1"`, `device = "cpu"` | |

- Use multilingual models (not `.en`) for anything other than English.
- `languages = ["de", "en"]` limits auto-detection to these languages. A
  fixed `language` that doesn't match the speech makes Whisper **translate**
  into it.
- `initial_prompt` gives Whisper hints about names and jargon you use often.
- `keep_loaded = "idle"` frees (V)RAM after `idle_unload_secs`, e.g. for games
  or a local LLM. `preload` loads the model while you are still speaking.
- GPU mode never falls back to the CPU silently. A missing GPU or running out
  of memory is reported as an error.

Measure on your own hardware before choosing:

```sh
wayland-wispr bench recording.wav --device gpu
wayland-wispr bench recording.wav --model small-q5_1 --device cpu
```

### LLM cleanup

Cleanup sends the transcript to an OpenAI-compatible chat completions server
that you run yourself. The model's reply is checked before it is used:

- `<think>` blocks, quotes, labels and code fences are stripped.
- Refusals, empty replies, and replies much longer or shorter than the
  transcript are rejected.
- Replies made mostly of words that were never dictated are rejected, e.g.
  translations, answers or rewrites.

If the server is down, too slow or fails these checks, the raw transcript is
used. Whisper hallucinations at the end of a transcript, such as "Untertitel
der Amara.org-Community", are removed beforehand.

- **Thinking models** (Qwen3.x) need `reasoning_effort = "none"`. Otherwise
  they spend the whole time budget thinking.
- **VRAM:** Whisper and the LLM share the GPU. Run `ollama ps` and check that
  it shows `100% GPU`. Models that don't fit are split onto the CPU and become
  slow. With `large-v3-turbo` loaded, a 16 GiB card easily fits a 7B model.
- **Remote providers:** set `base_url` (must use https) and `api_key_env`, and
  provide the key through `environmentFile`. Transcripts are then sent to that
  server.
- **Prompt tuning:** copy [src/cleanup/prompt.toml](src/cleanup/prompt.toml)
  and set `prompt_file`. Small models follow examples much better than rules.
  Compare models and prompts with `wayland-wispr cleanup-eval fixtures.toml`
  (format: [dev/cleanup-fixtures.toml](dev/cleanup-fixtures.toml)).

### Text insertion

With the default `method = "type"`, the text is typed through
`zwp_virtual_keyboard_v1` with a short-lived keymap. It types the same way in
editors, browsers and terminals.

- **Line breaks** are typed as Shift+Return. A terminal may still run a
  command on it, so set `newline = "space"` if you dictate into shells.
- **Emoji** may not type in Chromium-based apps. The clipboard always has the
  exact text.
- **`method = "paste"`** sends `paste_chord` instead. Terminals usually need
  `"ctrl+shift+v"`.
- **The clipboard is replaced** and not restored.
- **Success** only means the compositor received the keys. Wayland can't
  confirm that the app inserted them. If insertion fails, the result stays
  ready for a manual `insert` or `copy`.

### Themes

The overlay is styled with GTK CSS. `overlay.theme` loads your stylesheet on
top of the built-in one, and your rules override any built-in rule.

```sh
wayland-wispr default-theme > ~/.config/wayland-wispr/theme.css   # start from the default
```

```toml
[overlay]
theme = "theme.css"            # one stylesheet, relative to config.toml

# or light/dark variants, switched live
[overlay.theme]
light = "light.css"
dark = "dark.css"              # an omitted variant uses the default theme
```

```nix
# home-manager: Nix paths work, and the bundled example is in the package
services.wayland-wispr.settings.overlay.theme = {
  light = "${config.services.wayland-wispr.package}/share/wayland-wispr/themes/light.css";
  dark = ./wispr-dark.css;
};
```

- **Classes:** `.pill` carries the state classes `.recording`, `.processing`,
  `.ready` and `.error`. Inside it are `.dot`, `.wave` (bar colour), `.time`,
  `.status`, `.detail`, `.step` (with `.pending`, `.active`, `.done`) and
  `spinner.busy`.
- **Light/dark switching:** variants follow
  [darkman](https://darkman.whynothugo.nl/) over D-Bus. Without darkman they
  follow the XDG portal's `color-scheme`, and without either they use `dark`.
- **Your rules beat every built-in rule** for the same property, regardless of
  selector specificity. A plain `.dot { background-color: … }` therefore also
  replaces the recording and processing colours, so restate the state rules
  as [themes/light.css](themes/light.css) does.
- **Background:** a background colour in your theme replaces
  `overlay.opacity`. `overlay.scale` only scales the built-in sizes.
- **Size:** the pill's width is measured once at startup. Variants should
  change colours, not sizes or fonts.
- **Shadows** need room: give `window.wispr` some padding. That padding adds to
  `overlay.margin`.

### Troubleshooting

| Symptom | Check |
| --- | --- |
| Service doesn't start | `journalctl --user -u wayland-wispr`. Is `WAYLAND_DISPLAY` in `systemctl --user show-environment`? |
| No overlay / nothing typed | `wayland-wispr probe`: the compositor lacks a required protocol |
| "no speech" | `wayland-wispr devices`, then set `audio.device`. Lower `audio.silence_threshold_dbfs` for quiet microphones. |
| Wrong language or translated output | Set `languages = ["de", "en"]` and keep `language = "auto"` |
| "Ready (not cleaned up)" | `wayland-wispr status` shows `cleanup_error`. Is the LLM server running and the model pulled? |
| First characters missing or keybinding modifiers leak into the text | Increase `injection.pre_insert_delay_ms` |
| Config rejected | `wayland-wispr check-config` shows the error and the model and theme files it resolves |

Logs use `RUST_LOG` (default `info`), e.g. `RUST_LOG=debug wayland-wispr run`.

## Configuration

The config file is `$XDG_CONFIG_HOME/wayland-wispr/config.toml`, or the file
given with `--config` / `WAYLAND_WISPR_CONFIG`. All keys are optional.
Unknown keys are errors. Relative paths are relative to the config file. Only
`run` reads the config, so restart the service after changing it.
[config.example.toml](config.example.toml) is a commented template.

### `[overlay]`

| Key | Default | Description |
| --- | --- | --- |
| `layer` | `"overlay"` | Layer-shell layer: `"overlay"` or `"top"` |
| `position` | `"bottom"` | `top-left`, `top`, `top-right`, `left`, `center`, `right`, `bottom-left`, `bottom`, `bottom-right` |
| `margin` | `48` | Distance in logical pixels from the anchored edge(s) |
| `margin_x`, `margin_y` | `margin` | Separate horizontal and vertical margins |
| `output` | compositor's choice | Output connector name, e.g. `"DP-1"` |
| `scale` | `1.0` | Size of the pill (0.5–4). Margins are not scaled. |
| `opacity` | `0.88` | Background opacity of the pill. Text stays opaque. |
| `theme` | built-in | A CSS path, or `{ light = …, dark = … }`; see [Themes](#themes) |

### `[recording]`

| Key | Default | Description |
| --- | --- | --- |
| `max_seconds` | `120` | Recording stops automatically after this many seconds (1–3600) |

### `[audio]`

| Key | Default | Description |
| --- | --- | --- |
| `host` | `"auto"` | `"auto"` (PipeWire if reachable, else ALSA), `"pipewire"` or `"alsa"` |
| `device` | system default | Name or ID from `wayland-wispr devices` |
| `min_seconds` | `0.3` | Shorter recordings are rejected |
| `silence_threshold_dbfs` | `-50.0` | Recordings quieter than this are rejected as "no speech" |

### `[transcription]` and `[transcription.whisper]`

| Key | Default | Description |
| --- | --- | --- |
| `backend` | `"whisper"` | `"whisper"` (local whisper.cpp) or `"fake"` (placeholder text for UI tests) |
| `whisper.model` | `"base-q5_1"` | Model name, resolved to `<models_dir>/ggml-<model>.bin` |
| `whisper.models_dir` | `$XDG_DATA_HOME/wayland-wispr/models` | Directory for named models |
| `whisper.model_path` | – | Explicit model file. Overrides `model` and `models_dir`. |
| `whisper.device` | `"cpu"` | `"cpu"` or `"gpu"` (needs the Vulkan build) |
| `whisper.gpu_device` | `0` | GPU index as listed by `bench` |
| `whisper.flash_attn` | `true` | Use flash attention |
| `whisper.threads` | cores, at most 8 | CPU threads |
| `whisper.language` | `"auto"` | `"auto"` or a fixed code such as `"de"` (other speech gets translated) |
| `whisper.languages` | `[]` (any) | With `"auto"`: only choose among these |
| `whisper.initial_prompt` | – | Vocabulary hints, e.g. `"Names: makussu, riverfx, fenster."` |
| `whisper.beam_size` | `1` | `1` = greedy (fast), `5` = beam search (more accurate, slower) |
| `whisper.keep_loaded` | `"idle"` | `"always"`, `"idle"`, or `"never"` (unload after each use) |
| `whisper.idle_unload_secs` | `300` | With `"idle"`: free (V)RAM after this long unused |
| `whisper.preload` | `true` | Start loading the model when recording starts |

### `[cleanup]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Run LLM cleanup after transcription |
| `base_url` | `"http://127.0.0.1:11434/v1"` | OpenAI-compatible API base. `/chat/completions` is appended. Non-local hosts need https. |
| `model` | `"qwen2.5:7b-instruct"` | Model name as the server knows it |
| `reasoning_effort` | – | `"none"` for thinking models such as Qwen3.x |
| `api_key_env` | – | Environment variable holding a bearer token |
| `allow_insecure_http` | `false` | Allow `http://` to non-local hosts (sends text unencrypted) |
| `temperature` | `0.0` | Sampling temperature |
| `connect_timeout_ms` | `1000` | Connection timeout |
| `timeout_secs` | `30` | Deadline per request, including a cold model load |
| `min_words` | `4` | Shorter transcripts are used as-is |
| `preload` | `true` | Ask a local server to load the model when recording starts |
| `prompt_file` | built-in | TOML file that replaces the built-in prompt and examples |

### `[injection]`

| Key | Default | Description |
| --- | --- | --- |
| `on_ready` | `"insert"` | When a result is ready: `"insert"` (copy and type), `"copy"`, or `"none"` |
| `method` | `"type"` | `"type"` (virtual keyboard) or `"paste"` (send `paste_chord`) |
| `newline` | `"shift_enter"` | How line breaks are typed: `"shift_enter"`, `"enter"` or `"space"` |
| `type_delay_ms` | `2` | Pause after each typed character |
| `paste_chord` | `"ctrl+v"` | With `"paste"`, e.g. `"ctrl+shift+v"` for terminals |
| `pre_insert_delay_ms` | `150` | Time to release the triggering keybinding before typing |
| `key_delay_ms` | `10` | Delay after keymap changes and while a chord is held |

### `[debug]`

| Key | Default | Description |
| --- | --- | --- |
| `fake_processing_ms` | `1500` | How long the `fake` backend "processes" |
| `fake_transcript` | placeholder | Text returned by the `fake` backend |
| `save_recordings_dir` | – | Save every normalised recording (16 kHz mono WAV). Off by default. |

## Command reference

```text
wayland-wispr run                 resident process with overlay (the service runs this)
wayland-wispr toggle              start recording / stop and process
wayland-wispr start | stop        push-to-talk press / release
wayland-wispr cancel              discard recording, processing or result (alias: abort)
wayland-wispr insert [--raw]      copy the ready result and type/paste it into the focused app
wayland-wispr copy [--raw]        copy the ready result to the clipboard
wayland-wispr status              JSON state of the running instance (no transcript text)

wayland-wispr probe [--json]      check the compositor's Wayland protocols
wayland-wispr devices [--json]    list microphones
wayland-wispr check-config        validate the config, show resolved model and theme files
wayland-wispr default-theme       print the built-in overlay CSS
wayland-wispr bench <wav>         load time, latency, RAM/VRAM of a whisper model
                                  [--model, --model-path, --device, --beam-size, --language, --threads, --runs]
wayland-wispr cleanup [text]      clean up a text (or stdin) with the configured LLM
wayland-wispr cleanup-eval <file> compare cleanup models/prompts on a fixtures file
```

All commands accept `--config <path>`. Client commands talk to `run` over
`$XDG_RUNTIME_DIR/wayland-wispr/control.sock`, a private socket that only
accepts connections from the same user.

## Development

```sh
nix develop        # or: direnv allow
```

The dev shell provides the Rust toolchain, GTK4 and gtk4-layer-shell,
PipeWire/ALSA, the whisper.cpp build tools (CMake, Clang, Vulkan headers,
`glslc`) and `wayland-info`. On x86_64 it sets `GGML_AVX2` and related flags,
because Nix's compiler wrapper strips `-march=native`. Without them
whisper.cpp would build without SIMD and run about 10× slower. After changing
`GGML_*` variables, run `cargo clean -p whisper-rs-sys`.

Inside the shell, `WAYLAND_WISPR_CONFIG` points at
[dev/config.toml](dev/config.toml). That config saves recordings to
`dev/recordings/` and expects models in `dev/models/` (both gitignored).

```sh
cargo run --features whisper-vulkan -- run   # terminal 1 (or without the feature for CPU)
cargo run -q -- toggle                       # terminal 2: start, then stop → transcribe → type
cargo run -q -- status
```

To test the UI and insertion without a model, set
`transcription.backend = "fake"`.

```sh
cargo test                                                    # no model needed
WAYLAND_WISPR_TEST_MODEL=dev/models/ggml-base-q5_1.bin cargo test   # + real-model test
nix flake check --all-systems --no-build
nix fmt -- flake.nix nix/*.nix
```

| Nix file | Purpose |
| --- | --- |
| `nix/default.nix` | Package. `vulkanSupport` enables the GPU build, `ggmlCpuFlags` overrides the SIMD level. |
| `nix/ggml-cpu.nix` | `GGML_*` flags shared by the package and the shell (x86-64-v3 on x86_64) |
| `nix/shell.nix` | Dev shell |
| `nix/service.nix` | home-manager module |
| `nix/models.nix`, `nix/models.json` | Pinned whisper.cpp models from Hugging Face |
| `nix/update-models.sh` | Regenerates `models.json` from the Hugging Face API |

## License

[MIT](LICENSE)
