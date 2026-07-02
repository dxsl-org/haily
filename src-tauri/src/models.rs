//! Local GGUF model discovery for the Settings → Model LLM picker.

/// Scan <exe_dir>/models/ for GGUF files and return metadata for the UI.
/// Multi-part files (part 2, 3, …) are hidden — only the -00001- entry point
/// is shown; llama.cpp loads all parts automatically.
pub fn list_local_models() -> Vec<serde_json::Value> {
    let models_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("models")))
        .unwrap_or_else(|| std::path::PathBuf::from("models"));

    let Ok(entries) = std::fs::read_dir(&models_dir) else { return vec![] };

    let mut models: Vec<serde_json::Value> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "gguf" { return None; }
            let name = path.file_name()?.to_str()?.to_string();
            // Hide continuation parts (e.g. -00002-of-00003)
            if name.contains("-of-") && !name.contains("-00001-of-") { return None; }
            let lower = name.to_lowercase();
            let format = if lower.contains("gemma") { "gemma4" } else { "chatml" };
            Some(serde_json::json!({
                "name": name,
                "path": path.to_string_lossy(),
                "format": format,
            }))
        })
        .collect();

    models.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    models
}
