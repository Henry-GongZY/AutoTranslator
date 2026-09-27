# vendor/whisper-rs-sys

Vendored copy of whisper-rs-sys 0.13.1 (crates.io, includes whisper.cpp) with
one local change in build.rs: env keys `WHISPER_CMAKE_<VAR>` are forwarded as
CMake defines `-D<VAR>=VALUE`. Wired via `[patch.crates-io]` in the workspace
Cargo.toml.

Why: on GitHub's virtualized arm64 macOS runners, ggml's `-mcpu=native`
feature macros disagree with its runtime feature tests (i8mm), and
`ggml-cpu-quants.c` fails with "always_inline function 'vmmlaq_s32' requires
target feature 'i8mm'". CI sets `WHISPER_CMAKE_GGML_NATIVE=OFF` to build a
fixed armv8.2 baseline instead of native dispatch. Local Apple Silicon builds
are unaffected (env unset).

Upgrade path: bump whisper-rs in Cargo.toml/Cargo.lock, re-vendor, re-apply
the build.rs mapping (or drop it once upstream ggml fixes the mismatch).
