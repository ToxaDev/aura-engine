// The console window is kept visible on purpose (no windows_subsystem = "windows"):
// the engine prints its full DSP trace to stdout — filter resolution, hybrid-phase
// coverage, true-peak decisions, bit-perfect verification — and that console is the
// only log sink. Hiding it would silence the audit trail this project is built on.

mod audio;

#[tauri::command]
fn convert_files(
    paths: Vec<String>,
    fs_multiplier: u32,
    taps: u32,
    precision: u32,
    win_type: i32,
    custom_filter_path: Option<String>,
    use_gpu: bool,
    use_fir_resampling: bool,
    apodizing: u32,
    headroom_db: f64,
    adaptive_apodizer: bool,
    hybrid_phase: bool,
    iir_dc_blocking: bool,
) -> Result<(), String> {
    let settings = crate::audio::converter::ConvertSettings {
        out_rate: 0,      // Computed per-file in prepare.rs from src_rate x family_base x fs_multiplier
        fs_multiplier,    // FS slider value: 2, 4, 8, or 16
        taps: taps as usize,
        precision,
        win_type,
        custom_filter_path,
        use_gpu,
        use_fir_resampling,
        apodizing,
        headroom_db,
        adaptive_apodizer,
        hybrid_phase,
        iir_dc_blocking,
    };
    crate::audio::converter::convert_files(paths, settings)
}

#[tauri::command]
fn get_conversion_progress() -> (u32, u32, u32, String, String, u32) {
    crate::audio::converter::get_progress()
}

#[tauri::command]
fn cancel_conversion() -> Result<(), String> {
    crate::audio::converter::cancel();
    Ok(())
}

#[tauri::command]
fn cancel_file(idx: u32) -> Result<(), String> {
    crate::audio::converter::cancel_file(idx);
    Ok(())
}

#[tauri::command]
fn get_queue_status() -> String {
    crate::audio::converter::get_file_statuses()
}

/// Validate a .npy filter file and return its tap count.
/// Called by the frontend when the user picks a custom FIR filter via the file dialog.
/// The actual path is stored in JS state and passed to convert_files on conversion start.
#[tauri::command]
fn set_custom_filter(path: String) -> Result<u32, String> {
    use std::path::Path;
    let p = Path::new(&path);
    if !p.exists() {
        return Err(format!("Filter file not found: {}", path));
    }
    let coeffs = crate::audio::dsp_core::load_npy_f64(&path)
        .map_err(|e| format!("Failed to load filter '{}': {}", path, e))?;
    if coeffs.is_empty() {
        return Err(format!("Filter file is empty or has no coefficients: {}", path));
    }
    println!("[FILTER] Custom filter validated: {} ({} taps)", path, coeffs.len());
    Ok(coeffs.len() as u32)
}

/// Acknowledge filter clear — the filter path lives in JS state, so this is a no-op on
/// the Rust side. Kept as a command so the frontend can await it without try/catch errors.
#[tauri::command]
fn clear_custom_filter() -> Result<(), String> {
    println!("[FILTER] Custom filter cleared");
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            convert_files,
            get_conversion_progress,
            cancel_conversion,
            cancel_file,
            get_queue_status,
            set_custom_filter,
            clear_custom_filter
        ])
        .run(tauri::generate_context!())
        .expect("Error while running Aura Engine");
}
