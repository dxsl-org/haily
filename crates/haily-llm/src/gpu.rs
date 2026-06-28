/// GPU layer auto-detection for embedded llama.cpp inference.
///
/// Returns the default number of GPU layers based on which GPU backends were compiled in.
/// llama.cpp clamps this to the actual layer count of the loaded model, so 999 means
/// "offload everything that fits in VRAM."
///
/// Override at runtime via the `llm.llama_n_gpu_layers` DB preference.
pub fn default_gpu_layers() -> u32 {
    // Any GPU backend compiled in → full offload by default.
    #[cfg(any(feature = "cuda", feature = "metal", feature = "vulkan"))]
    {
        tracing::info!(
            "GPU backend detected at compile time (cuda={}, metal={}, vulkan={}) — defaulting to full GPU offload",
            cfg!(feature = "cuda"),
            cfg!(feature = "metal"),
            cfg!(feature = "vulkan"),
        );
        999
    }
    #[cfg(not(any(feature = "cuda", feature = "metal", feature = "vulkan")))]
    {
        0
    }
}

/// Human-readable description of the active GPU mode for startup logs.
pub fn gpu_mode_label(n_gpu_layers: u32) -> &'static str {
    if n_gpu_layers == 0 {
        "CPU-only"
    } else if n_gpu_layers >= 999 {
        "GPU full-offload"
    } else {
        "GPU partial-offload"
    }
}
