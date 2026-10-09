# CMake defines for whisper.cpp's CPU backend, passed as environment
# variables (whisper-rs-sys forwards every GGML_* variable to CMake).
#
# Nix's cc-wrapper strips -march=native, so ggml's default native build
# silently ends up without SIMD (~10x slower on CPU). On x86_64 request
# x86-64-v3 (AVX2/FMA/F16C/BMI2, any CPU since ~2015) explicitly. aarch64
# baseline NEON needs nothing extra.
hostPlatform:
if hostPlatform.isx86_64 then
  {
    GGML_NATIVE = "OFF";
    GGML_AVX = "ON";
    GGML_AVX2 = "ON";
    GGML_FMA = "ON";
    GGML_F16C = "ON";
    GGML_BMI2 = "ON";
  }
else
  { }
