# wayland-wispr

A planned Wayland dictation overlay: record speech, transcribe locally or through
an OpenAI-compatible API, optionally polish with an LLM, then insert the result
into the active application.

**Status:** early but usable end to end except transcription. The resident
process, layer-shell overlay, control socket, session state machine, Wayland
capability probe, microphone capture (native PipeWire, ALSA fallback), and native
clipboard/paste insertion work. Transcription and cleanup are **not implemented
yet** — a recording produces a placeholder transcript stating its duration
(or `debug.fake_transcript`). No Nix app package yet.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the proposed design, backend choices,
Wayland constraints, and phased implementation checklist with acceptance criteria.
[RESEARCH.md](RESEARCH.md) compares native HTTP/SDK, local Whisper/VRAM, and
clipboard/virtual-keyboard options with source links.

## Usage

```sh
cargo run -- probe            # which native protocols the compositor offers
cargo run -- run              # resident process + overlay (needs wlr layer-shell)

# from another terminal / a WM keybinding:
wayland-wispr toggle          # start recording / stop and process
wayland-wispr start | stop    # push-to-talk press / release
wayland-wispr cancel          # discard recording, processing, or ready result
wayland-wispr status          # JSON state (never contains transcript text)
wayland-wispr devices         # list microphones of the audio host
wayland-wispr copy            # put the ready result on the clipboard
wayland-wispr insert          # copy, then send the paste chord to the focused app
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
| `src/injection/` | Data-control clipboard (`wl-clipboard-rs`), XKB keymap/chord planning (`xkbcommon`), `zwp_virtual_keyboard_v1` paste chord (`wayland-protocols-misc`) |
| `src/config.rs` | TOML settings with validation |

**Insertion notes.** `copy` and `insert` replace your clipboard (no restoration).
`insert` targets whatever is focused *when it runs* and reports success once the
chord was delivered to the compositor — Wayland gives no acknowledgement that
the app actually pasted. On failure the result stays ready for a manual retry;
nothing is retried automatically. Terminals usually need
`injection.paste_chord = "ctrl+shift+v"`. The paste uses a fixed US keymap on a
short-lived virtual keyboard, independent of your layout.

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
- CMake, Ninja, Clang, and libclang for CPU-only `whisper-rs` builds and bindgen.
- OpenSSL for HTTP clients using native TLS; `reqwest` with rustls is also an option.
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
cargo run -q -- toggle      # start; toggle again to stop → Ready after fake_processing_ms
cargo run -q -- status
cargo run -q -- insert      # focus your target window first, or use a keybinding
cargo run -q -- copy
cargo run -q -- cancel
```

The dev config saves every normalized recording (16 kHz mono WAV, private
permissions) to `dev/recordings/` (gitignored) so you can listen to what a model
would receive, e.g. `pw-play dev/recordings/<file>.wav`. Recordings shorter than
`audio.min_seconds` or quieter than `audio.silence_threshold_dbfs` end in an
error instead of a result. Uncomment `debug.fake_transcript` to paste fixed test
text (Unicode, emoji, newline, tab) instead of the duration placeholder.
Outside the dev config, recordings are never written to disk.
Config changes require restarting `run`; client commands don't read the config.

Check the environment:

```sh
nix flake check --all-systems --no-build
nix fmt -- flake.nix
nix develop -c sh -c 'rustc --version; cargo --version; pkg-config --modversion gtk4 gtk4-layer-shell-0 alsa'
```

From a running Wayland session, check compositor capabilities:

```sh
wayland-info | grep -E 'layer_shell|virtual_keyboard|data_control'
```

The dev shell does not enable host services, change input permissions, install
models, or configure API keys. Keep credentials out of Nix files and Git; use
runtime environment variables or a user-local secret store. Local Whisper model
files should live outside the repository. GPU acceleration is not configured.

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
