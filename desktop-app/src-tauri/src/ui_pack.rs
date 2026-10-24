//! Interface pack: a signed zip of the interface (`desktop-app/src`) for the
//! exact version of this exe, served instead of the interface built into it.
//!
//! A fix that touches only the interface reaches users as a pack on the
//! release of the same version, without a new exe or a new version number:
//!
//! ```text
//! <exe folder>/ui/aura-engine-ui-v<version>-p<N>.zip
//! <exe folder>/ui/aura-engine-ui-v<version>-p<N>.zip.sig
//! ```
//!
//! The `.sig` is base64 of an Ed25519 signature (the app updates' key) over
//!
//! ```text
//! aura-engine-ui-v1\n<tag, e.g. v1.6.2>\n<N>\n<asset file name>\n<lowercase hex SHA-256 of the zip>\n
//! ```
//!
//! — a first line of its own, so a pack's signature is never an app update's
//! and an app update's never a pack's. The pack is checked at every start,
//! not only when it is downloaded: its bytes are read once, hashed, verified
//! and parsed from the same buffer. Any failure leaves the built-in interface
//! and a `[UI] pack … ignored: <why>` line in the log. `--no-ui-pack` on the
//! command line skips the pack altogether.
//!
//! Assets are served pack first, the built-in ones for anything the pack does
//! not have, through the same `tauri://localhost` origin, so the pages talk to
//! the app exactly as the built-in ones do.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tauri::utils::assets::{AssetKey, Assets, CspHash, EmbeddedAssets};

/// The folder beside the exe that holds the pack.
pub const UI_DIR: &str = "ui";
/// The command-line flag that keeps the built-in interface.
pub const NO_UI_PACK_FLAG: &str = "--no-ui-pack";
/// The pack's own description inside the zip (not served).
pub const MANIFEST: &str = "ui-pack.json";

const NAME_PREFIX: &str = "aura-engine-ui-v";
const MESSAGE_DOMAIN: &str = "aura-engine-ui-v1";

/// The pack file itself.
pub const MAX_PACK_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 1000;
const MAX_ENTRY_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 40 * 1024 * 1024;

/// What the interface consists of; anything else in a pack refuses it.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "html", "js", "mjs", "css", "json", "svg", "png", "ico", "woff2", "aura-vis", "glsl", "md", "txt",
];

// ── names and the signed message ─────────────────────────────────────────

/// `aura-engine-ui-v<version>-p<patch>.zip` (made by tools/make_ui_pack.py;
/// the app only reads the name, `parse_asset_name`).
#[cfg(test)]
pub fn asset_name(version: &str, patch: u32) -> String {
    format!("{NAME_PREFIX}{version}-p{patch}.zip")
}

/// The version and patch of a pack's file name, when it is exactly
/// `aura-engine-ui-v<X.Y.Z>-p<N>.zip` with N ≥ 1 written without leading zeros.
pub fn parse_asset_name(name: &str) -> Option<(String, u32)> {
    let rest = name.strip_prefix(NAME_PREFIX)?.strip_suffix(".zip")?;
    let (version, patch) = rest.rsplit_once("-p")?;
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    if patch.is_empty() || patch.starts_with('0') || !patch.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = patch.parse().ok()?;
    Some((version.to_string(), n))
}

/// The exact message a pack's `.sig` signs.
pub fn build_ui_message(tag: &str, patch: u32, asset_name: &str, sha256_hex: &str) -> String {
    format!("{MESSAGE_DOMAIN}\n{tag}\n{patch}\n{asset_name}\n{sha256_hex}\n")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

// ── the pack ─────────────────────────────────────────────────────────────

/// A verified pack: its files keyed as the built-in assets are (`/js/main.js`).
pub struct Pack {
    pub patch: u32,
    pub sha256: String,
    pub commit: String,
    pub files: HashMap<String, Vec<u8>>,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pack(UI {}, {} files, sha256 {})", self.patch, self.files.len(), self.sha256)
    }
}

/// A path inside the pack: relative, `/`-separated, ASCII letters, digits and
/// `-_.` only, no empty, `.` or `..` component, an allowed extension.
fn check_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.starts_with('/') || name.ends_with('/') {
        return Err(format!("bad entry name {name:?}"));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b)) {
        return Err(format!("bad character in entry name {name:?}"));
    }
    if name.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(format!("bad path in entry name {name:?}"));
    }
    let file = name.rsplit('/').next().unwrap_or(name);
    let ext = file.rsplit_once('.').map_or("", |(_, e)| e);
    if !ALLOWED_EXTENSIONS.contains(&ext) {
        return Err(format!("unexpected file type {name:?}"));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct Manifest {
    format: u32,
    tag: String,
    patch: u32,
    #[serde(default)]
    commit: String,
}

/// The zip's files and manifest, within the caps; the manifest must name the
/// same tag and patch as the file name and the signature.
fn parse_zip(bytes: &[u8], tag: &str, patch: u32) -> Result<(HashMap<String, Vec<u8>>, String), String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("not a zip: {e}"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!("{} entries (at most {MAX_ENTRIES})", zip.len()));
    }
    let mut files = HashMap::new();
    let mut manifest: Option<Manifest> = None;
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            return Err(format!("directory entry {name:?}"));
        }
        if entry.size() > MAX_ENTRY_BYTES {
            return Err(format!("{name} is {} bytes (at most {MAX_ENTRY_BYTES})", entry.size()));
        }
        let mut buf = Vec::with_capacity(entry.size() as usize);
        (&mut entry)
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("{name}: {e}"))?;
        if buf.len() as u64 > MAX_ENTRY_BYTES {
            return Err(format!("{name} is larger than {MAX_ENTRY_BYTES} bytes"));
        }
        total += buf.len() as u64;
        if total > MAX_TOTAL_BYTES {
            return Err(format!("more than {MAX_TOTAL_BYTES} bytes in all"));
        }
        if name == MANIFEST {
            let m: Manifest = serde_json::from_slice(&buf).map_err(|e| format!("{MANIFEST}: {e}"))?;
            manifest = Some(m);
            continue;
        }
        check_entry_name(&name)?;
        let key = AssetKey::from(name.as_str()).as_ref().to_string();
        if files.insert(key, buf).is_some() {
            return Err(format!("{name} twice"));
        }
    }
    let m = manifest.ok_or_else(|| format!("no {MANIFEST}"))?;
    if m.format != 1 {
        return Err(format!("{MANIFEST} format {}", m.format));
    }
    if m.tag != tag || m.patch != patch {
        return Err(format!("{MANIFEST} says {} UI {}, the file {tag} UI {patch}", m.tag, m.patch));
    }
    if !files.contains_key("/index.html") {
        return Err("no index.html".to_string());
    }
    Ok((files, m.commit))
}

/// Checks a pack for `version` from its bytes and its `.sig` text: the name,
/// the signature over the bytes' SHA-256, then the contents.
pub fn verify_pack(
    bytes: &[u8],
    sig_text: &str,
    asset: &str,
    version: &str,
    pubkey: &[u8; 32],
) -> Result<Pack, String> {
    let (v, patch) = parse_asset_name(asset).ok_or_else(|| format!("{asset} is not a pack's name"))?;
    if v != version {
        return Err(format!("for {v}, this is {version}"));
    }
    if bytes.len() as u64 > MAX_PACK_BYTES {
        return Err(format!("{} bytes (at most {MAX_PACK_BYTES})", bytes.len()));
    }
    let tag = format!("v{version}");
    let sha = sha256_hex(bytes);
    let message = build_ui_message(&tag, patch, asset, &sha);
    crate::updater::verify_ed25519(pubkey, message.as_bytes(), sig_text.trim())
        .map_err(|e| format!("signature: {e}"))?;
    let (files, commit) = parse_zip(bytes, &tag, patch)?;
    Ok(Pack { patch, sha256: sha, commit, files })
}

// ── the folder beside the exe ────────────────────────────────────────────

pub fn ui_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|d| d.join(UI_DIR))
}

/// What a file in the ui folder is: a pack (or its signature, or a download
/// left half-written) of some version and patch.
fn classify(name: &str) -> Option<(String, u32)> {
    let base = name
        .strip_suffix(".tmp")
        .unwrap_or(name);
    let base = base.strip_suffix(".sig").unwrap_or(base);
    parse_asset_name(base)
}

fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    f.take(cap + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() as u64 > cap {
        return Err(format!("larger than {cap} bytes"));
    }
    Ok(buf)
}

/// The highest valid pack for `version` in `dir`. Packs of other versions,
/// half-written downloads and packs below the one taken are deleted; a pack
/// that fails its checks is logged and left (the next one down is tried).
pub fn load_from_dir(dir: &Path, version: &str, pubkey: Option<&[u8; 32]>) -> Option<Pack> {
    let Ok(rd) = std::fs::read_dir(dir) else { return None };
    let mut candidates: Vec<(u32, String)> = Vec::new();
    let mut ours: Vec<(u32, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((v, patch)) = classify(&name) else { continue };
        if v != version || name.ends_with(".tmp") {
            match std::fs::remove_file(e.path()) {
                Ok(()) => crate::aelog!("[UI] pack {name} removed: not for {version}"),
                Err(err) => crate::aelog!("[UI] pack {name}: cannot remove: {err}"),
            }
            continue;
        }
        if name.ends_with(".zip") {
            candidates.push((patch, name.clone()));
        }
        ours.push((patch, e.path()));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let mut taken: Option<Pack> = None;
    for (_, name) in &candidates {
        let Some(key) = pubkey else {
            crate::aelog!("[UI] pack {name} ignored: no public key in this build");
            return None;
        };
        let zip_path = dir.join(name);
        let sig_path = dir.join(format!("{name}.sig"));
        let result = read_capped(&sig_path, 4096)
            .map_err(|e| format!("signature file: {e}"))
            .and_then(|sig| String::from_utf8(sig).map_err(|_| "signature file is not text".to_string()))
            .and_then(|sig| {
                let bytes = read_capped(&zip_path, MAX_PACK_BYTES)?;
                verify_pack(&bytes, &sig, name, version, key)
            });
        match result {
            Ok(p) => {
                taken = Some(p);
                break;
            }
            Err(why) => crate::aelog!("[UI] pack {name} ignored: {why}"),
        }
    }
    if let Some(p) = &taken {
        for (patch, path) in &ours {
            if *patch < p.patch {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    taken
}

/// Writes a verified pack into `dir`: both files through `.tmp` names and a
/// rename, the zip last but one, so a start in between finds either the old
/// pack or a whole new one (a zip without its `.sig` is ignored).
pub fn write_pack(dir: &Path, asset: &str, bytes: &[u8], sig_text: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let zip_path = dir.join(asset);
    let sig_path = dir.join(format!("{asset}.sig"));
    let zip_tmp = dir.join(format!("{asset}.tmp"));
    let sig_tmp = dir.join(format!("{asset}.sig.tmp"));
    let write = |p: &Path, b: &[u8]| -> Result<(), String> {
        let mut f = std::fs::File::create(p).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        std::io::Write::write_all(&mut f, b).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        f.sync_all().map_err(|e| format!("cannot write {}: {e}", p.display()))
    };
    let r = write(&zip_tmp, bytes)
        .and_then(|_| write(&sig_tmp, sig_text.trim().as_bytes()))
        .and_then(|_| std::fs::rename(&zip_tmp, &zip_path).map_err(|e| format!("cannot place {asset}: {e}")))
        .and_then(|_| std::fs::rename(&sig_tmp, &sig_path).map_err(|e| format!("cannot place {asset}.sig: {e}")));
    if r.is_err() {
        let _ = std::fs::remove_file(&zip_tmp);
        let _ = std::fs::remove_file(&sig_tmp);
    }
    r
}

// ── this run's pack ──────────────────────────────────────────────────────

/// What this run serves: `None` when the built-in interface is used.
struct Active {
    patch: u32,
    sha256: String,
    commit: String,
    files: Arc<HashMap<String, Vec<u8>>>,
}

static ACTIVE: OnceLock<Option<Active>> = OnceLock::new();

/// `--no-ui-pack` among the command-line arguments.
fn built_in_asked(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|a| a == NO_UI_PACK_FLAG)
}

fn active() -> &'static Option<Active> {
    ACTIVE.get_or_init(|| {
        if built_in_asked(std::env::args()) {
            crate::aelog!("[UI] {NO_UI_PACK_FLAG}: the built-in interface");
            return None;
        }
        let dir = ui_dir()?;
        let key = crate::updater::active_pubkey();
        let pack = load_from_dir(&dir, env!("CARGO_PKG_VERSION"), key.as_ref())?;
        crate::aelog!(
            "[UI] interface update {} for {} (sha256 {}, {} files)",
            pack.patch,
            env!("CARGO_PKG_VERSION"),
            &pack.sha256[..12],
            pack.files.len()
        );
        Some(Active { patch: pack.patch, sha256: pack.sha256, commit: pack.commit, files: Arc::new(pack.files) })
    })
}

/// Loads this run's pack (once); call before the version is first shown.
pub fn init() {
    let _ = active();
}

/// The patch of the interface this run serves, 0 for the built-in one.
pub fn active_patch() -> u32 {
    active().as_ref().map_or(0, |a| a.patch)
}

/// `1.6.2`, or `1.6.2 · UI 1` while a pack is served.
pub fn version_label() -> String {
    label(env!("CARGO_PKG_VERSION"), active_patch())
}

pub fn label(version: &str, patch: u32) -> String {
    if patch == 0 {
        version.to_string()
    } else {
        format!("{version} · UI {patch}")
    }
}

#[tauri::command]
pub fn ui_pack_info() -> serde_json::Value {
    let a = active().as_ref();
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "patch": a.map_or(0, |a| a.patch),
        "sha256": a.map(|a| a.sha256.as_str()).unwrap_or(""),
        "commit": a.map(|a| a.commit.as_str()).unwrap_or(""),
        "label": version_label(),
    })
}

// ── serving ──────────────────────────────────────────────────────────────

/// The pack's files first, the built-in ones for the rest.
pub struct PackAssets<E: Assets = EmbeddedAssets> {
    embedded: Arc<E>,
    pack: Option<Arc<HashMap<String, Vec<u8>>>>,
}

impl<E: Assets> Assets for PackAssets<E> {
    fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
        if let Some(b) = self.pack.as_ref().and_then(|p| p.get(key.as_ref())) {
            return Some(Cow::Borrowed(b.as_slice()));
        }
        self.embedded.get(key)
    }

    fn iter(&self) -> Box<dyn Iterator<Item = (&&str, &&[u8])> + '_> {
        self.embedded.iter()
    }

    fn csp_hashes(&self, html_path: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        self.embedded.csp_hashes(html_path)
    }
}

/// The generated context with its assets wrapped. Everything else is the
/// generated context's own (the config, icons, package info, shell scope);
/// the app icon is set only for a macOS dev build, which this is not.
pub fn context(ctx: tauri::Context<EmbeddedAssets>) -> tauri::Context<PackAssets> {
    let pack = active().as_ref().map(|a| a.files.clone());
    let assets = Arc::new(PackAssets { embedded: ctx.assets(), pack });
    tauri::Context::new(
        ctx.config().clone(),
        assets,
        ctx.default_window_icon().cloned(),
        None,
        ctx.system_tray_icon().cloned(),
        ctx.package_info().clone(),
        (),
        tauri::Pattern::Brownfield(std::marker::PhantomData),
        ctx.allowed_commands().clone(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};

    pub(crate) fn key() -> SigningKey {
        SigningKey::generate(&mut rand::rngs::OsRng)
    }

    pub(crate) fn sign(key: &SigningKey, message: &str) -> String {
        STANDARD.encode(key.sign(message.as_bytes()).to_bytes())
    }

    /// A zip as make_ui_pack.py writes it: the manifest, then the files.
    pub(crate) fn zip_of(manifest: Option<String>, files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opt = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        if let Some(m) = manifest {
            w.start_file(MANIFEST, opt).unwrap();
            w.write_all(m.as_bytes()).unwrap();
        }
        for (n, b) in files {
            w.start_file(*n, opt).unwrap();
            w.write_all(b).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    pub(crate) fn manifest(tag: &str, patch: u32) -> String {
        format!(r#"{{"format":1,"tag":"{tag}","patch":{patch},"base":"b","commit":"c0ffee"}}"#)
    }

    /// A signed pack for `version` UI `patch` with `index` as its index.html.
    pub(crate) fn signed_pack(key: &SigningKey, version: &str, patch: u32, index: &[u8]) -> (String, Vec<u8>, String) {
        let name = asset_name(version, patch);
        let bytes = zip_of(Some(manifest(&format!("v{version}"), patch)), &[("index.html", index), ("js/main.js", b"//")]);
        let sig = sign(key, &build_ui_message(&format!("v{version}"), patch, &name, &sha256_hex(&bytes)));
        (name, bytes, sig)
    }

    #[test]
    fn names_are_exact() {
        assert_eq!(asset_name("1.6.2", 3), "aura-engine-ui-v1.6.2-p3.zip");
        assert_eq!(parse_asset_name("aura-engine-ui-v1.6.2-p3.zip"), Some(("1.6.2".into(), 3)));
        assert_eq!(parse_asset_name("aura-engine-ui-v1.6.2-p12.zip"), Some(("1.6.2".into(), 12)));
        for bad in [
            "aura-engine-ui-v1.6.2-p0.zip",
            "aura-engine-ui-v1.6.2-p01.zip",
            "aura-engine-ui-v1.6.2-p.zip",
            "aura-engine-ui-v1.6-p1.zip",
            "aura-engine-ui-v1.6.2.1-p1.zip",
            "aura-engine-ui-v1.6.2-p1.zip.sig",
            "aura-engine-ui-v1.6.2-p1.md",
            "aura-engine-ui-1.6.2-p1.zip",
            "aura-engine-v1.6.2-windows-x64.zip",
            "aura-engine-ui-v1.6.2-p1-windows-x64.zip",
            "aura-engine-ui-v1.6.2-px.zip",
            "aura-engine-ui-v1.6.2-p99999999999.zip",
        ] {
            assert_eq!(parse_asset_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_message_has_its_own_domain() {
        let m = build_ui_message("v1.6.2", 2, "aura-engine-ui-v1.6.2-p2.zip", "ab");
        assert_eq!(m, "aura-engine-ui-v1\nv1.6.2\n2\naura-engine-ui-v1.6.2-p2.zip\nab\n");
        let app = crate::updater::build_update_message("v1.6.2", "aura-engine-ui-v1.6.2-p2.zip", "ab");
        assert!(!app.starts_with("aura-engine-ui-v1\n"));
        assert_ne!(m, app);
    }

    #[test]
    fn a_signed_pack_verifies() {
        let k = key();
        let (name, bytes, sig) = signed_pack(&k, "1.6.2", 1, b"<p>new</p>");
        let p = verify_pack(&bytes, &sig, &name, "1.6.2", &k.verifying_key().to_bytes()).unwrap();
        assert_eq!(p.patch, 1);
        assert_eq!(p.commit, "c0ffee");
        assert_eq!(p.files["/index.html"], b"<p>new</p>");
        assert!(p.files.contains_key("/js/main.js"));
        assert!(!p.files.keys().any(|k| k.contains(MANIFEST)), "the manifest is not served");
    }

    #[test]
    fn a_wrong_signature_is_refused() {
        let k = key();
        let pk = k.verifying_key().to_bytes();
        let (name, bytes, sig) = signed_pack(&k, "1.6.2", 1, b"x");
        // another key
        let other = key().verifying_key().to_bytes();
        assert!(verify_pack(&bytes, &sig, &name, "1.6.2", &other).unwrap_err().contains("signature"));
        // another version's tag in the message
        let sig_tag = sign(&k, &build_ui_message("v1.6.3", 1, &name, &sha256_hex(&bytes)));
        assert!(verify_pack(&bytes, &sig_tag, &name, "1.6.2", &pk).is_err());
        // the same bytes under another patch's name
        let p2 = asset_name("1.6.2", 2);
        assert!(verify_pack(&bytes, &sig, &p2, "1.6.2", &pk).is_err());
        // a message naming another asset
        let sig_name = sign(&k, &build_ui_message("v1.6.2", 1, "aura-engine-ui-v1.6.2-p1.zip.x", &sha256_hex(&bytes)));
        assert!(verify_pack(&bytes, &sig_name, &name, "1.6.2", &pk).is_err());
        // a changed byte
        let mut changed = bytes.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(verify_pack(&changed, &sig, &name, "1.6.2", &pk).is_err());
        // garbage
        assert!(verify_pack(&bytes, "not base64!", &name, "1.6.2", &pk).is_err());
    }

    #[test]
    fn app_and_pack_signatures_never_stand_for_each_other() {
        let k = key();
        let pk = k.verifying_key().to_bytes();
        let (name, bytes, ui_sig) = signed_pack(&k, "1.6.2", 1, b"x");
        let sha = sha256_hex(&bytes);
        // an app update's signature of these very bytes under this name
        let app_sig = sign(&k, &crate::updater::build_update_message("v1.6.2", &name, &sha));
        assert!(verify_pack(&bytes, &app_sig, &name, "1.6.2", &pk).is_err());
        // the pack's signature as an app update's
        let app_msg = crate::updater::build_update_message("v1.6.2", &name, &sha);
        assert!(crate::updater::verify_ed25519(&pk, app_msg.as_bytes(), &ui_sig).is_err());
    }

    #[test]
    fn a_pack_for_another_version_is_refused() {
        let k = key();
        let (name, bytes, sig) = signed_pack(&k, "1.6.2", 1, b"x");
        let e = verify_pack(&bytes, &sig, &name, "1.6.3", &k.verifying_key().to_bytes()).unwrap_err();
        assert!(e.contains("1.6.2"), "{e}");
    }

    /// Signs whatever zip the case builds, so only the contents are judged.
    fn contents_error(manifest_: Option<String>, files: &[(&str, &[u8])]) -> String {
        let k = key();
        let name = asset_name("1.6.2", 1);
        let bytes = zip_of(manifest_, files);
        let sig = sign(&k, &build_ui_message("v1.6.2", 1, &name, &sha256_hex(&bytes)));
        match verify_pack(&bytes, &sig, &name, "1.6.2", &k.verifying_key().to_bytes()) {
            Ok(_) => String::new(),
            Err(e) => e,
        }
    }

    #[test]
    fn the_contents_are_checked() {
        let m = || Some(manifest("v1.6.2", 1));
        let idx: (&str, &[u8]) = ("index.html", b"i");
        assert_eq!(contents_error(m(), &[idx]), "");
        assert!(contents_error(None, &[idx]).contains("no ui-pack.json"));
        assert!(contents_error(Some(manifest("v1.6.2", 2)), &[idx]).contains("UI 2"));
        assert!(contents_error(Some(manifest("v1.6.1", 1)), &[idx]).contains("v1.6.1"));
        assert!(contents_error(m(), &[("js/a.js", b"")]).contains("no index.html"));
        assert!(contents_error(m(), &[idx, idx]).contains("twice"));
        for bad in [
            "../index.html",
            "js/../../x.js",
            "/abs.js",
            "C:/x.js",
            "js\\x.js",
            "./x.js",
            "js//x.js",
            "x.exe",
            "x.dll",
            "Makefile",
            "js/x.js.bat",
            "js/x y.js",
            "js/\u{44f}.js",
        ] {
            let e = contents_error(m(), &[idx, (bad, b"")]);
            assert!(!e.is_empty(), "{bad} was taken");
        }
        for good in ["vis/aurora.aura-vis", "js/analytics/protocol.test.mjs", "favicon.ico", "css/a-b_c.css", "vis/A.md"] {
            assert_eq!(contents_error(m(), &[idx, (good, b"")]), "", "{good}");
        }
    }

    #[test]
    fn the_caps_hold() {
        let m = || Some(manifest("v1.6.2", 1));
        let names: Vec<String> = (0..MAX_ENTRIES).map(|i| format!("js/f{i}.js")).collect();
        let mut files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        files.push(("index.html", b"i"));
        assert!(contents_error(m(), &files).contains("entries"));
        let big = vec![b' '; MAX_ENTRY_BYTES as usize + 1];
        assert!(contents_error(m(), &[("index.html", b"i"), ("js/big.js", &big)]).contains("bytes"));
        let nine = vec![b' '; 9 * 1024 * 1024];
        let many: Vec<String> = (0..5).map(|i| format!("js/n{i}.js")).collect();
        let mut files: Vec<(&str, &[u8])> = many.iter().map(|n| (n.as_str(), nine.as_slice())).collect();
        files.push(("index.html", b"i"));
        assert!(contents_error(m(), &files).contains("in all"));
    }

    fn place(dir: &Path, name: &str, bytes: &[u8], sig: &str) {
        std::fs::write(dir.join(name), bytes).unwrap();
        std::fs::write(dir.join(format!("{name}.sig")), sig).unwrap();
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn the_highest_valid_pack_is_taken_and_the_rest_cleaned() {
        let d = tempfile::tempdir().unwrap();
        let k = key();
        let pk = k.verifying_key().to_bytes();
        for (v, n) in [("1.6.2", 1), ("1.6.2", 2), ("1.6.1", 5)] {
            let (name, bytes, sig) = signed_pack(&k, v, n, format!("{v}/{n}").as_bytes());
            place(d.path(), &name, &bytes, &sig);
        }
        // p3: a bad signature — ignored, left in place
        let (name3, bytes3, _) = signed_pack(&k, "1.6.2", 3, b"3");
        place(d.path(), &name3, &bytes3, &sign(&key(), "x"));
        // a half-written download, and a file of someone else's
        std::fs::write(d.path().join("aura-engine-ui-v1.6.2-p4.zip.tmp"), b"half").unwrap();
        std::fs::write(d.path().join("notes.txt"), b"mine").unwrap();

        let p = load_from_dir(d.path(), "1.6.2", Some(&pk)).unwrap();
        assert_eq!(p.patch, 2);
        assert_eq!(p.files["/index.html"], b"1.6.2/2");
        assert_eq!(
            names_in(d.path()),
            [
                "aura-engine-ui-v1.6.2-p2.zip",
                "aura-engine-ui-v1.6.2-p2.zip.sig",
                "aura-engine-ui-v1.6.2-p3.zip",
                "aura-engine-ui-v1.6.2-p3.zip.sig",
                "notes.txt",
            ]
        );
    }

    #[test]
    fn without_a_key_or_a_valid_pack_the_built_in_interface_stays() {
        let d = tempfile::tempdir().unwrap();
        let k = key();
        let pk = k.verifying_key().to_bytes();
        let (name, bytes, sig) = signed_pack(&k, "1.6.2", 1, b"x");
        place(d.path(), &name, &bytes, &sig);
        assert!(load_from_dir(d.path(), "1.6.2", None).is_none());
        assert!(load_from_dir(d.path(), "1.6.2", Some(&key().verifying_key().to_bytes())).is_none());
        // a zip without its .sig
        std::fs::remove_file(d.path().join(format!("{name}.sig"))).unwrap();
        assert!(load_from_dir(d.path(), "1.6.2", Some(&pk)).is_none());
        // no folder at all
        assert!(load_from_dir(&d.path().join("none"), "1.6.2", Some(&pk)).is_none());
        // the pack of another version is deleted
        let (n2, b2, s2) = signed_pack(&k, "1.6.1", 1, b"x");
        place(d.path(), &n2, &b2, &s2);
        assert!(load_from_dir(d.path(), "1.6.2", Some(&pk)).is_none());
        assert_eq!(names_in(d.path()), [name]);
    }

    #[test]
    fn a_written_pack_loads_and_leaves_no_temporary_files() {
        let d = tempfile::tempdir().unwrap();
        let ui = d.path().join(UI_DIR);
        let k = key();
        let (name, bytes, sig) = signed_pack(&k, "1.6.2", 4, b"four");
        write_pack(&ui, &name, &bytes, &sig).unwrap();
        assert_eq!(names_in(&ui), [name.clone(), format!("{name}.sig")]);
        let p = load_from_dir(&ui, "1.6.2", Some(&k.verifying_key().to_bytes())).unwrap();
        assert_eq!(p.files["/index.html"], b"four");
    }

    /// A stand-in for the built-in assets.
    struct Fixed(HashMap<String, Vec<u8>>);
    impl Assets for Fixed {
        fn get(&self, key: &AssetKey) -> Option<Cow<'_, [u8]>> {
            self.0.get(key.as_ref()).map(|b| Cow::Borrowed(b.as_slice()))
        }
        fn iter(&self) -> Box<dyn Iterator<Item = (&&str, &&[u8])> + '_> {
            Box::new(std::iter::empty())
        }
        fn csp_hashes(&self, _: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
            Box::new(std::iter::empty())
        }
    }

    #[test]
    fn the_pack_comes_first_the_built_in_interface_after() {
        let built_in: HashMap<String, Vec<u8>> =
            [("/index.html", "old"), ("/js/only-built-in.js", "b")].map(|(k, v)| (k.to_string(), v.into())).into();
        let k = key();
        let (name, bytes, sig) = signed_pack(&k, "1.6.2", 1, b"new");
        let pack = verify_pack(&bytes, &sig, &name, "1.6.2", &k.verifying_key().to_bytes()).unwrap();
        let a = PackAssets { embedded: Arc::new(Fixed(built_in.clone())), pack: Some(Arc::new(pack.files)) };
        // the keys Tauri asks with (manager.rs get_asset: AssetKey::from the path)
        assert_eq!(a.get(&AssetKey::from("index.html")).unwrap().as_ref(), b"new");
        assert_eq!(a.get(&AssetKey::from("js/main.js")).unwrap().as_ref(), b"//");
        assert_eq!(a.get(&AssetKey::from("js/only-built-in.js")).unwrap().as_ref(), b"b");
        assert!(a.get(&AssetKey::from("js/none.js")).is_none());
        let none = PackAssets { embedded: Arc::new(Fixed(built_in)), pack: None };
        assert_eq!(none.get(&AssetKey::from("index.html")).unwrap().as_ref(), b"old");
    }

    /// A pack made by tools/make_ui_pack.py and signed by `sign_update.py
    /// sign-ui` with a test key loads here, as at a start: the pack in a folder
    /// of its own, AURA_UI_PACK_TEST_ZIP=<its path>, AURA_UI_PACK_TEST_PUBKEY=<hex>.
    #[test]
    #[ignore]
    fn a_pack_from_the_tools_loads() {
        let zip = PathBuf::from(std::env::var("AURA_UI_PACK_TEST_ZIP").expect("AURA_UI_PACK_TEST_ZIP"));
        let hex = std::env::var("AURA_UI_PACK_TEST_PUBKEY").expect("AURA_UI_PACK_TEST_PUBKEY");
        let pk = crate::updater::decode_pubkey_hex(&hex).unwrap();
        let (version, patch) = parse_asset_name(&zip.file_name().unwrap().to_string_lossy()).unwrap();
        let p = load_from_dir(zip.parent().unwrap(), &version, Some(&pk)).expect("the pack loads");
        assert_eq!(p.patch, patch);
        assert!(p.files.contains_key("/index.html"));
        assert!(p.files.contains_key("/js/main.js"));
        println!("{p:?}, commit {}", p.commit);
    }

    #[test]
    fn no_ui_pack_on_the_command_line_keeps_the_built_in_interface() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
        assert!(built_in_asked(args(&["aura-engine.exe", "--no-ui-pack"])));
        assert!(built_in_asked(args(&["aura-engine.exe", "--after-update", "12", "--no-ui-pack"])));
        assert!(!built_in_asked(args(&["aura-engine.exe"])));
        assert!(!built_in_asked(args(&["aura-engine.exe", "--no-ui-packs", "no-ui-pack"])));
    }

    #[test]
    fn the_label_names_the_pack() {
        assert_eq!(label("1.6.2", 0), "1.6.2");
        assert_eq!(label("1.6.2", 1), "1.6.2 · UI 1");
    }
}
