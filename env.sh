# source this before any cargo/rustc work in nx-wisp
export RUSTUP_HOME=/run/media/nerdrx/Lex/claude/tools/rust/rustup
export CARGO_HOME=/run/media/nerdrx/Lex/claude/tools/rust/cargo
export PATH="$CARGO_HOME/bin:/run/media/nerdrx/Lex/claude/tools/cmake-3.31.10-linux-x86_64/bin:$PATH"
# ort-sys (onnxruntime for wisp-voice's Piper TTS) cannot download its prebuilt
# binaries here: the machine has an RA-learned IPv6 default route with no
# working IPv6 path, and ort-sys's fetch has no happy-eyeballs. The archive was
# fetched once over IPv4, hash-verified against ort-sys's own pin (acc1cba7…),
# raw-LZMA2-decoded, and planted in this cache — ort-sys skips the network
# entirely when the cache entry exists.
export ORT_CACHE_DIR=/run/media/nerdrx/Lex/claude/tools/ort-cache
# llama.cpp's ggml-vulkan needs Vulkan headers + SPIRV-Headers cmake packages,
# which CachyOS does not ship; staged under tools/vulkan-sdk by the wisp-mind
# build (2026-08-23).
export VULKAN_SDK=/run/media/nerdrx/Lex/claude/tools/vulkan-sdk
export CMAKE_PREFIX_PATH="$VULKAN_SDK${CMAKE_PREFIX_PATH:+:$CMAKE_PREFIX_PATH}"
