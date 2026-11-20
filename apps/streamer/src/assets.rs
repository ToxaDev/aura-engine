//! The web remote's files. In the exe; with `AURA_WEB_DIR` (the repository's
//! folder) read from the disk as they are, for working on the pages.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

fn table() -> &'static HashMap<&'static str, &'static [u8]> {
    static T: OnceLock<HashMap<&'static str, &'static [u8]>> = OnceLock::new();
    T.get_or_init(|| {
        let mut m = HashMap::new();
        for (path, bytes) in ASSETS {
            // The first of a path wins (build.rs).
            m.entry(*path).or_insert(*bytes);
        }
        m
    })
}

fn from_disk(repo: &std::path::Path, path: &str) -> Option<Vec<u8>> {
    let remote = repo.join("web").join("remote");
    let tries: [PathBuf; 2] = match path.strip_prefix("remote/") {
        Some(rest) => [remote.join(rest), remote.join(rest)],
        None => [remote.join("override").join(path), repo.join("desktop-app").join("src").join(path)],
    };
    tries.iter().find_map(|p| std::fs::read(p).ok())
}

/// The file at `path` ("js/player.js", no leading slash).
pub fn get(path: &str) -> Option<Cow<'static, [u8]>> {
    // A path out of the tree is nobody's file.
    if path.split(['/', '\\']).any(|part| part == "..") || path.contains(':') {
        return None;
    }
    if let Some(repo) = std::env::var_os("AURA_WEB_DIR") {
        return from_disk(std::path::Path::new(&repo), path).map(Cow::Owned);
    }
    table().get(path).map(|b| Cow::Borrowed(*b))
}

pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "glsl" | "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A page goes out with the bridge to the server before anything of its own,
/// and the remote's look after the app's; the main page with the remote's own
/// script at the end.
pub fn serve_page(path: &str, html: &str) -> String {
    let mut s = html.replacen("<head>", "<head>\n<script src=\"/remote/shim.js\"></script>", 1);
    s = s.replacen("</head>", "<link rel=\"stylesheet\" href=\"/remote/remote.css\">\n</head>", 1);
    if path == "index.html" {
        // The browser's bar in the remote's colour. A web page only: no
        // manifest, nothing to install (2.0.0 serves plain http).
        s = s.replacen("</head>", "<meta name=\"theme-color\" content=\"#0b1426\">\n</head>", 1);
        s = s.replacen("</body>", "<script type=\"module\" src=\"/remote/remote.js\"></script>\n</body>", 1);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_out_of_the_tree_is_refused() {
        assert!(get("../Cargo.toml").is_none());
        assert!(get("js/../../secret").is_none());
        assert!(get("C:/Windows/win.ini").is_none());
    }

    #[test]
    fn the_remotes_override_stands_before_the_apps_file() {
        let shown = String::from_utf8(get("js/aura-url.js").unwrap().into_owned()).unwrap();
        assert!(shown.contains("/aura"), "{shown}");
        assert!(get("index.html").is_some() && get("remote/shim.js").is_some());
        assert!(get("manifest.webmanifest").is_none() && get("sw.js").is_none());
    }

    #[test]
    fn a_page_gets_the_bridge_first_and_only_the_main_page_gets_the_remote() {
        let page = "<html><head><title>x</title></head><body>b</body></html>";
        let main = serve_page("index.html", page);
        assert!(main.find("shim.js").unwrap() < main.find("<title>").unwrap());
        assert!(main.find("remote.css").unwrap() > main.find("</title>").unwrap());
        assert!(main.contains("remote.js"));
        assert!(!serve_page("eq.html", page).contains("remote.js"));
        assert!(!main.contains("manifest") && main.contains("theme-color"));
    }
}
