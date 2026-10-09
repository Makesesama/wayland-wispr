# wayland-wispr

A planned Wayland dictation overlay: record speech, transcribe locally or through
an OpenAI-compatible API, optionally polish with an LLM, then insert the result
into the active application.

**Status:** usable end to end with local transcription. The resident
process, layer-shell overlay, control socket, session state machine, Wayland
capability probe, microphone capture (native PipeWire, ALSA fallback), local
whisper.cpp transcription (CPU or Vulkan GPU), LLM cleanup through a
user-run OpenAI-compatible server (e.g. Ollama), and native clipboard/paste
insertion work. The remote transcription API backend is **not implemented
yet**. Nix package and home-manager service: see [Nix](#nix).

See [ARCHITECTURE.md](ARCHITECTURE.md) for the proposed design, backend choices,
Wayland constraints, and phased implementation checklist with acceptance criteria.
[RESEARCH.md](RESEARCH.md) compares native HTTP/SDK, local Whisper/VRAM, and
clipboard/virtual-keyboard options with source links.

## Usage

```sh
cargo run -- probe            # which native protocols the compositor offers
cargo run -- run              # resident process + overlay (needs wlr layer-shell)

# from another terminal / a WM keybinding:
wayland-wispr toggle          # start recording / stop, process, copy + type into focused field
wayland-wispr start | stop    # push-to-talk press / release
wayland-wispr cancel          # discard recording, processing, or ready result
wayland-wispr status          # JSON state (never contains transcript text)
wayland-wispr devices         # list microphones of the audio host
wayland-wispr copy            # put the ready result on the clipboard
wayland-wispr insert          # copy, then type (or paste) into the focused app
wayland-wispr insert --raw    # same with the raw transcript (also `copy --raw`)
wayland-wispr check-config    # validate the config, show which model it resolves to
wayland-wispr bench <wav>     # measure the configured whisper model on this machine
wayland-wispr cleanup "text"  # run LLM cleanup on a text (or stdin)
wayland-wispr cleanup-eval dev/cleanup-fixtures.toml  # compare models/prompts
```

The control socket is `$XDG_RUNTIME_DIR/wayland-wispr/control.sock` (private
directory, same-UID peers only). Logging uses `RUST_LOG` (default `info`).
Optional settings: `$XDG_CONFIG_HOME/wayland-wispr/config.toml`, see
[config.example.toml](config.example.toml).

| Module | Purpose |
| --- | --- |
| `src/main.rs` | CLI (`clap`) |
| `src/app.rs` | Resident process: controller thread, IPC thread, effect workers |
| `src/controller.rs` | Pure session state machine with session IDs (unit-tested) |
| `src/ipc.rs` | Versioned, size-bounded JSON-line Unix socket protocol |
| `src/ui.rs` | GTK4 layer-shell pill (`KeyboardMode::None`, exclusive zone 0) |
| `src/probe.rs` | Native `wayland-client` global probe on its own connection |
| `src/audio/` | `cpal` capture thread (lock-free `rtrb` ring from the realtime callback), downmix, `rubato` resampling to 16 kHz mono, silence/length checks, `hound` WAV |
| `src/injection/` | Data-control clipboard (`wl-clipboard-rs`), XKB keymap/chord planning (`xkbcommon`), `zwp_virtual_keyboard_v1` direct typing and paste chord (`wayland-protocols-misc`) |
| `src/transcription/` | Backend interface + cancellation; `whisper.rs`: whisper.cpp worker thread (lazy load, preload on record, idle unload, de/en-restricted language detection); `model.rs`: model resolution/validation |
| `src/cleanup/` | LLM cleanup: blocking `ureq` client for streamed `/chat/completions` (cancellable, output-capped), built-in few-shot prompt (`prompt.toml`), reply sanitizing/guards with raw fallback, `cleanup`/`cleanup-eval` commands |
| `src/bench.rs` | `bench` subcommand: load time, latency, RSS and VRAM per model/device |
| `src/config.rs` | TOML settings with validation |

## Local transcription (whisper)

Models are whisper.cpp ggml files, looked up as
`$XDG_DATA_HOME/wayland-wispr/models/ggml-<model>.bin` (or `models_dir` /
`model_path`). The app never downloads anything; fetch models yourself:

```sh
mkdir -p ~/.local/share/wayland-wispr/models && cd $_
curl -LO https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin
```

Small vs. large is a per-machine config choice, not a different code path:

| Machine | Suggested | Notes |
| --- | --- | --- |
| Discrete GPU (e.g. RX 9070 XT) | `large-v3-turbo-q5_0`, `device = "gpu"`, `beam_size = 5` | build with `--features whisper-vulkan`; ~0.3 s per clip, ~0.9 GiB VRAM |
| CPU only / little VRAM | `small-q5_1` or `base-q5_1`, `device = "cpu"` | |

Use multilingual models (not `.en`) for German. `languages = ["de", "en"]`
restricts auto-detection; a *fixed* `language` that doesn't match the speech
makes Whisper translate. `keep_loaded = "idle"` frees (V)RAM after
`idle_unload_secs` (e.g. for a local LLM or games); `preload` loads the model
while you are still speaking. GPU mode never silently falls back to CPU: a
missing GPU or out-of-memory is an error suggesting a smaller model or CPU.

Measure before choosing (prints the transcript to stdout):

```sh
cargo run --release --features whisper-vulkan -- bench dev/recordings/<file>.wav --device gpu
cargo run --release -- bench dev/recordings/<file>.wav --model small-q5_1 --device cpu
```

`cancel` stops inference at the next whisper.cpp graph boundary; a running
encoder pass (≈ one 30 s window) cannot be interrupted, so cancelling a long CPU
transcription can take a moment.

## LLM cleanup

After transcription, the text goes to an OpenAI-compatible chat completions
server that **you run yourself**. Cleanup removes fillers and stutters,
resolves self-corrections ("at two, actually three" becomes "at three"), fixes
punctuation, turns spoken "new line"/"neuer Absatz"/"Komma" into formatting,
and writes enumerations as lists. It is on by default and points at Ollama:

```sh
ollama serve                      # or services.ollama on NixOS, see "Ollama for LLM cleanup"
ollama pull qwen2.5:7b-instruct   # the default cleanup.model
```

If the server is not running, slow, or replies with something unusable, the
**raw transcript is used**. The overlay then shows "Ready (not cleaned up)", and
`status` reports `"cleanup": "failed"` with the reason. Before Ready, the overlay
shows "Polishing…". When the cleaned text is wrong, `insert --raw` / `copy --raw`
use the original transcript. Transcripts shorter than `min_words` skip the LLM.

The model's reply is checked before it is used. The checks strip `<think>`
blocks, quotes, labels, and code fences. They reject refusals, empty replies,
replies much longer or shorter than the transcript, and replies made mostly of
words that were never dictated (translations, answers, rewrites). Trailing
Whisper hallucinations such as "Untertitel der Amara.org-Community" are removed
before cleanup.

Notes:

- **Thinking models** (Qwen3.x, Qwen3.8) need `reasoning_effort = "none"`;
  otherwise they spend the whole budget thinking and the raw text is used.
- **Ollama unloads idle models after 5 minutes.** `preload = true` sends a tiny
  request when recording starts, so the model loads while you speak (local
  servers only, so a paid API is not billed for it). Setting
  `OLLAMA_KEEP_ALIVE` on the server also works.
- **Models that don't fit the GPU are split onto the CPU** by Ollama and become
  slow rather than failing. Check `ollama ps` for `100% GPU`. With Whisper
  large-v3-turbo loaded, a 16 GiB card fits a 7B model easily. Qwen3.8-27B fits
  only at about 3 bits (`hf.co/unsloth/Qwen3.8-27B-GGUF:UD-IQ3_XXS`).
- A non-local `base_url` must use https; transcripts are then sent to that
  server. `api_key_env` names the environment variable holding a bearer token.
- Tune the prompt by copying [src/cleanup/prompt.toml](src/cleanup/prompt.toml)
  and setting `prompt_file`. Small models follow examples much better than
  rules, so add an example rather than another "do not". Measure with
  `cleanup-eval`; `--model`, `--base-url`, `--reasoning-effort`, and
  `--prompt-file` override the config.

**Insertion notes.** By default (`injection.on_ready = "insert"`) the result
is copied to the clipboard and then typed into whatever is focused *when
processing finishes* — no extra command. Typing goes through
`zwp_virtual_keyboard_v1` with a short-lived keymap per text that maps each
character to a printable key position, so it works the same in editors,
browsers, and terminals and is independent of your layout and of app paste
shortcuts. Line breaks are typed as Shift+Return (`injection.newline`); in a
terminal that may still run a command, so prefer `"space"` if you dictate into
shells. Emoji/supplementary characters may not type in Chromium-based apps; the
clipboard always has the exact text. `method = "paste"` sends `paste_chord`
instead (terminals: `"ctrl+shift+v"`). `on_ready = "none"` restores the
explicit flow: the result stays ready for `copy`/`insert`.

The clipboard is replaced (no restoration). Success means the keys were
delivered to the compositor — Wayland gives no acknowledgement that the app
inserted them. On failure the result stays ready for a manual `insert`/`copy`;
nothing is retried automatically.

## Nix

Everything lives in [nix/](nix/); `flake.nix` only wires it up.

| File | Purpose |
| --- | --- |
| `nix/default.nix` | Package (`callPackage`-style). `vulkanSupport = true` builds whisper.cpp with Vulkan; `ggmlCpuFlags` overrides the CPU SIMD level |
| `nix/ggml-cpu.nix` | `GGML_*` flags shared by package and shell (x86-64-v3 on x86_64, because the Nix compiler wrapper strips `-march=native`) |
| `nix/shell.nix` | Dev shell (`nix develop`, or `nix-shell nix/shell.nix`) |
| `nix/service.nix` | home-manager module: systemd user service, config file, models |
| `nix/models.nix`, `nix/models.json` | Pinned whisper.cpp ggml models from Hugging Face (fixed-output fetches) |
| `nix/update-models.sh` | Regenerates `models.json` from the Hugging Face API (hashes from LFS metadata, no downloads) |

Flake outputs: `packages.<system>.{wayland-wispr,wayland-wispr-vulkan,default}`,
`overlays.default`, `homeManagerModules.{wayland-wispr,default}`,
`legacyPackages.<system>.whisperModels.<name>`, `devShells`, `checks`.

```nix
# flake inputs
wayland-wispr.url = "git+ssh://git@git.makussu.de:2122/Makussu/wayland-wispr.git";
wayland-wispr.inputs.nixpkgs.follows = "nixpkgs";

# home-manager
imports = [ inputs.wayland-wispr.homeManagerModules.default ];
services.wayland-wispr = {
  enable = true;
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
  models = [ "large-v3-turbo-q5_0" "small-q5_1" ]; # optional: extra models for `bench`
};
```

- **Config:** `settings` becomes `~/.config/wayland-wispr/config.toml` and is
  checked at build time with `wayland-wispr check-config`, so typos and invalid
  values fail `home-manager switch` instead of the service. With empty
  `settings`, no file is written.
- **Models:** `models` (default: the configured `transcription.whisper.model`)
  are fetched from the pinned whisper.cpp repo and linked into
  `~/.local/share/wayland-wispr/models/`, the directory the app already uses.
  Manually downloaded models there keep working. `extraModels` links your own
  files by name. Setting `model_path`/`models_dir` bypasses the links (the
  module warns).
- **Service:** `wayland-wispr.service` runs `wayland-wispr run` with
  `graphical-session.target`. Your session must import `WAYLAND_DISPLAY` into
  the systemd user environment (e.g. `systemctl --user import-environment
  WAYLAND_DISPLAY` from the river init). A config or model change restarts it.
  Secrets for `cleanup.api_key_env` go in `environmentFile`, never `settings`.
  The module does not run Ollama; use `services.ollama` on the system.
- Bind `wayland-wispr toggle`, `insert`, and `cancel` in the WM. The package is
  added to `home.packages`.

### Ollama for LLM cleanup

Cleanup needs an OpenAI-compatible server, and the home-manager module does not
start one. Use the NixOS `services.ollama` module. Unlike the home-manager
module of the same name, it can pull models declaratively. It goes in the
**system** configuration:

```nix
# NixOS configuration
services.ollama = {
  enable = true;
  # GPU backend: ollama-vulkan (most GPUs), ollama-rocm (AMD), ollama-cuda
  # (NVIDIA), ollama-cpu. The plain `ollama` package is CPU-only unless
  # nixpkgs.config.rocmSupport/cudaSupport is set.
  package = pkgs.ollama-vulkan;
  # Pulled by ollama-model-loader.service after ollama starts; must match
  # services.wayland-wispr.settings.cleanup.model.
  loadModels = [ "qwen2.5:7b-instruct" ];
  # host = "127.0.0.1"; port = 11434;  # defaults, matching cleanup.base_url
  # environmentVariables.OLLAMA_KEEP_ALIVE = "30m";  # default unloads after 5 min
};
hardware.graphics.enable = true; # Vulkan driver, also for wayland-wispr-vulkan
```

```nix
# home-manager: the defaults already point at this server
services.wayland-wispr.settings.cleanup = {
  base_url = "http://127.0.0.1:11434/v1"; # default
  model = "qwen2.5:7b-instruct";          # default
  # reasoning_effort = "none";            # for thinking models such as Qwen3.x
};
```

Notes:

- The first rebuild pulls the model in the background. Check progress with
  `journalctl -u ollama-model-loader`. Until the pull finishes, cleanup fails
  and the raw transcript is used. Check the result with
  `wayland-wispr cleanup "ähm also das ist, äh, ein Test"`.
- `wayland-wispr.service` is a user unit and cannot be ordered after the system
  `ollama.service`. No ordering is needed: requests are made per dictation, and
  `cleanup.preload` loads the model while you speak.
- Whisper and the LLM share VRAM when both run on the GPU. Check `ollama ps`
  for `100% GPU`; see the notes under [LLM cleanup](#llm-cleanup) for sizing.
- Ollama listens on loopback by default. Keep it that way: setting `host` to
  `0.0.0.0` exposes an unauthenticated API to the network. Only set
  `openFirewall` if that is intended.
- On machines without a usable GPU, choose a small model (e.g.
  `qwen2.5:3b-instruct`) or set `cleanup.enabled = false`.

## Development

```sh
nix develop
# Or, with direnv installed:
direnv allow
```

The flake pins nixpkgs in `flake.lock` and provides development shells for
`x86_64-linux` and `aarch64-linux`. It includes:

- Rust, Cargo, rustfmt, Clippy, rust-analyzer, and Rust library sources.
- GTK4 and gtk4-layer-shell for a Wayland overlay, plus GTK schemas and icons.
- PipeWire and ALSA libraries for CPAL microphone capture. `audio.host = "auto"`
  uses the native PipeWire host when its socket is reachable and logs why it
  fell back to ALSA otherwise.
- CMake, Ninja, Clang, and libclang for `whisper-rs` builds and bindgen, plus
  Vulkan headers/loader and `glslc` for `--features whisper-vulkan`. On x86_64
  the shell sets `GGML_AVX2` & co.: Nix's compiler wrapper strips
  `-march=native`, which would otherwise build whisper.cpp without SIMD
  (~10x slower). After changing `GGML_*` variables run
  `cargo clean -p whisper-rs-sys`.
- OpenSSL (unused so far; the cleanup HTTP client uses rustls).
- Wayland/xkbcommon libraries and `wayland-info` for protocol inspection.
  Clipboard/input will use native Rust libraries, not subprocess tools.

### Iterating from this repo

Inside the dev shell (direnv or `nix develop` from the repo root),
`WAYLAND_WISPR_CONFIG` points at [dev/config.toml](dev/config.toml), so the app
uses the repo's config instead of `~/.config`. `--config <path>` still wins;
outside the dev shell the XDG path applies. The resident process logs which
config it loaded.

```sh
# terminal 1: edit dev/config.toml, then (re)start
cargo run -- run

# terminal 2: drive it (or bind these in fenster to the absolute
# path of target/debug/wayland-wispr)
cargo run -q -- toggle      # start; toggle again to stop → transcribes → types into focus
cargo run -q -- status
cargo run -q -- insert      # focus your target window first, or use a keybinding
cargo run -q -- copy
cargo run -q -- cancel
```

The dev config saves every normalized recording (16 kHz mono WAV, private
permissions) to `dev/recordings/` (gitignored) so you can listen to what a model
would receive, e.g. `pw-play dev/recordings/<file>.wav`. Recordings shorter than
`audio.min_seconds` or quieter than `audio.silence_threshold_dbfs` end in an
error instead of a result. Set `transcription.backend = "fake"` (and optionally
`debug.fake_transcript`) to test the UI/insertion without a model.

The dev config expects `dev/models/ggml-large-v3-turbo-q5_0.bin` on the GPU, so
run the resident process with `cargo run --features whisper-vulkan -- run`
(client commands work from either build). `cargo test` needs no model;
`WAYLAND_WISPR_TEST_MODEL=dev/models/ggml-base-q5_1.bin cargo test` adds a
real-model cancellation test.
Outside the dev config, recordings are never written to disk.
Config changes require restarting `run`; client commands don't read the config.

Check the environment:

```sh
nix flake check --all-systems --no-build
nix fmt -- flake.nix nix/*.nix   # nixfmt needs file arguments
nix develop -c sh -c 'rustc --version; cargo --version; pkg-config --modversion gtk4 gtk4-layer-shell-0 alsa'
```

From a running Wayland session, check compositor capabilities:

```sh
wayland-info | grep -E 'layer_shell|virtual_keyboard|data_control'
```

The dev shell does not enable host services, change input permissions, install
models, or configure API keys. Keep credentials out of Nix files and Git; use
runtime environment variables or a user-local secret store. Local Whisper model
files live outside the repository (or in gitignored `dev/models/`). The GPU
build needs a host Vulkan driver (RADV on NixOS via `hardware.graphics`).

## Suggested implementation

**Rust + `gtk4` / `gtk4-layer-shell`** is the recommended first implementation:
mature widgets with a small non-focus-stealing recording/status overlay. It
requires a compositor exposing `zwlr_layer_shell_v1` (not GNOME/Mutter). Keep
GTK on its main thread and audio/network/model work off that thread.

1. Build an overlay with keyboard mode `None` and no exclusive screen zone.
2. Add a CLI `toggle` command communicating with the running app over a Unix
   socket; bind it in the compositor/window manager. Foreign-toplevel protocols
   do not provide global shortcuts. The GlobalShortcuts portal is a possible
   alternative only where the desktop's portal backend implements it.
3. Capture audio with `cpal`, encode WAV, and transcribe with `whisper-rs` or
   an OpenAI-compatible `/audio/transcriptions` endpoint. Resample to the local
   model's required sample rate (typically 16 kHz for Whisper).
4. Optionally send the transcript to `/chat/completions` for cleanup. Configure
   transcription and cleanup endpoints/models independently, and make remote
   processing explicit: it sends audio and/or text off the machine.
5. Insert through native data-control clipboard + native virtual-keyboard paste
   where supported. Prefer `wl-clipboard-rs` directly for Wayland-only behavior;
   `arboard` with `wayland-data-control` is a native alternative, but can fall
   back to X11. Use `wayland-client` / `wayland-protocols-misc` for keyboard
   events, with a configurable paste chord (terminals often need `Ctrl+Shift+V`).
   Investigate direct Unicode typing in a native spike. Missing protocols are
   explicit errors; subprocess, X11, and uinput workarounds are out of scope.

Start with the overlay and native clipboard/virtual-keyboard capability spike
before building the transcription pipeline. Use explicit insertion initially:
Wayland cannot universally identify or restore the original focused text field,
so automatic insertion after processing could target the wrong application.
