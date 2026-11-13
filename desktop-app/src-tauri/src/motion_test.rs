//! A window of steady motion (`motion-test.html`) to judge by eye and by
//! numbers whether the frames the page draws all reach the screen.
//!
//! Opened at start only when `AURA_MOTION_TEST` is set; unset, nothing
//! happens. The value is a comma list: `gl` draws with WebGL (else a 2D
//! canvas), `opaque` makes the window non-transparent (else it is built like
//! the main window: frameless, transparent, no shadow), `hz=<rate>` gives the
//! display's exact refresh to the page. Any other value (`1`) = 2D,
//! transparent. The page logs a line `[UI] motion: …` every 10 s.

/// What the test window shows and how it is built.
#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub gl: bool,
    pub opaque: bool,
    pub hz: Option<f64>,
}

/// The mode from the variable's value; None when it is unset or empty.
pub fn mode_from(value: Option<&str>) -> Option<Mode> {
    let v = value?.trim();
    if v.is_empty() {
        return None;
    }
    let mut m = Mode { gl: false, opaque: false, hz: None };
    for t in v.split(',').map(str::trim) {
        match t {
            "gl" => m.gl = true,
            "opaque" => m.opaque = true,
            _ => {
                if let Some(hz) = t.strip_prefix("hz=").and_then(|x| x.parse::<f64>().ok()) {
                    if hz > 0.0 {
                        m.hz = Some(hz);
                    }
                }
            }
        }
    }
    Some(m)
}

/// The page's address for a mode.
pub fn url_for(m: &Mode) -> String {
    let mut q = vec![format!("gl={}", if m.gl { 1 } else { 0 })];
    if let Some(hz) = m.hz {
        q.push(format!("hz={hz}"));
    }
    format!("motion-test.html?{}", q.join("&"))
}

/// Opens the test window when `AURA_MOTION_TEST` asks for it.
pub fn start(app: &tauri::AppHandle) {
    let Some(m) = mode_from(std::env::var("AURA_MOTION_TEST").ok().as_deref()) else {
        return;
    };
    crate::aelog!("[motion] test window: {:?}", m);
    // A while after start, so the main window is up and is the first page.
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(8));
        let built = tauri::WebviewWindowBuilder::new(&app, "motion-test", tauri::WebviewUrl::App(url_for(&m).into()))
            .title("Motion test")
            .inner_size(900.0, 600.0)
            .center()
            .resizable(true)
            .decorations(false)
            .transparent(!m.opaque)
            .shadow(false)
            .use_https_scheme(true)
            .build();
        if let Err(e) = built {
            crate::aelog!("[motion] test window failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_empty_opens_nothing() {
        assert_eq!(mode_from(None), None);
        assert_eq!(mode_from(Some("  ")), None);
    }

    #[test]
    fn tokens_pick_the_drawing_the_window_and_the_rate() {
        assert_eq!(mode_from(Some("1")), Some(Mode { gl: false, opaque: false, hz: None }));
        assert_eq!(
            mode_from(Some("gl, opaque,hz=174.971")),
            Some(Mode { gl: true, opaque: true, hz: Some(174.971) })
        );
        assert_eq!(mode_from(Some("hz=x,hz=-3")).unwrap().hz, None);
    }

    #[test]
    fn url_carries_the_mode() {
        let m = Mode { gl: true, opaque: false, hz: Some(174.971) };
        assert_eq!(url_for(&m), "motion-test.html?gl=1&hz=174.971");
        assert_eq!(url_for(&Mode { gl: false, opaque: true, hz: None }), "motion-test.html?gl=0");
    }
}
