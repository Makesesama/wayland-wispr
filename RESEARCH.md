# Native implementation research

Research snapshot: 2026-10-09. Three read-only research agents investigated HTTP,
local speech inference, and Wayland clipboard/input. Recommendations below are
proposals for the first implementation, not a claim that code has been tested.
No Cargo package exists yet.

**Scope:** native Rust libraries and protocol clients. No runtime `wl-copy`,
`wl-paste`, `wtype`, `ydotool`, shell scripts, or privileged input workaround.
Diagnostic commands in development are separate from application dependencies.

## 1. HTTP and OpenAI-compatible SDKs

### Shortlist

| Choice | Audio transcription | LLM cleanup | Trade-off |
| --- | --- | --- | --- |
| `async-openai` | Typed multipart requests, including in-memory audio | Typed chat completions | Strongest focused SDK candidate; configure transport/policies explicitly. |
| `reqwest` directly | Build multipart audio request | Serialize a small chat request | Less SDK abstraction, complete control over endpoint/auth/error behavior. |
| `rig-core` | OpenAI transcription abstraction | LLM framework | Supports both, but agent/tool/framework machinery is unnecessary for two operations. |
| `genai` | No transcription API found in the inspected stable docs | Multi-provider chat abstraction | Would still need a separate audio client. |

`async-openai` **0.42.1** and `reqwest` **0.13.x** were inspected. Exact versions
and features must be rechecked when creating Cargo.toml; old async-openai
examples may use different audio APIs and retry configuration.

### What async-openai provides

- Configurable API base URL and API key; create distinct clients/configurations
  for transcription and cleanup.
- Audio upload directly from bytes with a filename such as `recording.wav`; no
  temp file or external encoder is needed.
- Chat completion types and structured API errors. Cleanup must handle missing
  or empty message content, not assume every response contains text.
- A custom `reqwest::Client` can supply connect/read/request timeouts and TLS
  policy. Wrap each complete stage in an absolute deadline/cancellation branch.
- Relevant API features are `audio` and `chat-completion`; transport can use
  `rustls`. Do not enable the entire API surface without a reason.

Caveats in the inspected version:

1. The default `OpenAIConfig` constructs a Bearer authorization header even when
   its key is empty. **This is not a limitation of the whole SDK:** its public
   `Config` trait supplies the headers and URL, so an application-owned config
   can legitimately omit auth for a local server. Do not use a dummy key as a
   workaround or inherit unrelated OpenAI environment settings implicitly.
2. The standard HTTP executor installs an automatic retry layer (including
   429/5xx cases). Injecting a custom reqwest client does not by itself remove
   that SDK layer. Supported custom HTTP-service/middleware integration can
   change it. Dictation needs a deliberately bounded policy: retries can add
   latency and duplicate billable processing.
3. Default base-URL construction concatenates paths. Normalize the base URL
   including `/v1`, without a trailing slash; test joining explicitly.
4. Typed APIs assume compatible request/response shapes. Plain-text STT output,
   extra vendor parameters, or unusual errors may need raw/custom-body APIs.

### Recommendation

**Initial preference: a small `reqwest` transport implementing the two operations.**
There is no need for an agent framework, and directly expressing multipart STT
and chat-completions gives optional auth and explicit timeout/retry behavior
without configuring an additional middleware stack. This is native library use,
not a subprocess or compatibility workaround.

**If typed SDK ergonomics are preferred, choose `async-openai`, not a generic
LLM-only client.** It covers both operations. Use its documented custom config
and HTTP transport hooks rather than assuming defaults fit all local servers.
It is a viable alternative, not rejected as incompatible with keyless servers.

Before committing the dependency choice, implement a tiny transport spike against
mock servers: multipart in-memory WAV, a chat reply, a keyless localhost endpoint,
Bearer auth, timeout, cancellation, 429, and malformed error responses. Pick the
SDK only if those policies stay simple and observable.

For direct reqwest 0.13, the researched feature set is `default-features = false`
with `rustls`, `json`, and `multipart`. Proxy/HTTP2 support is an explicit product
choice. The rustls feature does not require OpenSSL, but its crypto provider may
still need a C toolchain; the current shell has one. Verify certificate trust on
NixOS rather than disabling certificate validation.

### Sources

- [async-openai 0.42.1 client and transport hooks](https://docs.rs/async-openai/0.42.1/async_openai/struct.Client.html)
- [Public Config trait — headers are configurable](https://docs.rs/async-openai/0.42.1/async_openai/config/trait.Config.html)
- [async-openai configuration source](https://docs.rs/crate/async-openai/0.42.1/source/src/config.rs)
- [HTTP executor source](https://docs.rs/crate/async-openai/0.42.1/source/src/executor.rs)
- [SDK retry policy source](https://docs.rs/crate/async-openai/0.42.1/source/src/middleware/retry/openai.rs)
- [Audio transcription implementation](https://docs.rs/crate/async-openai/0.42.1/source/src/audio/transcriptions.rs)
- [reqwest features](https://docs.rs/crate/reqwest/latest/features)
- [reqwest client configuration](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html)
- [Rig transcription API](https://docs.rs/rig-core/latest/rig_core/transcription/index.html)
- [genai stable API](https://docs.rs/genai/latest/genai/)

## 2. Local Whisper and memory constraints

### Library choice

**Recommend `whisper-rs` (inspected release: 0.16.0), CPU by default.** It binds
whisper.cpp natively, accepts quantized ggml models, and exposes blocking
inference plus an abort callback. Run inference on a dedicated worker and bridge
an atomic cancellation flag into `set_abort_callback_safe`; also reject stale
session results. Dropping a Rust future does not stop a native inference call.

The maintained repository moved to Codeberg; the old GitHub repository is
archived. Its sys crate vendors/builds a whisper.cpp snapshot. Installing the
nixpkgs whisper CLI does **not** select the library version used by whisper-rs.
Record the actual vendored backend version when benchmarking.

Input is **mono 16 kHz f32 PCM**. CPAL device rates/channel layouts must be
converted and resampled before inference; WAV encoding is only needed for the
API backend, not the native Whisper call.

Alternatives:

- **Candle:** Rust-native model execution with CPU/CUDA/Metal support. The
  inspected Whisper implementation has no Vulkan backend, and the ordinary
  safetensors path uses f32. It requires more model/decoder integration than
  whisper-rs; quantized paths must be evaluated separately.
- **ONNX stacks:** useful if adding Parakeet, Moonshine, or other speech models.
  `transcribe-rs`, `sherpa-onnx`, and related crates merit a later comparison;
  not all of these are Whisper wrappers, and GPU execution-provider packaging
  introduces its own native dependencies.

### GPU/backend plan

| Platform | Initial path | Optional acceleration |
| --- | --- | --- |
| x86_64 Linux | CPU | Vulkan across supported vendors; CUDA on NVIDIA; HIP/ROCm where supported. |
| aarch64 Linux | CPU | Vulkan only after checking the particular GPU/driver/backend combination. |
| Apple Silicon running NixOS | CPU | Asahi Vulkan is an experiment to measure, not equivalent to macOS Metal. |

**No GPU and no VRAM are required for CPU inference.** Metal/CoreML are macOS
paths, not acceleration options on NixOS Apple Silicon. Prefer Vulkan as the
first optional Linux GPU build; retain CUDA as a separate opt-in investigation.

The current dev shell supplies the CPU build tools (CMake, Ninja, Clang,
libclang/bindgen). A Vulkan-enabled build would additionally need Vulkan
headers/loader, shaderc and the required SPIR-V tools/headers, plus a functioning
host ICD/driver. CUDA needs a compatible toolkit/link environment and NixOS
driver handling. Do not add GPU dependencies or change host drivers until a
backend and target hardware have been selected.

### RAM / disk / VRAM are different quantities

whisper.cpp publishes this rough model table:

| Model family | Unquantized model on disk | Upstream approximate memory figure |
| --- | ---: | ---: |
| tiny | 75 MiB | ~273 MB |
| base | 142 MiB | ~388 MB |
| small | 466 MiB | ~852 MB |
| medium | 1.5 GiB | ~2.1 GB |
| large | 2.9 GiB | ~3.9 GB |

The README does not specify a sufficiently precise measurement method/backend
for that memory column. **It is not a guaranteed VRAM table**, and does not
separately characterize large-v3-turbo or every quantization/backend combination.

Selected model-file sizes from the upstream Hugging Face repository:

| Model file | Approximate disk size |
| --- | ---: |
| `ggml-base-q5_1.bin` | 57 MiB |
| `ggml-small-q5_1.bin` | 181 MiB |
| `ggml-medium-q5_0.bin` | 514 MiB |
| `ggml-large-v3-turbo-q5_0.bin` | 547 MiB |
| `ggml-large-v3-q5_0.bin` | 1,031 MiB |
| `ggml-large-v3.bin` | 2,952 MiB |

These are **weight files**, not total resident memory. Quantization reduces
weights, not all activations/KV caches/scratch buffers. A GPU budget includes
GPU-resident weights + KV/cache buffers + compute buffers + driver/runtime
allocations, while some allocations can remain in system RAM.

A reported CUDA failure for quantized large-v3 on a nominal **4 GB GPU with only
~2 GB free** demonstrates why total card capacity and model file size alone
are inadequate. That single report is not a universal minimum requirement.
No trustworthy consolidated whisper.cpp VRAM-per-model table was found.
OpenAI's PyTorch Whisper VRAM figures must not be reused as whisper.cpp numbers.

**Practical plan:** begin with multilingual base/small quantized models; evaluate
large-v3-turbo as a quality option after measuring. Use `.en` variants only if
English-only is an explicit requirement. Do not promise a particular model fits
2/4/8 GB before measuring the selected backend and build.

Benchmark model load and a representative 30-second inference, recording:

- Peak process RSS and peak GPU memory, including first-inference allocations.
- Free GPU memory before the test (desktop/browser/local LLM can compete for it).
- Model checksum/quantization, backend version, device, threads, and attention
  settings. Flash-attention defaults can differ between wrapper/backend versions.
- End-to-end latency and transcription quality on the user's languages/audio.
- Behavior on memory exhaustion; report it rather than silently changing backend.

For a local cleanup LLM sharing the GPU, account for both models' resident
allocations or deliberately unload/serialize them. On integrated/unified-memory
GPUs, total shared RAM and driver limits matter more than a discrete VRAM label.

### Implementation findings (whisper-rs 0.16.0, whisper.cpp 1.8.3)

Measured 2026-10-09 while implementing Phase 7.

- **Nix strips `-march=native`.** ggml defaults to `GGML_NATIVE=ON`, but the Nix
  cc-wrapper (`NIX_ENFORCE_NO_NATIVE`) removes the flag, yielding a build with
  no SIMD at all (`print_system_info` lists only `REPACK`): base-q5_1 ran at
  RTF 1.49. Setting `GGML_NATIVE=OFF` + `GGML_AVX/AVX2/FMA/F16C/BMI2=ON`
  (x86-64-v3; whisper-rs-sys forwards `GGML_*` env vars to CMake) gives RTF 0.13.
  A future Nix package must set these flags explicitly too. aarch64 is untuned.
- **`set_abort_callback_safe` is unsound in whisper-rs 0.16.0.** It registers
  `trampoline::<F>` but passes a `Box<dyn FnMut>` as user data, so a plain
  closure reads foreign memory and aborts inference at random (observed:
  "failed to encode", code -6, with nothing cancelled). Workaround in
  `transcription/whisper.rs`: pass `F = Box<dyn FnMut() -> bool>`, which makes
  the cast correct. Worth reporting upstream.
- **Abort granularity.** whisper.cpp polls the abort callback only after each
  encoder/decoder graph run, not inside one. We also install an
  `encoder_begin_callback`. Cancel latency is therefore ≤ one encoder pass
  (≈ 2 s for base on 2 CPU threads; far less on GPU).
- **`whisper_rs::vulkan::list_devices()` aborts the process without a Vulkan
  driver** (C++ exception crossing into Rust). The generic ggml backend
  registry (`ggml_backend_dev_*`, via whisper-rs `raw-api`) catches Vulkan init
  failures; we enumerate GPUs there, mirroring whisper.cpp's `gpu_device`
  indexing, and use `ggml_backend_dev_memory` for VRAM numbers.
- **whisper.cpp silently uses the CPU when no GPU is found**; we check first.
- **Language detection costs one extra encoder pass**, both whisper.cpp's own
  `auto` and our restricted `languages = ["de", "en"]` (same cost, plus a mel
  computation). A fixed language halves encoder time (base: 0.57 s vs 1.1 s on a
  7 s clip) but a *wrong* fixed language makes Whisper translate.
- Whisper always encodes a full 30 s window, so short clips cost about as much
  as 30 s ones; latency is dominated by the encoder on CPU.

CPU benchmark — Ryzen 9 7900X, 8 threads, x86-64-v3 build, 7 s German clip,
restricted de/en detection (2 encoder passes), greedy:

| Model | Load | Latency | RTF | Peak RSS | Beam 5 latency |
| --- | ---: | ---: | ---: | ---: | ---: |
| base-q5_1 | 50 ms | 0.94 s | 0.13 | 174 MiB | 1.0 s |
| small-q5_1 | 120 ms | 3.2 s | 0.45 | 368 MiB | 3.3 s |
| large-v3-turbo-q5_0 | 250 ms | 16.2 s | 2.30 | 746 MiB | 16.7 s |

All three transcribed the clip correctly. Conclusion: large-v3-turbo needs a
GPU; CPU-only hosts should use small (or base).

Vulkan benchmark — RX 9070 XT (16 GiB, RADV GFX1201), same build flags,
large-v3-turbo-q5_0, flash attention, beam 5, restricted de/en detection,
6.9 s clip:

| Load | Latency (runs 1–3) | RTF | VRAM after load | VRAM peak | Process RSS |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 212 ms | 313 / 291 / 261 ms | ≈ 0.04 | +820 MiB | +890 MiB | 145 MiB |

About 50x faster than the same model on the CPU, using < 1 GiB VRAM (device-wide
free-memory delta). That leaves ~13 GiB for a local cleanup LLM: a 7B model at
Q4 (~4.5–5 GiB weights plus KV cache) fits alongside Whisper, so unloading
Whisper before cleanup is unnecessary on this host; `keep_loaded = "idle"`
stays the sensible default. Notes: the Ryzen iGPU is enumerated too (gpu 1, shared
memory), so `gpu_device = 0` matters; RADV prints a harmless "not a
conformant Vulkan implementation" warning for GFX12.

### Sources

- [whisper-rs maintained repository](https://codeberg.org/tazz4843/whisper-rs)
- [whisper-rs 0.16.0 API](https://docs.rs/whisper-rs/0.16.0/whisper_rs/)
- [Feature flags](https://docs.rs/crate/whisper-rs/0.16.0/features)
- [Inference parameters / abort callback](https://docs.rs/whisper-rs/0.16.0/whisper_rs/struct.FullParams.html)
- [whisper.cpp memory table, quantization, backends](https://github.com/ggml-org/whisper.cpp)
- [Upstream model repository and files](https://huggingface.co/ggerganov/whisper.cpp/tree/main)
- [Quantized large-v3 CUDA allocation failure](https://github.com/ggml-org/whisper.cpp/discussions/2317)
- [Candle](https://github.com/huggingface/candle)
- [Candle Whisper implementation](https://github.com/huggingface/candle/tree/main/candle-transformers/src/models/whisper)
- [transcribe-rs](https://docs.rs/transcribe-rs/latest/transcribe_rs/)

## 3. Native clipboard and typing on river

### Clipboard: arboard versus direct data-control

**`arboard` is native, but the default Linux path is X11/XWayland.** For native
Wayland, enable `wayland-data-control`. A text-only configuration can disable
its default image feature:

```toml
arboard = { version = "3.6", default-features = false, features = ["wayland-data-control"] }
```

This enables its `wl-clipboard-rs` backend. Arboard can fall back to X11 when
Wayland initialization fails; if using it, do not advertise guaranteed native
Wayland behavior based solely on successful construction. Hold the clipboard
object for the lifetime of the resident process to serve paste requests.

**Preferred for this Wayland-only app: `wl-clipboard-rs` directly.** Despite its
name it is a native protocol library, **not execution of `wl-copy`**. It supports
`ext-data-control-v1` and `wlr-data-control-unstable-v1`, avoiding arboard's
cross-platform/X11 fallback. Arboard remains a valid convenience choice if that
fallback is acceptable; no need to write clipboard protocols ourselves.

Data-control matters because the overlay must never take focus. Ordinary GTK/GDK
clipboard access uses the core Wayland data-device path, whose selection reads
are focus-scoped and writes require a valid input serial. A never-focused,
never-clicked indicator cannot rely on it for background clipboard control.
Data-control supplies selection access without that input-serial requirement;
compositors may restrict it to trusted clients.

Clipboard ownership is a protocol lifecycle, not a one-shot copy call. Keep the
library's serving mechanism alive; do not destroy the owner immediately after
emitting a paste chord. Replacing clipboard content is a visible side effect;
naive timed restoration is not part of the initial design.

### River virtual-keyboard support

Upstream source inspection found `zwp_virtual_keyboard_manager_v1` creation in
river's input manager in both older 0.3.x and newer 0.4.x lines. River-classic
also supports it. It is **not a WM-only protocol**: ordinary trusted clients can
create a virtual keyboard. Newer river's separate `river-virtual-keyboard-v1`
synchronization extension is WM-facing and is not needed here.

Newer upstream river also creates both ext and wlr data-control managers;
older river exposes wlr data-control. Its security-context policy can hide
clipboard/input globals from sandboxed clients. A native unconfined app is the
initial target; sandbox packaging would require separate design work.

**Host caveat:** riverfx/fenster sources and an active Wayland connection are
absent in this research environment. Upstream support is strong evidence, not
proof of the deployed fork's revision or policy. Probe the actual session before
committing to an input backend; no root permissions should be necessary when
these globals are exposed to the app.

### Native Rust implementation

Use `wayland-client` and
`wayland-protocols-misc = { version = "0.3", features = ["client"] }`.
The verified binding module is
`wayland_protocols_misc::zwp_virtual_keyboard_v1::client`.
It is **not** the current `wayland-protocols-wlr` virtual-keyboard module.

Own a separate Wayland connection/event queue on a worker; leave GTK/GDK's
connection under GTK's event loop. Bind the manager and selected `wl_seat`, then
create `zwp_virtual_keyboard_v1`. Handle multiple seats explicitly.

The protocol sends **keys, not text**:

1. Compile/serialize an XKB text keymap with `xkbcommon`/libxkbcommon.
2. Send the NUL-terminated keymap through an FD before any key/modifier events.
   An anonymous memory-backed FD is preferable to a named temporary file.
3. Resolve keys and modifier indices from that keymap. Wire keycodes are
   XKB keycodes minus 8. Do not hardcode modifier bitmasks from someone else's
   keymap or confuse depressed/latched/locked masks.
4. Emit balanced press/release events and modifier changes. Clear synthetic
   state on cancellation/errors; never leave Ctrl/Shift logically pressed.
5. Keep event delivery/connection lifetime valid through completion. There is no
   universal target-app acknowledgement that text was inserted.

### Direct typing or clipboard + paste?

**Recommend native clipboard + native virtual-keyboard paste as the first
insertion implementation.** This is not a shell workaround: both parts use
Wayland APIs through libraries. Unicode, emoji, multiline text, and long results
are handled by clipboard transfer instead of one keyboard event per character.
The paste chord is configurable for editors versus terminals.

Direct typing is possible without changing the clipboard: generate a per-device
keymap mapping Unicode keysyms, then synthesize presses/releases. But it is not a
universal text-insertion API. Applications sometimes interpret physical keycodes
as editing commands despite printable keysyms; supplementary Unicode and keymap
changes can also behave differently across toolkits. Newline/Tab can trigger
submission or navigation instead of inserting literal characters.

Treat direct typing as a bounded native proof of concept, not a promised fallback.
Test layout preservation and physical modifier interactions on the actual fork.
Recent river isolates virtual-keyboard keymaps/modifier state, but that behavior
has not been established for the user's riverfx revision.

Neither method guarantees returning to the original field if focus changes.
Explicit insertion targets the field focused at insertion time. Automatic
insertion, if later enabled, needs an explicit statement of that risk.

### Required native proof of concept (before the model pipeline)

- Query required globals natively at startup, and document the actual riverfx
  version/capabilities. `wayland-info` is only a development diagnostic.
- Set/copy text with data-control while an editor is focused and the overlay has
  `KeyboardMode::None`; verify clipboard ownership survives until paste.
- Upload an XKB keymap and send a balanced native paste chord in GTK, Chromium,
  and a terminal, then verify physical typing/modifiers still work.
- Separately try direct Unicode typing: Latin accents, CJK, emoji, Tab, newline,
  long input, and cancellation mid-insertion. Never run this against a shell
  containing potentially executable commands.
- Validate absent-manager/security-denied cases: clear unsupported error, no
  X11/uinput/subprocess fallback. Do not intentionally provoke protocol errors
  on GTK's connection; disposable test connections isolate negative tests.

### Sources

- [arboard README and Wayland feature](https://github.com/1Password/arboard)
- [arboard Linux backend selection](https://docs.rs/arboard/latest/src/arboard/platform/linux/mod.rs.html)
- [wl-clipboard-rs native protocols](https://github.com/YaLTeR/wl-clipboard-rs)
- [ext-data-control specification](https://wayland.app/protocols/ext-data-control-v1)
- [Virtual keyboard specification](https://wayland.app/protocols/virtual-keyboard-unstable-v1)
- [Rust virtual-keyboard binding module](https://docs.rs/wayland-protocols-misc/latest/wayland_protocols_misc/zwp_virtual_keyboard_v1/index.html)
- [River 0.4.8 input manager](https://github.com/riverwm/river/blob/v0.4.8/river/InputManager.zig)
- [River server/global policy](https://github.com/riverwm/river/blob/master/river/Server.zig)
- [River keyboard/keymap handling](https://github.com/riverwm/river/blob/master/river/Keyboard.zig)
- [river-classic](https://github.com/riverwm/river-classic)
- [wtype implementation — protocol reference, not an app dependency](https://github.com/atx/wtype/blob/master/main.c)
- [wtype keycode-collision report/patch](https://github.com/atx/wtype/pull/77)
