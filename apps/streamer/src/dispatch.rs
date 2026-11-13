//! The commands the web remote may call, by name. This list is the server's
//! boundary: what is not named here cannot be asked. Paths from a request count
//! only inside the music folders (`library.rs`).

use aura_core::commands as engine;
use aura_core::filters_dl;
use aura_core::player::controller::RepeatMode;
use aura_core::player::settings::PlayerSettings;
use serde_json::{json, Value};

use crate::library::{self, Library};

/// Takes `$f`'s arguments out of the request's JSON (as Tauri does: camelCase
/// names, a missing `Option` is None).
macro_rules! call {
    ($args:ident, $f:expr, $($n:ident : $t:ty),+ $(,)?) => {{
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct A { $($n: $t),+ }
        let a: A = serde_json::from_value($args).map_err(|e| format!("bad arguments: {e}"))?;
        $f($(a.$n),+)
    }};
}

/// An engine future, waited for here: `dispatch` runs on the blocking pool.
fn wait<T>(f: impl std::future::Future<Output = T>) -> Result<T, String> {
    let rt = tokio::runtime::Handle::try_current().map_err(|_| "no runtime".to_string())?;
    Ok(rt.block_on(f))
}

fn to(v: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// Answer `cmd` with `args` (a JSON object). May wait: not on a thread that
/// must not.
pub fn dispatch(lib: &Library, cmd: &str, args: Value) -> Result<Value, String> {
    let inside = |paths: Vec<String>| -> (Vec<String>, Vec<String>) {
        paths.into_iter().partition(|p| lib.allowed(p))
    };
    match cmd {
        // The player.
        "player_add" => call!(args, |paths: Vec<String>, list: Option<u64>| {
            let (ok, out) = inside(paths);
            let mut r = engine::player_add(ok, list);
            if !out.is_empty() {
                let errors = r["errors"].as_array_mut().ok_or("no errors list")?;
                errors.extend(out.iter().map(|p| json!(format!("Not in the library: {p}"))));
            }
            Ok(r)
        }, paths: Vec<String>, list: Option<u64>),
        // A page reloading takes off the queue what is not in its list: the
        // network renderer's tracks stay.
        "player_remove" => to(call!(args, |id: u64| if !crate::upnp::owns(id) { engine::player_remove(id) }, id: u64)),
        "player_reorder" => to(call!(args, engine::player_reorder, ids: Vec<u64>)),
        "player_clear" => to(engine::player_clear()),
        "player_play" => to(call!(args, engine::player_play, id: Option<u64>)),
        "player_pause" => to(engine::player_pause()),
        "player_stop" => to(engine::player_stop()),
        "player_next" => to(engine::player_next()),
        "player_prev" => to(engine::player_prev()),
        "player_seek" => to(call!(args, engine::player_seek, seconds: f64)),
        "player_set_volume" => to(call!(args, engine::player_set_volume, db: f64)),
        "player_set_cue" => to(call!(args, engine::player_set_cue, params: aura_core::player::cue::CueParams)),
        "player_cue_view" => to(engine::player_cue_view()),
        "player_set_eq" => to(call!(args, engine::player_set_eq, eq: aura_core::audio::converter::dsp::eq::EqState)),
        "player_set_settings" => to(call!(args,
            |settings: PlayerSettings, rendered: Option<Vec<engine::RenderedItem>>, instant_start, lower_taps, prepare_ahead| {
                // A converted file plays in its track's place: it counts as any
                // other path, and outside the folders the track plays live.
                let rendered = rendered.map(|r| r.into_iter().map(|mut e| {
                    e.path = e.path.filter(|p| lib.allowed(p));
                    e
                }).collect());
                engine::player_set_settings(settings, rendered, instant_start, lower_taps, prepare_ahead)
            },
            settings: PlayerSettings, rendered: Option<Vec<engine::RenderedItem>>,
            instant_start: Option<bool>, lower_taps: Option<bool>, prepare_ahead: Option<bool>)?),
        "radio_stream_wait" => to(call!(args, engine::radio_stream_wait, settings: PlayerSettings, src_rate: Option<u32>)?),
        "player_status" => Ok(engine::player_status()),
        "player_dsd_capabilities" => Ok(call!(args, engine::player_dsd_capabilities, remeasure: Option<bool>)),
        "player_radio" => to(call!(args, engine::player_radio, url: String)?),
        // The input (Aura Player ASIO) is not offered here yet.
        "player_input_supported" => Ok(json!(false)),
        "player_paths_exist" => {
            call!(args, |paths: Vec<String>| to(paths.iter().map(|p| lib.allowed(p)).collect::<Vec<_>>()), paths: Vec<String>)
        }
        "player_render_checks" => call!(args, |paths: Vec<String>| {
            to(paths.into_iter().map(|p| {
                if lib.allowed(&p) {
                    engine::player_render_checks(vec![p]).remove(0)
                } else {
                    json!({ "exists": false, "valid": false, "tip": null })
                }
            }).collect::<Vec<_>>())
        }, paths: Vec<String>),
        "player_devices" => engine::player_devices(),
        "player_set_device" => to(call!(args, engine::player_set_device, id: Option<String>)),
        "player_ui_log" => to(call!(args, engine::player_ui_log, level: String, message: String)),
        "player_queue" => engine::player_queue(),
        "player_set_repeat" => to(call!(args, engine::player_set_repeat, mode: RepeatMode)),
        "player_set_vis_ahead" => to(call!(args, engine::player_set_vis_ahead, source: String, seconds: f64)),
        "player_set_track_settings" => {
            to(call!(args, engine::player_set_track_settings, id: u64, settings: PlayerSettings))
        }
        "player_clear_track_settings" => to(call!(args, engine::player_clear_track_settings, id: u64)),
        // The analyzer (a frame over the remote): its measuring session. A file
        // to measure counts as any other path: outside the folders, none.
        "an_subject_open" => to(call!(args,
            |mode: String, entry_id: Option<u64>, path: Option<String>, conv: Option<String>, settings: Option<Value>| {
                let path = path.filter(|p| lib.allowed(p));
                let conv = conv.filter(|p| lib.allowed(p));
                aura_core::player::analytics::hub::get().subject_open(&mode, entry_id, path.as_deref(), conv.as_deref(), settings)
            },
            mode: String, entry_id: Option<u64>, path: Option<String>, conv: Option<String>, settings: Option<Value>)),
        "an_subject_close" => to(call!(args, |sid: u32| aura_core::player::analytics::hub::get().subject_close(sid), sid: u32)),

        // What the rack needs to know about this machine.
        "check_filters" => to(call!(args,
            |paths: Vec<String>, fs_multiplier, taps, hybrid_phase, tfs_phase| {
                let (ok, _) = inside(paths);
                engine::check_filters(ok, fs_multiplier, taps, None, hybrid_phase, tfs_phase)
            },
            paths: Vec<String>, fs_multiplier: u32, taps: u32, hybrid_phase: bool, tfs_phase: bool)),
        "gpu_status" => to(engine::gpu_status()),
        "gpu_info" => to(engine::gpu_info()),
        "filter_inventory" => to(engine::filter_inventory()),
        "tfs_unavailable" => to(call!(args, aura_core::audio::converter::dsp::lab::tfs::unavailable_here, taps: usize)),
        "dsd_export_modulators" => to(call!(args, aura_core::audio::converter::dsd_out::modulators, multiple: u32)),
        "get_conversion_progress" => to(aura_core::audio::converter::get_progress()),
        "get_queue_status" => to(call!(args, aura_core::audio::converter::get_file_statuses, known_rev: u32)),
        "perf_history" => to(aura_core::perf::perf_history()),
        "spatial_status" => Ok(aura_core::spatial::pack::spatial_status()),
        "spatial_track_status" => Ok(aura_core::spatial::spatial_track_status()),

        // The filter download, the app's own (aura_core::filters_dl): the
        // files a setting misses, from the signed list, into the folder the
        // lookup searches (beside this exe on Windows). Not offered: opening
        // that folder in a file manager on the computer.
        "filters_plan" => call!(args,
            |taps, fs, phases, families, hires| wait(filters_dl::plan(taps, fs, phases, families, hires))?,
            taps: usize, fs: u32, phases: Vec<String>, families: Vec<String>, hires: Option<bool>),
        "filters_download" => to(call!(args, |names| -> Result<(), String> {
            let rt = tokio::runtime::Handle::try_current().map_err(|_| "no runtime".to_string())?;
            rt.spawn(filters_dl::start_download(names)?);
            Ok(())
        }, names: Vec<String>)?),
        "filters_cancel" => to(filters_dl::cancel()),
        "filters_storage" => wait(filters_dl::storage())?,
        "filters_trash" => to(call!(args, |names| wait(filters_dl::trash(names)), names: Vec<String>)??),

        // Its own updates (update.rs): the check never fails, the install
        // goes on in the background.
        "update_check" => Ok(crate::update::check()),
        "update_install" => crate::update::install(),

        // The music folders.
        "lib_browse" => call!(args, |path: Option<String>| lib.browse(path.as_deref()), path: Option<String>),
        "lib_pick" => call!(args, |path: Option<String>| library::pick(path.as_deref()), path: Option<String>),
        "lib_roots" => to(lib.roots()),
        "lib_add_root" => to(call!(args, |path: String| lib.add_root(&path), path: String)?),
        "lib_remove_root" => to(call!(args, |path: String| lib.remove_root(&path), path: String)),
        // The library: albums, artists, genres, search (library.rs).
        c if c.starts_with("lib_") => lib.command(c, args),

        _ => Err(format!("{cmd} is not available on the streamer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> std::sync::Arc<Library> {
        Library::open(None, None)
    }

    #[test]
    fn a_command_not_on_the_list_is_refused() {
        for cmd in ["convert_files", "set_custom_filter", "app_quit", "filters_dir_open", "ui_pack_install", ""] {
            assert!(dispatch(&lib(), cmd, json!({})).unwrap_err().contains("not available"), "{cmd}");
        }
    }

    #[test]
    fn arguments_are_read_as_the_pages_send_them() {
        assert!(dispatch(&lib(), "player_seek", json!({})).unwrap_err().contains("bad arguments"));
        assert!(dispatch(&lib(), "player_seek", json!({ "seconds": "x" })).unwrap_err().contains("bad arguments"));
    }

    #[test]
    fn the_filter_download_is_on_the_list() {
        for cmd in ["filters_plan", "filters_download", "filters_trash"] {
            assert!(dispatch(&lib(), cmd, json!({})).unwrap_err().contains("bad arguments"), "{cmd}");
        }
        assert_eq!(dispatch(&lib(), "filters_cancel", json!({})), Ok(Value::Null));
        // The work waits on the engine's runtime; none here.
        assert!(dispatch(&lib(), "filters_storage", json!({})).unwrap_err().contains("no runtime"));
    }

    #[test]
    fn the_analyzer_is_on_the_list() {
        // Its arguments are read before the hub (started by main) is reached.
        for cmd in ["an_subject_open", "an_subject_close"] {
            assert!(dispatch(&lib(), cmd, json!({})).unwrap_err().contains("bad arguments"), "{cmd}");
        }
    }

    #[test]
    fn its_own_update_is_on_the_list() {
        let v = dispatch(&lib(), "update_check", json!({})).unwrap();
        assert_eq!(v["product"], json!("streamer"));
        let r = dispatch(&lib(), "update_install", json!({}));
        assert!(!r.is_err_and(|e| e.contains("not available")));
    }

    #[test]
    fn a_path_outside_the_folders_does_not_reach_the_engine() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let r = dispatch(&lib(), "player_add", json!({ "paths": [exe.clone()] })).unwrap();
        assert_eq!(r["tracks"].as_array().unwrap().len(), 0);
        assert!(r["errors"][0].as_str().unwrap().contains("Not in the library"));
        let r = dispatch(&lib(), "player_paths_exist", json!({ "paths": [exe.clone()] })).unwrap();
        assert_eq!(r, json!([false]), "does not even say the file is there");
        let r = dispatch(&lib(), "player_render_checks", json!({ "paths": [exe] })).unwrap();
        assert_eq!(r[0]["exists"], json!(false));
    }
}
