//! Extra WebView2 browser arguments for A/B tests of how frames reach the
//! screen (`AURA_WV2_EXTRA`).
//!
//! Unset or empty, nothing changes. Set, they go to every webview of the
//! process through `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` (WebView2 takes
//! that variable over the arguments wry passes), after what the variable
//! already holds or, when it is empty, after wry's default arguments.
//! Features in a `--disable-features=` of the extra arguments join the list
//! already there, so both stay in force.

/// What wry passes when no arguments are given (wry 0.57, webview2/mod.rs).
const WRY_DEFAULT: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

const DISABLE: &str = "--disable-features=";

/// `base` (wry's defaults when empty) with the extra arguments; None when
/// there are none.
pub fn merged(base: Option<&str>, extra: Option<&str>) -> Option<String> {
    let extra = extra?.trim();
    if extra.is_empty() {
        return None;
    }
    let base = base.map(str::trim).filter(|b| !b.is_empty()).unwrap_or(WRY_DEFAULT);
    let mut features: Vec<String> = Vec::new();
    let mut rest = Vec::new();
    for arg in base.split_whitespace().chain(extra.split_whitespace()) {
        match arg.strip_prefix(DISABLE) {
            Some(list) => {
                for f in list.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                    if !features.iter().any(|g| g == f) {
                        features.push(f.to_string());
                    }
                }
            }
            None => rest.push(arg),
        }
    }
    let mut line = String::new();
    if !features.is_empty() {
        line = format!("{DISABLE}{}", features.join(","));
    }
    for arg in rest {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(arg);
    }
    Some(line)
}

/// Before the first webview: sets the variable from `AURA_WV2_EXTRA` and
/// writes the line to the log.
pub fn apply() {
    let base = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").ok();
    if let Some(line) = merged(base.as_deref(), std::env::var("AURA_WV2_EXTRA").ok().as_deref()) {
        crate::aelog!("[WV2] browser args: {line}");
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_empty_changes_nothing() {
        assert_eq!(merged(None, None), None);
        assert_eq!(merged(Some("--x"), Some("  ")), None);
    }

    #[test]
    fn plain_switch_follows_the_defaults() {
        assert_eq!(
            merged(None, Some("--disable-direct-composition")).unwrap(),
            format!("{WRY_DEFAULT} --disable-direct-composition")
        );
        assert_eq!(merged(Some(" "), Some("--a")).unwrap(), format!("{WRY_DEFAULT} --a"));
    }

    #[test]
    fn disabled_features_join_the_default_list() {
        assert_eq!(
            merged(None, Some("--disable-features=DelegatedCompositing,msWebOOUI --disable-gpu-vsync")).unwrap(),
            format!("{WRY_DEFAULT},DelegatedCompositing --disable-gpu-vsync")
        );
    }

    #[test]
    fn a_set_variable_is_kept_and_extended() {
        assert_eq!(
            merged(Some("--remote-debugging-port=9226"), Some("--disable-features=A,B")).unwrap(),
            "--disable-features=A,B --remote-debugging-port=9226"
        );
        assert_eq!(
            merged(Some("--disable-features=X --p=1"), Some("--disable-features=X,Y --q")).unwrap(),
            "--disable-features=X,Y --p=1 --q"
        );
    }
}
