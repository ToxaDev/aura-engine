//! How this copy of the app puts a verified update in its own place, by the
//! way it was installed (`updater` downloads and verifies; this places).
//!
//! * Windows — the zip beside the exe (`updater::do_install`, unchanged).
//! * Linux AppImage — the file `$APPIMAGE` names (when the exe runs from its
//!   mount, `$APPDIR`): the new one is copied
//!   beside it as `.<name>.new`, checked against the verified hash, made
//!   executable and renamed over it (atomic; the running app keeps its own
//!   inode). The old one stays as a hard link `.<name>.old` until the new one
//!   has started, for a rollback if it cannot be started.
//! * Linux .deb (an exe under `/usr/`, dpkg present) — the verified package
//!   installed under `pkexec` (the system asks for the password) by a fixed
//!   script that copies it into a folder of root's, checks the copy's hash
//!   and installs that copy with apt-get.
//! * macOS — the `.app` bundle the exe is in: a copy of the disk image, its
//!   hash checked again, is mounted read-only at a folder of ours, its one
//!   `.app` copied with `ditto` beside
//!   the bundle as `.<Name>.app.new`, its version checked, then renamed into
//!   place (the old one moved aside as `.<Name>.app.old`, put back on any
//!   failure).
//!
//! Anything else — a tarball or a development build, a macOS app run from the
//! disk image or translocated by Gatekeeper, a folder the app cannot write —
//! gets the release page with the reason, never a half-done install.
//!
//! The decisions are pure functions of paths and flags, so they are tested on
//! every system; the placing works on plain files and folders, tested on every
//! system too. Only the system tools (`pkexec`, `hdiutil`, `ditto`, `open`)
//! are left to the system they belong to.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How this copy can be updated.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallKind {
    /// Windows: the zip, beside the exe.
    WindowsZip,
    /// Linux: the AppImage file at this path.
    AppImage(PathBuf),
    /// Linux: installed from the .deb, under `/usr/`.
    Deb,
    /// macOS: the `.app` bundle at this path.
    MacApp(PathBuf),
    /// Updated from the release page, for this reason.
    Page(PageReason),
}

/// Why the update comes from the release page.
#[derive(Clone, Debug, PartialEq)]
pub enum PageReason {
    /// macOS runs the app from a random read-only place (App Translocation).
    Translocated,
    /// The app runs from the mounted disk image.
    OnDiskImage,
    /// The app's folder is not writable.
    NotWritable(PathBuf),
    /// The release has no signed .deb (an older release).
    NoSignedDeb,
    /// No `pkexec` to ask for the system password.
    NoPasswordPrompt,
    /// A tarball, a development build, or a layout not recognised.
    Other,
}

impl InstallKind {
    pub fn can_install(&self) -> bool {
        !matches!(self, InstallKind::Page(_))
    }

    /// The kind as the update dialog reads it.
    pub fn tag(&self) -> &'static str {
        match self {
            InstallKind::WindowsZip => "zip",
            InstallKind::AppImage(_) => "appimage",
            InstallKind::Deb => "deb",
            InstallKind::MacApp(_) => "app",
            InstallKind::Page(_) => "page",
        }
    }

    /// Why the dialog offers the page instead of installing; `None` when it installs.
    pub fn note(&self) -> Option<String> {
        let InstallKind::Page(reason) = self else { return None };
        Some(match reason {
            PageReason::Translocated | PageReason::OnDiskImage => {
                "Move Aura Engine to Applications first — then it can update itself.".to_string()
            }
            PageReason::NotWritable(dir) => format!(
                "Aura Engine can't write to {}, so this update comes from the download page.",
                dir.display()
            ),
            PageReason::NoSignedDeb => "This release installs from the download page.".to_string(),
            PageReason::NoPasswordPrompt => {
                "No system password prompt is available — install the update from the download page.".to_string()
            }
            PageReason::Other => "This copy of Aura Engine updates from the download page.".to_string(),
        })
    }
}

// ── which kind ────────────────────────────────────────────────────────────

/// The kind from what the system says: `os` as `std::env::consts::OS`, the
/// running exe, `$APPIMAGE` when it names an existing file, `$APPDIR`, and
/// whether dpkg is installed. Writability is not looked at here (see
/// [`current`]).
pub fn detect(os: &str, exe: &Path, appimage: Option<&Path>, appdir: Option<&Path>, has_dpkg: bool) -> InstallKind {
    match os {
        "windows" => InstallKind::WindowsZip,
        "linux" => {
            if let Some(a) = appimage_of(exe, appimage, appdir) {
                InstallKind::AppImage(a)
            } else if has_dpkg && exe.starts_with("/usr") && !exe.starts_with("/usr/local") {
                InstallKind::Deb
            } else {
                InstallKind::Page(PageReason::Other)
            }
        }
        "macos" => {
            if exe.to_string_lossy().contains("/AppTranslocation/") {
                return InstallKind::Page(PageReason::Translocated);
            }
            match mac_bundle_of(exe) {
                Some(b) => InstallKind::MacApp(b),
                None => InstallKind::Page(PageReason::Other),
            }
        }
        _ => InstallKind::Page(PageReason::Other),
    }
}

/// The AppImage this exe runs from: `$APPIMAGE`, only when the exe is inside
/// `$APPDIR`, the AppImage's own mount. Both are inherited by every program
/// started from an AppImage's shell, so a package's exe under `/usr/` that
/// merely sees them is not that AppImage.
pub fn appimage_of(exe: &Path, appimage: Option<&Path>, appdir: Option<&Path>) -> Option<PathBuf> {
    let a = appimage.filter(|a| a.has_root() && a.file_name().is_some())?;
    let dir = appdir.filter(|d| d.has_root() && d.file_name().is_some())?;
    exe.starts_with(dir).then(|| a.to_path_buf())
}

/// `$APPIMAGE` when it names a file, and `$APPDIR` resolved (as the exe path
/// is, for comparing the two).
fn appimage_env() -> (Option<PathBuf>, Option<PathBuf>) {
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file());
    let appdir = std::env::var_os("APPDIR").map(|d| std::fs::canonicalize(&d).unwrap_or_else(|_| PathBuf::from(d)));
    (appimage, appdir)
}

/// The AppImage this process runs from.
pub fn running_appimage(exe: &Path) -> Option<PathBuf> {
    let (appimage, appdir) = appimage_env();
    appimage_of(exe, appimage.as_deref(), appdir.as_deref())
}

/// The `.app` an exe at `<X>.app/Contents/MacOS/<bin>` belongs to.
pub fn mac_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_app = bundle.extension().map_or(false, |e| e == "app");
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app).then(|| bundle.to_path_buf())
}

/// The reason a bundle whose folder cannot be written gets the page: on the
/// mounted disk image (under `/Volumes/`, read-only) it is to be moved first.
pub fn unwritable_reason(dir: &Path) -> PageReason {
    if dir.starts_with("/Volumes") {
        PageReason::OnDiskImage
    } else {
        PageReason::NotWritable(dir.to_path_buf())
    }
}

/// This run's kind, writability and the tools it needs checked.
pub fn current() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Page(PageReason::Other);
    };
    let (appimage, appdir) = appimage_env();
    let has_dpkg = Path::new(DPKG).is_file();
    usable(detect(std::env::consts::OS, &exe, appimage.as_deref(), appdir.as_deref(), has_dpkg))
}

/// `kind`, or the page when what it needs is not there now.
pub fn usable(kind: InstallKind) -> InstallKind {
    match &kind {
        InstallKind::AppImage(file) => match file.parent() {
            Some(dir) if dir_writable(dir) => kind,
            Some(dir) => InstallKind::Page(PageReason::NotWritable(dir.to_path_buf())),
            None => InstallKind::Page(PageReason::Other),
        },
        InstallKind::MacApp(bundle) => match bundle.parent() {
            Some(dir) if dir_writable(dir) => kind,
            Some(dir) => InstallKind::Page(unwritable_reason(dir)),
            None => InstallKind::Page(PageReason::Other),
        },
        InstallKind::Deb if !Path::new(PKEXEC).is_file() => InstallKind::Page(PageReason::NoPasswordPrompt),
        _ => kind,
    }
}

/// Can a file be made in `dir` (and removed again)?
pub fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".aura-update-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ── placing: plain files and folders ──────────────────────────────────────

/// `<dir>/.<name>.<what>`, the hidden sibling of `live`.
pub fn sibling(live: &Path, what: &str) -> Option<PathBuf> {
    let name = live.file_name()?.to_string_lossy();
    Some(live.parent()?.join(format!(".{name}.{what}")))
}

/// Clears `p` for a new use: removed, or — when it cannot be (an old bundle
/// whose folders belong to root after an installer package) — renamed aside
/// to `<p>-<stamp>`, so a leftover never stands in the way of an update. An
/// error only when it can be neither (a folder that cannot be written).
pub fn clear_path(p: &Path) -> Result<(), String> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
    };
    let removed = if meta.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    let Err(e) = removed else { return Ok(()) };
    let aside = aside_name(p).ok_or_else(|| format!("cannot remove {}: {e}", p.display()))?;
    std::fs::rename(p, &aside).map_err(|e2| format!("cannot remove {} ({e}) or move it aside ({e2})", p.display()))?;
    crate::aelog!("[UPDATER] {} could not be removed ({}); moved aside to {}", p.display(), e, aside.display());
    Ok(())
}

/// A free `<p>-<stamp>` beside `p`.
fn aside_name(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_string_lossy().into_owned();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (0..100)
        .map(|n| if n == 0 { format!("{name}-{stamp}") } else { format!("{name}-{stamp}-{n}") })
        .map(|n| p.with_file_name(n))
        .find(|c| std::fs::symlink_metadata(c).is_err())
}

/// Puts the verified AppImage `src` in the place of `live`: a copy beside it,
/// its hash checked again against `sha256_hex` (the one the signature
/// covers), made executable and synced, then renamed over `live`. The old
/// file is kept as a hard link `.<name>.old` for [`restore_file`]; `None`
/// when the link could not be made (the update stands without it).
pub fn place_file(src: &Path, live: &Path, sha256_hex: &str) -> Result<Option<PathBuf>, String> {
    let new = sibling(live, "new").ok_or("the app's path has no folder")?;
    let old = sibling(live, "old").ok_or("the app's path has no folder")?;
    clear_path(&new)?;
    // A leftover .old that stays only costs the rollback copy (see below).
    let _ = clear_path(&old);
    let staged = (|| -> Result<(), String> {
        std::fs::copy(src, &new).map_err(|e| format!("cannot write {}: {e}", new.display()))?;
        let copied = crate::updater::sha256_file(&new)?;
        if copied != sha256_hex {
            return Err(format!("the copy at {} does not match the verified download", new.display()));
        }
        make_executable(&new)?;
        std::fs::OpenOptions::new().write(true).open(&new)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("cannot sync {}: {e}", new.display()))
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    // A hard link costs nothing; where the file system has none (exFAT, FAT,
    // some FUSE) a copy. Without either the update still goes on: the new
    // file's hash has been checked.
    let kept = match std::fs::hard_link(live, &old).or_else(|_| std::fs::copy(live, &old).map(|_| ())) {
        Ok(()) => Some(old),
        Err(e) => {
            let _ = std::fs::remove_file(&old);
            crate::aelog!("[UPDATER] no copy of the old app kept ({}): a failed restart cannot be undone", e);
            None
        }
    };
    if let Err(e) = std::fs::rename(&new, live) {
        let _ = std::fs::remove_file(&new);
        if let Some(o) = &kept {
            let _ = std::fs::remove_file(o);
        }
        return Err(format!("cannot replace {}: {e} — nothing was changed", live.display()));
    }
    sync_dir(live.parent());
    Ok(kept)
}

/// Puts the old file back after the new one could not be started.
pub fn restore_file(old: &Path, live: &Path) -> Result<(), String> {
    std::fs::rename(old, live).map_err(|e| {
        format!("the old app is at {} — rename it to {} by hand ({e})", old.display(), live.display())
    })
}

/// Swaps the folder `new` into the place of `live`: `live` → `old`, `new` →
/// `live`, the first rename undone when the second fails. A stale `old` from
/// an earlier update is removed first, or moved aside ([`clear_path`]).
pub fn swap_dir(new: &Path, live: &Path, old: &Path) -> Result<(), String> {
    clear_path(old)?;
    std::fs::rename(live, old).map_err(|e| swap_refused(live, &e))?;
    if let Err(e) = std::fs::rename(new, live) {
        return Err(match std::fs::rename(old, live) {
            Ok(()) => format!("cannot put the new app in place ({e}) — nothing was changed"),
            Err(rb) => format!(
                "cannot put the new app in place ({e}) and cannot put the old one back ({rb}) — \
                 the old app is at {} — rename it to {} by hand",
                old.display(),
                live.display()
            ),
        });
    }
    Ok(())
}

/// Undoes [`swap_dir`] after the new app could not be started.
pub fn unswap_dir(new: &Path, live: &Path, old: &Path) -> String {
    match std::fs::rename(live, new).and_then(|_| std::fs::rename(old, live)) {
        Ok(()) => "rolled back".to_string(),
        Err(e) => format!("rollback failed ({e}) — the old app may be at {}", old.display()),
    }
}

/// The error for a bundle that cannot be moved. On macOS 13 and later the
/// system may refuse an app that changes apps (App Management) with EPERM.
fn swap_refused(live: &Path, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied && cfg!(target_os = "macos") {
        "macOS did not let Aura Engine replace itself — nothing was changed. Allow it in System Settings → \
         Privacy & Security → App Management, or use the download page"
            .to_string()
    } else {
        format!("cannot move {} aside: {e} — nothing was changed", live.display())
    }
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("cannot make {} executable: {e}", p.display()))
}

#[cfg(not(unix))]
fn make_executable(_p: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: Option<&Path>) {
    if let Some(d) = dir {
        let _ = std::fs::File::open(d).and_then(|f| f.sync_all());
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: Option<&Path>) {}

// ── macOS: the version inside the bundle ──────────────────────────────────

/// The `<string>` value right after `<key>key</key>` in an XML property list.
pub fn plist_string(xml: &str, key: &str) -> Option<String> {
    let at = xml.find(&format!("<key>{key}</key>"))?;
    let rest = &xml[at..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")?;
    // The next element must be the value, not another key.
    if rest[..start].matches("<key>").count() != 1 {
        return None;
    }
    Some(rest[start..start + end].trim().to_string())
}

/// `CFBundleShortVersionString` of a bundle.
pub fn bundle_version(bundle: &Path) -> Result<String, String> {
    bundle_key(bundle, "CFBundleShortVersionString")
}

/// A string of a bundle's Info.plist (read as XML, converted by `plutil`
/// when it is binary).
pub fn bundle_key(bundle: &Path, key: &str) -> Result<String, String> {
    let plist = bundle.join("Contents").join("Info.plist");
    let bytes = std::fs::read(&plist).map_err(|e| format!("cannot read {}: {e}", plist.display()))?;
    let xml = if bytes.starts_with(b"bplist") {
        let out = Command::new(PLUTIL)
            .args(["-convert", "xml1", "-o", "-"])
            .arg(&plist)
            .output()
            .map_err(|e| format!("cannot run plutil: {e}"))?;
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    plist_string(&xml, key).ok_or_else(|| format!("{} has no {key}", bundle.display()))
}

// ── system tools (absolute paths: no PATH lookup) ─────────────────────────

const DPKG: &str = "/usr/bin/dpkg";
const PKEXEC: &str = "/usr/bin/pkexec";
const SH: &str = "/bin/sh";
const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const DPKG_QUERY: &str = "/usr/bin/dpkg-query";
const HDIUTIL: &str = "/usr/bin/hdiutil";
const DITTO: &str = "/usr/bin/ditto";
const XATTR: &str = "/usr/bin/xattr";
const PLUTIL: &str = "/usr/bin/plutil";
const OPEN: &str = "/usr/bin/open";

/// What runs as root to install the .deb: `$1` the verified package (in the
/// user's data folder), `$2` its SHA-256, `$3` its package name. The user's
/// file could be swapped while the password is asked for, so root copies it
/// into a folder of its own and installs that copy only when its hash and
/// name are the verified ones. apt-get brings missing dependencies and waits
/// up to a minute for a busy package manager; dpkg where there is no apt-get.
/// Fixed text: nothing is put into it, the values come as arguments.
/// Exits: the package manager's own; 90 the package is not the verified one;
/// 91 it could not be copied; 92 no package manager could be started.
pub const DEB_INSTALL_SCRIPT: &str = r#"PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH
d=$(mktemp -d /tmp/aura-update.XXXXXXXX) || exit 91
if ! cp -- "$1" "$d/update.deb"; then rm -rf -- "$d"; exit 91; fi
got=$(sha256sum < "$d/update.deb") || got=
got=${got%% *}
if [ "$got" != "$2" ] || [ "$(dpkg-deb -f "$d/update.deb" Package 2>/dev/null)" != "$3" ]; then
  rm -rf -- "$d"; exit 90
fi
chmod 0755 "$d"; chmod 0644 "$d/update.deb"
if [ -x /usr/bin/apt-get ]; then
  apt-get install -y -o DPkg::Lock::Timeout=60 "$d/update.deb"
else
  dpkg -i "$d/update.deb"
fi
rc=$?
rm -rf -- "$d"
case $rc in 126|127) rc=92 ;; esac
exit $rc
"#;

/// The program and arguments that install the verified `deb` (SHA-256
/// `sha256_hex`, package `package`) as root: [`DEB_INSTALL_SCRIPT`] under
/// pkexec, every value a separate argument.
pub fn deb_command(deb: &Path, sha256_hex: &str, package: &str) -> (PathBuf, Vec<OsString>) {
    let mut args: Vec<OsString> = [SH, "-c", DEB_INSTALL_SCRIPT, "aura-update"].into_iter().map(OsString::from).collect();
    args.push(deb.as_os_str().to_owned());
    args.push(sha256_hex.into());
    args.push(package.into());
    (PathBuf::from(PKEXEC), args)
}

/// A Debian package name: lower-case letters, digits, `+ - .`, starting
/// with a letter or digit (so never an option).
pub fn is_package_name(s: &str) -> bool {
    s.len() >= 2
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
}

/// What pkexec's exit means. 126: the password dialog was dismissed; 127:
/// not authorised, or no authentication agent to ask with. 90–92: the
/// install script's own (see [`DEB_INSTALL_SCRIPT`]). Anything else is the
/// package manager's own.
pub fn pkexec_outcome(code: Option<i32>, stderr: &str) -> Result<(), String> {
    match code {
        Some(0) => Ok(()),
        Some(126) => Err("the password prompt was closed — nothing was changed".to_string()),
        Some(127) => Err(
            "no system password prompt is available — install the update from the download page".to_string(),
        ),
        Some(90) => Err("the package changed after it was checked — nothing was changed".to_string()),
        Some(91) => Err("the package could not be copied for installing — nothing was changed".to_string()),
        Some(92) => Err(
            "no package manager could be started — install the update from the download page".to_string(),
        ),
        other => {
            let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(3)..].join(" ");
            let code = other.map_or("a signal".to_string(), |c| c.to_string());
            Err(format!("the package manager stopped ({code}): {tail}"))
        }
    }
}

/// Installs the verified .deb, the system asking for the password. Blocks
/// until the user answers and the package manager is done.
fn install_deb(deb: &Path, sha256_hex: &str, package: &str) -> Result<(), String> {
    let (program, args) = deb_command(deb, sha256_hex, package);
    crate::aelog!("[UPDATER] {} {} (package {}, sha256 {})", program.display(), deb.display(), package, sha256_hex);
    let out = Command::new(&program)
        .args(&args)
        .output()
        .map_err(|e| format!("cannot run {}: {e} — install the update from the download page", program.display()))?;
    pkexec_outcome(out.status.code(), &String::from_utf8_lossy(&out.stderr))
}

/// What dpkg prints for `c`, or its complaint.
fn dpkg_text(c: &mut Command) -> Result<String, String> {
    let out = c.output().map_err(|e| format!("cannot ask dpkg: {e}"))?;
    if !out.status.success() {
        return Err(format!("dpkg says: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The package name of the verified `deb`, read before the password is
/// asked for; root installs the file only when its copy has this name too.
fn deb_package(deb: &Path) -> Result<String, String> {
    let pkg = dpkg_text(Command::new(DPKG_DEB).arg("-f").arg(deb).arg("Package"))?;
    if !is_package_name(&pkg) {
        return Err(format!("the package has an odd name ({pkg}) — install the update from the download page"));
    }
    Ok(pkg)
}

/// The version dpkg now has for `package`.
fn deb_installed_version(package: &str) -> Result<String, String> {
    dpkg_text(Command::new(DPKG_QUERY).args(["-W", "-f=${Version}"]).arg(package))
}

/// dpkg's version is the release's (`1.9.2`, or with a Debian revision, `1.9.2-1`).
pub fn deb_version_is(installed: &str, version: &str) -> bool {
    installed == version || installed.strip_prefix(version).map_or(false, |r| r.starts_with('-'))
}

/// The disk image mounted at a folder of ours; detached when dropped, on
/// every path out.
struct Mounted(PathBuf);

impl Mounted {
    fn attach(dmg: &Path, at: &Path) -> Result<Mounted, String> {
        std::fs::create_dir_all(at).map_err(|e| format!("cannot create {}: {e}", at.display()))?;
        let out = Command::new(HDIUTIL)
            .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
            .arg(at)
            .arg(dmg)
            .output()
            .map_err(|e| format!("cannot run hdiutil: {e}"))?;
        if !out.status.success() {
            let _ = std::fs::remove_dir(at);
            return Err(format!("cannot open the disk image: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(Mounted(at.to_path_buf()))
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let detach = |force: bool| {
            let mut c = Command::new(HDIUTIL);
            c.arg("detach").arg(&self.0).arg("-quiet");
            if force {
                c.arg("-force");
            }
            c.status().map_or(false, |s| s.success())
        };
        if !detach(false) && !detach(true) {
            crate::aelog!("[UPDATER] could not detach {}", self.0.display());
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// The one `.app` folder directly in `dir` (links and anything else ignored).
pub fn single_app_in(dir: &Path) -> Result<PathBuf, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("cannot read the disk image: {e}"))?;
    let apps: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map_or(false, |t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "app"))
        .collect();
    match apps.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err("the disk image has no app".to_string()),
        _ => Err("the disk image has more than one app".to_string()),
    }
}

/// A folder only this user can open, removed when dropped.
struct Private(PathBuf);

impl Private {
    fn make(at: &Path) -> Result<Private, String> {
        clear_path(at)?;
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        #[cfg(unix)]
        let made = {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(at)
        };
        #[cfg(not(unix))]
        let made = std::fs::create_dir(at);
        made.map_err(|e| format!("cannot create {}: {e}", at.display()))?;
        Ok(Private(at.to_path_buf()))
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A copy of the verified `file` in `dir`, its hash checked again against
/// `sha256_hex`: what is read next is what the signature covers, whatever
/// happens to the download meanwhile.
pub fn checked_copy(file: &Path, dir: &Path, sha256_hex: &str) -> Result<PathBuf, String> {
    let name = file.file_name().ok_or("the download has no name")?;
    let copy = dir.join(name);
    std::fs::copy(file, &copy).map_err(|e| format!("cannot write {}: {e}", copy.display()))?;
    if crate::updater::sha256_file(&copy)? != sha256_hex {
        let _ = std::fs::remove_file(&copy);
        return Err("the download changed after it was checked — nothing was changed".to_string());
    }
    Ok(copy)
}

/// Mounts the verified `dmg` (SHA-256 `sha256_hex`), copies its app beside
/// `bundle`, checks it is `version`, and swaps it into place. Returns the old
/// bundle's new path (`.<Name>.app.old`), for [`unswap_dir`] if the new one
/// cannot be started.
pub fn install_mac_app(
    dmg: &Path,
    sha256_hex: &str,
    work: &Path,
    bundle: &Path,
    version: &str,
) -> Result<PathBuf, String> {
    let new = sibling(bundle, "new").ok_or("the app's path has no folder")?;
    let old = sibling(bundle, "old").ok_or("the app's path has no folder")?;
    clear_path(&new)?;
    let staged = (|| -> Result<(), String> {
        // The image that is mounted is a copy of ours, checked right before.
        let private = Private::make(&work.join(format!("img-{}", std::process::id())))?;
        let image = checked_copy(dmg, &private.0, sha256_hex)?;
        let mnt = Mounted::attach(&image, &work.join(format!("mnt-{}", std::process::id())))?;
        let app = single_app_in(&mnt.0)?;
        let out = Command::new(DITTO)
            .arg(&app)
            .arg(&new)
            .output()
            .map_err(|e| format!("cannot run ditto: {e}"))?;
        if !out.status.success() {
            return Err(format!("cannot copy the new app: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        drop(mnt);
        // The same app (its identifier), and the version released.
        let ours = bundle_key(bundle, "CFBundleIdentifier")?;
        let theirs = bundle_key(&new, "CFBundleIdentifier")?;
        if theirs != ours {
            return Err(format!("the disk image holds {theirs}, not {ours}"));
        }
        let got = bundle_version(&new)?;
        if got != version {
            return Err(format!("the disk image holds {got}, not {version}"));
        }
        // Not downloaded by a browser, so not quarantined; cleared all the
        // same, so the new app never meets a Gatekeeper prompt.
        let _ = Command::new(XATTR).args(["-dr", "com.apple.quarantine"]).arg(&new).output();
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&new);
        return Err(e);
    }
    if let Err(e) = swap_dir(&new, bundle, &old) {
        let _ = std::fs::remove_dir_all(&new);
        return Err(e);
    }
    Ok(old)
}

// ── restart ───────────────────────────────────────────────────────────────

/// Variables an AppImage's start script sets for itself; the new AppImage
/// sets its own.
const APPIMAGE_VARS: &[&str] = &["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

/// The environment changes that keep the old AppImage's mount (`appdir`,
/// gone once the old app exits) out of the new one's: its own variables
/// removed, and every path list cleared of the entries inside the mount.
/// `None` removes the variable.
pub fn scrub_appimage_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    appdir: &str,
) -> Vec<(OsString, Option<OsString>)> {
    let mut out = Vec::new();
    if appdir.len() < 2 {
        return out;
    }
    for (k, v) in vars {
        if APPIMAGE_VARS.iter().any(|a| k == *a) {
            out.push((k, None));
            continue;
        }
        let Some(s) = v.to_str() else { continue };
        if !s.contains(appdir) {
            continue;
        }
        let inside = |p: &str| p == appdir || p.strip_prefix(appdir).map_or(false, |r| r.starts_with('/'));
        let kept: Vec<&str> = s.split(':').filter(|p| !inside(p)).collect();
        out.push((k, (!kept.is_empty() && kept.iter().any(|p| !p.is_empty())).then(|| OsString::from(kept.join(":")))));
    }
    out
}

/// Marks every open descriptor from `min_fd` up close-on-exec. The AppImage
/// runtime hands the app the write end of a pipe its FUSE server watches;
/// a new app that inherits it keeps the old mount and its server alive.
#[cfg(target_os = "linux")]
pub fn cloexec_from(min_fd: i32) {
    let Ok(rd) = std::fs::read_dir("/proc/self/fd") else { return };
    let fds: Vec<i32> = rd.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).filter(|&fd| fd >= min_fd).collect();
    for fd in fds {
        // SAFETY: fcntl on a descriptor number; a closed one only fails.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Starts the AppImage at `file` with the old mount left out of its
/// environment and descriptors.
fn spawn_appimage(file: &Path, old_pid: u32) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    cloexec_from(3);
    let mut c = Command::new(file);
    c.arg("--after-update").arg(old_pid.to_string());
    if let Some(appdir) = std::env::var("APPDIR").ok() {
        for (k, v) in scrub_appimage_env(std::env::vars_os(), appdir.trim_end_matches('/')) {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
    }
    c.spawn().map(|_| ()).map_err(|e| format!("failed to start the new app: {e}"))
}

/// Starts a bundle through LaunchServices: a new instance (`-n`) that the
/// Dock and the window server treat as the app itself, read from the new
/// bundle's Info.plist, not a child of the old process.
fn open_bundle(bundle: &Path, old_pid: u32) -> Result<(), String> {
    let st = Command::new(OPEN)
        .arg("-n")
        .arg(bundle)
        .args(["--args", "--after-update"])
        .arg(old_pid.to_string())
        .status()
        .map_err(|e| format!("failed to start the new app: {e}"))?;
    if st.success() { Ok(()) } else { Err(format!("failed to start the new app: open exited with {st}")) }
}

fn spawn_exe(exe: &Path, old_pid: u32) -> Result<(), String> {
    Command::new(exe)
        .arg("--after-update")
        .arg(old_pid.to_string())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to start the new app: {e}"))
}

/// Restarts this copy the way it was started (an interface update: the app
/// itself is the same). `exe` is the running exe, read before anything moved.
/// An AppImage restarts as the AppImage whatever its kind (one in a folder it
/// cannot write is a page kind): its exe lives in the mount that goes away
/// with this process.
pub fn relaunch(kind: &InstallKind, exe: &Path, old_pid: u32) -> Result<(), String> {
    match kind {
        InstallKind::AppImage(file) => spawn_appimage(file, old_pid),
        InstallKind::MacApp(bundle) => open_bundle(bundle, old_pid),
        _ => match relaunch_by(exe, running_appimage(exe), cfg!(target_os = "macos")) {
            Relaunch::AppImage(file) => spawn_appimage(&file, old_pid),
            Relaunch::Bundle(bundle) => open_bundle(&bundle, old_pid),
            Relaunch::Exe => spawn_exe(exe, old_pid),
        },
    }
}

/// How a copy that does not install itself is started again.
#[derive(Debug, PartialEq)]
pub enum Relaunch {
    AppImage(PathBuf),
    Bundle(PathBuf),
    Exe,
}

/// The way to restart `exe`: the AppImage it runs from, its bundle on macOS,
/// or the exe itself.
pub fn relaunch_by(exe: &Path, appimage: Option<PathBuf>, macos: bool) -> Relaunch {
    if let Some(file) = appimage {
        return Relaunch::AppImage(file);
    }
    match mac_bundle_of(exe).filter(|_| macos) {
        Some(bundle) => Relaunch::Bundle(bundle),
        None => Relaunch::Exe,
    }
}

// ── install, by kind ──────────────────────────────────────────────────────

/// Places the verified download `file` (SHA-256 `sha256_hex`, release
/// `version`) for `kind` and starts the new app; a new app that cannot be
/// started is rolled back where that is possible. `work` is the download's
/// folder; `exe` the running exe, read before anything moved.
pub fn install(
    kind: &InstallKind,
    file: &Path,
    sha256_hex: &str,
    version: &str,
    work: &Path,
    exe: &Path,
    old_pid: u32,
) -> Result<(), String> {
    match kind {
        InstallKind::AppImage(live) => {
            let old = place_file(file, live, sha256_hex)?;
            crate::aelog!("[UPDATER] {} replaced; restarting", live.display());
            if let Err(e) = spawn_appimage(live, old_pid) {
                let rb = match &old {
                    Some(o) => restore_file(o, live).map_or_else(|m| m, |_| "rolled back".to_string()),
                    None => "no copy of the old app to roll back to".to_string(),
                };
                return Err(format!("{e} ({rb})"));
            }
        }
        InstallKind::Deb => {
            let package = deb_package(file)?;
            install_deb(file, sha256_hex, &package)?;
            // Success only when dpkg has the version released.
            let got = deb_installed_version(&package)?;
            if !deb_version_is(&got, version) {
                return Err(format!(
                    "the package manager left version {got}, not {version} — install the update from the download page"
                ));
            }
            crate::aelog!("[UPDATER] the package {} is installed; restarting", got);
            spawn_exe(exe, old_pid)
                .map_err(|e| format!("the update is installed — start Aura Engine again ({e})"))?;
        }
        InstallKind::MacApp(bundle) => {
            let old = install_mac_app(file, sha256_hex, work, bundle, version)?;
            crate::aelog!("[UPDATER] {} replaced; restarting", bundle.display());
            if let Err(e) = open_bundle(bundle, old_pid) {
                let new = sibling(bundle, "new").unwrap_or_default();
                return Err(format!("{e} ({})", unswap_dir(&new, bundle, &old)));
            }
        }
        InstallKind::WindowsZip | InstallKind::Page(_) => {
            return Err("this copy is updated from the download page".to_string());
        }
    }
    // The download is in place now; the copy in the data folder is not needed.
    let _ = std::fs::remove_file(file);
    Ok(())
}

/// After a restart into an update: removes what the old app was kept as.
pub fn clean_after_update(kind: &InstallKind) {
    let target = match kind {
        InstallKind::AppImage(file) => file,
        InstallKind::MacApp(bundle) => bundle,
        _ => return,
    };
    let mut leftovers: Vec<PathBuf> = ["old", "new"].iter().filter_map(|w| sibling(target, w)).collect();
    // Copies moved aside by earlier updates (`.<name>.old-<stamp>`).
    if let (Some(dir), Some(name)) = (target.parent(), target.file_name()) {
        let prefix = format!(".{}.old-", name.to_string_lossy());
        if let Ok(rd) = std::fs::read_dir(dir) {
            leftovers.extend(rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).map(|e| e.path()));
        }
    }
    for p in leftovers {
        let r = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        match r {
            Ok(()) => crate::aelog!("[UPDATER] removed {}", p.display()),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                crate::aelog!("[UPDATER] could not remove {}: {}", p.display(), e)
            }
            Err(_) => {}
        }
    }
}

/// Waits until the process `pid` has exited, or `timeout_ms` has passed.
#[cfg(unix)]
pub fn wait_for_pid(pid: u32, timeout_ms: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    let Ok(pid) = libc::pid_t::try_from(pid) else { return };
    while std::time::Instant::now() < deadline {
        // 0: it is there; EPERM: there, someone else's; ESRCH: gone.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn each_layout_is_its_kind() {
        assert_eq!(detect("windows", p("C:/Aura/aura-engine.exe"), None, None, false), InstallKind::WindowsZip);
        // AppImage: the exe runs from its mount ($APPDIR)
        let mount = Some(p("/tmp/.mount_AuraXy"));
        assert_eq!(
            detect("linux", p("/tmp/.mount_AuraXy/usr/bin/aura-engine"), Some(p("/home/u/Apps/Aura.AppImage")), mount, true),
            InstallKind::AppImage(PathBuf::from("/home/u/Apps/Aura.AppImage"))
        );
        // a relative $APPIMAGE is not trusted
        assert_eq!(
            detect("linux", p("/tmp/.mount_AuraXy/usr/bin/aura-engine"), Some(p("Aura.AppImage")), mount, false),
            InstallKind::Page(PageReason::Other)
        );
        assert_eq!(detect("linux", p("/usr/bin/aura-engine"), None, None, true), InstallKind::Deb);
        // under /usr without dpkg (another distribution's package), /usr/local, a tarball
        assert_eq!(detect("linux", p("/usr/bin/aura-engine"), None, None, false), InstallKind::Page(PageReason::Other));
        assert_eq!(detect("linux", p("/usr/local/bin/aura-engine"), None, None, true), InstallKind::Page(PageReason::Other));
        assert_eq!(detect("linux", p("/home/u/aura/aura-engine"), None, None, true), InstallKind::Page(PageReason::Other));
        assert_eq!(
            detect("macos", p("/Applications/Aura Engine.app/Contents/MacOS/aura-engine"), None, None, false),
            InstallKind::MacApp(PathBuf::from("/Applications/Aura Engine.app"))
        );
        assert_eq!(
            detect("macos", p("/Users/u/Apps/Mine.app/Contents/MacOS/aura-engine"), None, None, false),
            InstallKind::MacApp(PathBuf::from("/Users/u/Apps/Mine.app"))
        );
        assert_eq!(
            detect(
                "macos",
                p("/private/var/folders/x/T/AppTranslocation/1234/d/Aura Engine.app/Contents/MacOS/aura-engine"),
                None,
                None,
                false
            ),
            InstallKind::Page(PageReason::Translocated)
        );
        // a development build, not in a bundle
        assert_eq!(
            detect("macos", p("/Users/u/src/target/fast/aura-engine"), None, None, false),
            InstallKind::Page(PageReason::Other)
        );
        assert_eq!(detect("freebsd", p("/usr/bin/aura-engine"), None, None, true), InstallKind::Page(PageReason::Other));
    }

    #[test]
    fn an_appimage_seen_only_in_the_environment_is_not_this_copy() {
        let other = Some(p("/home/u/Apps/Other.AppImage"));
        // the .deb's exe started from a terminal that runs inside another AppImage
        assert_eq!(detect("linux", p("/usr/bin/aura-engine"), other, Some(p("/tmp/.mount_OtherAb")), true), InstallKind::Deb);
        // a tarball the same way
        assert_eq!(
            detect("linux", p("/home/u/aura/aura-engine"), other, Some(p("/tmp/.mount_OtherAb")), true),
            InstallKind::Page(PageReason::Other)
        );
        // no $APPDIR, a relative one, or the root: not trusted
        assert_eq!(appimage_of(p("/tmp/.mount_AuraXy/usr/bin/aura-engine"), other, None), None);
        assert_eq!(appimage_of(p("/tmp/.mount_AuraXy/usr/bin/aura-engine"), other, Some(p(".mount_AuraXy"))), None);
        assert_eq!(appimage_of(p("/usr/bin/aura-engine"), other, Some(p("/"))), None);
        // a mount whose name only begins the same
        assert_eq!(appimage_of(p("/tmp/.mount_AuraXyz/usr/bin/aura-engine"), other, Some(p("/tmp/.mount_AuraXy"))), None);
    }

    #[test]
    fn an_appimage_restarts_as_the_appimage_whatever_its_kind() {
        let exe = p("/tmp/.mount_AuraXy/usr/bin/aura-engine");
        let file = PathBuf::from("/opt/ro/Aura.AppImage");
        assert_eq!(relaunch_by(exe, Some(file.clone()), false), Relaunch::AppImage(file));
        assert_eq!(relaunch_by(p("/usr/bin/aura-engine"), None, false), Relaunch::Exe);
        let mac = p("/Applications/Aura Engine.app/Contents/MacOS/aura-engine");
        assert_eq!(relaunch_by(mac, None, true), Relaunch::Bundle(PathBuf::from("/Applications/Aura Engine.app")));
        assert_eq!(relaunch_by(mac, None, false), Relaunch::Exe);
    }

    #[test]
    fn a_bundle_on_the_disk_image_is_to_be_moved_first() {
        assert_eq!(unwritable_reason(p("/Volumes/Aura Engine")), PageReason::OnDiskImage);
        assert_eq!(unwritable_reason(p("/Applications")), PageReason::NotWritable(PathBuf::from("/Applications")));
        let k = InstallKind::Page(PageReason::OnDiskImage);
        assert!(!k.can_install());
        assert_eq!(k.tag(), "page");
        assert!(k.note().unwrap().contains("Applications first"));
        assert!(InstallKind::Page(PageReason::NotWritable(PathBuf::from("/opt/x"))).note().unwrap().contains("/opt/x"));
        for k in [InstallKind::WindowsZip, InstallKind::Deb, InstallKind::AppImage("/a".into()), InstallKind::MacApp("/b.app".into())] {
            assert!(k.can_install() && k.note().is_none(), "{k:?}");
        }
    }

    #[test]
    fn an_unwritable_folder_turns_the_kind_into_the_page() {
        let d = TempDir::new().unwrap();
        let file = d.path().join("Aura.AppImage");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(usable(InstallKind::AppImage(file.clone())), InstallKind::AppImage(file));
        let gone = d.path().join("no-such-dir").join("Aura.AppImage");
        assert_eq!(
            usable(InstallKind::AppImage(gone)),
            InstallKind::Page(PageReason::NotWritable(d.path().join("no-such-dir")))
        );
        assert!(dir_writable(d.path()));
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "the probe is removed");
    }

    #[test]
    fn the_appimage_is_replaced_in_place_and_can_be_put_back() {
        let d = TempDir::new().unwrap();
        let live = d.path().join("My Aura.AppImage");
        let dl = d.path().join("download.AppImage");
        std::fs::write(&live, b"old app").unwrap();
        std::fs::write(&dl, b"new app").unwrap();
        let sha = crate::updater::sha256_file(&dl).unwrap();
        let old = place_file(&dl, &live, &sha).unwrap().expect("a hard link of the old one");
        assert_eq!(std::fs::read(&live).unwrap(), b"new app");
        assert_eq!(old, d.path().join(".My Aura.AppImage.old"));
        assert_eq!(std::fs::read(&old).unwrap(), b"old app");
        assert!(!d.path().join(".My Aura.AppImage.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&live).unwrap().permissions().mode() & 0o777, 0o755);
        }
        // the new one did not start: the old one is back, under the user's name
        restore_file(&old, &live).unwrap();
        assert_eq!(std::fs::read(&live).unwrap(), b"old app");
        assert!(!old.exists());
        // after a good start the old one is cleaned
        let old = place_file(&dl, &live, &sha).unwrap().unwrap();
        clean_after_update(&InstallKind::AppImage(live.clone()));
        assert!(!old.exists());
        assert_eq!(std::fs::read(&live).unwrap(), b"new app");
    }

    #[test]
    fn a_copy_that_does_not_match_the_verified_hash_changes_nothing() {
        let d = TempDir::new().unwrap();
        let live = d.path().join("Aura.AppImage");
        let dl = d.path().join("dl");
        std::fs::write(&live, b"old app").unwrap();
        std::fs::write(&dl, b"tampered").unwrap();
        let e = place_file(&dl, &live, &"0".repeat(64)).unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert_eq!(std::fs::read(&live).unwrap(), b"old app");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2, "no .new or .old left");
    }

    fn fake_app(at: &Path, version: &str, body: &[u8]) {
        let macos = at.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("aura-engine"), body).unwrap();
        std::fs::write(
            at.join("Contents").join("Info.plist"),
            format!(
                "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict>\n<key>CFBundleName</key>\n<string>Aura Engine</string>\n\
                 <key>CFBundleIdentifier</key>\n<string>com.aura.engine</string>\n\
                 <key>CFBundleShortVersionString</key>\n<string>{version}</string>\n</dict></plist>\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn the_bundle_is_swapped_and_rolled_back() {
        let d = TempDir::new().unwrap();
        let live = d.path().join("Aura Engine.app");
        let new = sibling(&live, "new").unwrap();
        let old = sibling(&live, "old").unwrap();
        assert_eq!(new, d.path().join(".Aura Engine.app.new"));
        fake_app(&live, "1.9.1", b"old");
        fake_app(&new, "1.9.2", b"new");
        // a stale .old from an interrupted update does not stop it
        fake_app(&old, "1.8.0", b"stale");
        swap_dir(&new, &live, &old).unwrap();
        assert_eq!(bundle_version(&live).unwrap(), "1.9.2");
        assert_eq!(bundle_version(&old).unwrap(), "1.9.1");
        assert_eq!(bundle_key(&live, "CFBundleIdentifier").unwrap(), "com.aura.engine");
        assert!(!new.exists());
        // the new one did not start
        assert_eq!(unswap_dir(&new, &live, &old), "rolled back");
        assert_eq!(bundle_version(&live).unwrap(), "1.9.1");
        assert_eq!(bundle_version(&new).unwrap(), "1.9.2");
        // a failed second rename puts the first back
        std::fs::remove_dir_all(&new).unwrap();
        let e = swap_dir(&new, &live, &old).unwrap_err();
        assert!(e.contains("nothing was changed"), "{e}");
        assert_eq!(bundle_version(&live).unwrap(), "1.9.1");
        assert!(!old.exists());
        // cleaned after a good start
        fake_app(&new, "1.9.2", b"new");
        swap_dir(&new, &live, &old).unwrap();
        clean_after_update(&InstallKind::MacApp(live.clone()));
        assert!(!old.exists() && !new.exists());
        assert_eq!(bundle_version(&live).unwrap(), "1.9.2");
    }

    #[test]
    fn the_version_is_read_from_the_plist() {
        let xml = "<dict><key>CFBundleVersion</key><string>7</string>\
                   <key>CFBundleShortVersionString</key>\n\t<string> 1.9.2 </string></dict>";
        assert_eq!(plist_string(xml, "CFBundleShortVersionString").as_deref(), Some("1.9.2"));
        assert_eq!(plist_string(xml, "CFBundleVersion").as_deref(), Some("7"));
        assert_eq!(plist_string(xml, "Missing"), None);
        // a key whose value is not a string does not take the next key's
        let xml = "<key>LSUIElement</key><true/><key>X</key><string>y</string>";
        assert_eq!(plist_string(xml, "LSUIElement"), None);
    }

    #[test]
    fn only_one_app_on_the_image_is_taken() {
        let d = TempDir::new().unwrap();
        assert!(single_app_in(d.path()).unwrap_err().contains("no app"));
        std::fs::write(d.path().join("Fake.app"), b"a file").unwrap();
        std::fs::create_dir(d.path().join("Applications")).unwrap();
        assert!(single_app_in(d.path()).is_err());
        fake_app(&d.path().join("Aura Engine.app"), "1.9.2", b"x");
        assert_eq!(single_app_in(d.path()).unwrap(), d.path().join("Aura Engine.app"));
        fake_app(&d.path().join("Other.app"), "1", b"x");
        assert!(single_app_in(d.path()).unwrap_err().contains("more than one"));
    }

    #[test]
    fn the_deb_is_installed_by_a_fixed_script_with_the_values_apart() {
        let deb = p("/home/u/.local/share/AuraEngine/updates/v1.9.2/aura engine; rm -rf ~.deb");
        let sha = "ab".repeat(32);
        let (prog, args) = deb_command(deb, &sha, "aura-engine");
        assert_eq!(prog, PathBuf::from("/usr/bin/pkexec"));
        assert_eq!(args[..4], ["/bin/sh", "-c", DEB_INSTALL_SCRIPT, "aura-update"].map(OsString::from));
        assert_eq!(args[4], deb.as_os_str(), "the path is one argument, as it is");
        assert_eq!(args[5], OsString::from(&sha));
        assert_eq!(args[6], "aura-engine");
        assert_eq!(args.len(), 7);
        // root installs its own copy, only after the hash and name are checked
        let s = DEB_INSTALL_SCRIPT;
        let at = |needle: &str| s.find(needle).unwrap_or_else(|| panic!("{needle} in the script"));
        assert!(at("mktemp -d") < at("cp -- \"$1\""));
        assert!(at("cp -- \"$1\"") < at("sha256sum"));
        assert!(at("[ \"$got\" != \"$2\" ]") < at("apt-get install"));
        assert!(at("Package") < at("apt-get install"));
        assert!(at("exit 90") < at("apt-get install"));
        assert!(s.contains("apt-get install -y -o DPkg::Lock::Timeout=60 \"$d/update.deb\""));
        assert!(s.contains("dpkg -i \"$d/update.deb\""));
        assert_eq!(s.matches("$1").count(), 1, "the user's file is only copied, never installed");
        assert!(s.starts_with("PATH=/usr/sbin:/usr/bin:/sbin:/bin\n"));
        assert!(s.trim_end().ends_with("exit $rc"));
    }

    #[test]
    fn a_package_name_is_never_an_option() {
        assert!(is_package_name("aura-engine"));
        assert!(is_package_name("aura-engine2.0+x"));
        for bad in ["", "a", "-x", "Aura", "aura engine", "aura;rm", "--force"] {
            assert!(!is_package_name(bad), "{bad}");
        }
    }

    /// The script on its own, as a user: a package whose hash is not the
    /// verified one is never handed to the package manager, and nothing of
    /// it stays in /tmp.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_install_script_stops_on_a_package_that_is_not_the_verified_one() {
        let d = TempDir::new().unwrap();
        let deb = d.path().join("update.deb");
        std::fs::write(&deb, b"not really a package").unwrap();
        let sha = crate::updater::sha256_file(&deb).unwrap();
        let leftovers = || {
            std::fs::read_dir("/tmp").unwrap().flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("aura-update."))
                .count()
        };
        let before = leftovers();
        let run = |sha: &str| {
            Command::new(SH).args(["-c", DEB_INSTALL_SCRIPT, "aura-update"]).arg(&deb).arg(sha).arg("aura-engine")
                .output().unwrap().status.code()
        };
        // another hash
        assert_eq!(run(&"0".repeat(64)), Some(90));
        // the right hash, but not the package named (not a package at all)
        assert_eq!(run(&sha), Some(90));
        // a file that is not there
        std::fs::remove_file(&deb).unwrap();
        assert_eq!(run(&sha), Some(91));
        assert_eq!(leftovers(), before, "the copy is removed");
    }

    #[test]
    fn the_installed_package_is_the_release() {
        assert!(deb_version_is("1.9.2", "1.9.2"));
        assert!(deb_version_is("1.9.2-1", "1.9.2"));
        assert!(!deb_version_is("1.9.1", "1.9.2"));
        assert!(!deb_version_is("1.9.20", "1.9.2"));
        assert!(!deb_version_is("", "1.9.2"));
    }

    #[test]
    fn pkexec_exits_are_told_apart() {
        assert!(pkexec_outcome(Some(0), "").is_ok());
        assert!(pkexec_outcome(Some(126), "").unwrap_err().contains("closed"));
        assert!(pkexec_outcome(Some(127), "Error: No authentication agent found.").unwrap_err().contains("download page"));
        let e = pkexec_outcome(Some(100), "a\n\nE: Could not get lock\nE: Unable to lock\n").unwrap_err();
        assert!(e.contains("(100)") && e.contains("Could not get lock") && e.contains("Unable to lock"), "{e}");
        assert!(pkexec_outcome(None, "").unwrap_err().contains("a signal"));
        // the install script's own
        assert!(pkexec_outcome(Some(90), "").unwrap_err().contains("nothing was changed"));
        assert!(pkexec_outcome(Some(91), "").unwrap_err().contains("nothing was changed"));
        assert!(pkexec_outcome(Some(92), "").unwrap_err().contains("download page"));
    }

    #[test]
    fn the_image_read_is_a_copy_checked_again() {
        let d = TempDir::new().unwrap();
        let dl = d.path().join("aura.dmg");
        std::fs::write(&dl, b"image").unwrap();
        let sha = crate::updater::sha256_file(&dl).unwrap();
        let private = Private::make(&d.path().join("img")).unwrap();
        let copy = checked_copy(&dl, &private.0, &sha).unwrap();
        assert_eq!(copy, d.path().join("img").join("aura.dmg"));
        assert_eq!(std::fs::read(&copy).unwrap(), b"image");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&private.0).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // the download changed after the check
        std::fs::write(&dl, b"other").unwrap();
        let e = checked_copy(&dl, &private.0, &sha).unwrap_err();
        assert!(e.contains("changed after it was checked"), "{e}");
        assert!(!copy.exists(), "the bad copy is not left");
        drop(private);
        assert!(!d.path().join("img").exists(), "the folder goes with it");
    }

    #[test]
    fn a_leftover_is_removed_or_moved_aside() {
        let d = TempDir::new().unwrap();
        clear_path(&d.path().join("none")).unwrap();
        let f = d.path().join(".Aura.AppImage.old");
        std::fs::write(&f, b"x").unwrap();
        clear_path(&f).unwrap();
        assert!(!f.exists());
        let aside = aside_name(&f).unwrap();
        assert!(aside.file_name().unwrap().to_string_lossy().starts_with(".Aura.AppImage.old-"));
        std::fs::write(&aside, b"taken").unwrap();
        assert_ne!(aside_name(&f).unwrap(), aside, "a free name");
    }

    /// An old bundle with a folder this user cannot empty (root's, after an
    /// installer package) is moved aside, and the update goes on.
    #[cfg(unix)]
    #[test]
    fn a_stale_old_bundle_that_cannot_be_removed_does_not_stop_the_update() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new().unwrap();
        let live = d.path().join("Aura Engine.app");
        let new = sibling(&live, "new").unwrap();
        let old = sibling(&live, "old").unwrap();
        fake_app(&live, "1.9.1", b"old");
        fake_app(&new, "1.9.2", b"new");
        fake_app(&old, "1.8.0", b"stale");
        let locked = old.join("Contents").join("MacOS");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        swap_dir(&new, &live, &old).unwrap();
        assert_eq!(bundle_version(&live).unwrap(), "1.9.2");
        assert_eq!(bundle_version(&old).unwrap(), "1.9.1");
        let aside: Vec<PathBuf> = std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(".Aura Engine.app.old-"))
            .collect();
        // root removes anything; a user cannot
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(aside.len(), 1, "moved aside");
            let still = aside[0].join("Contents").join("MacOS");
            std::fs::set_permissions(&still, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // after a good start the old one and the ones moved aside are cleaned
        clean_after_update(&InstallKind::MacApp(live.clone()));
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "only the app is left");
    }

    #[test]
    fn the_old_mount_is_kept_out_of_the_new_appimage() {
        let vars = [
            ("APPDIR", "/tmp/.mount_AuraAb"),
            ("APPIMAGE", "/home/u/Aura.AppImage"),
            ("ARGV0", "./Aura.AppImage"),
            ("OWD", "/home/u"),
            ("PATH", "/tmp/.mount_AuraAb/usr/bin:/usr/bin:/bin"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_AuraAb/usr/lib"),
            ("GDK_PIXBUF_MODULE_FILE", "/tmp/.mount_AuraAb/usr/lib/loaders.cache"),
            ("HOME", "/home/u"),
            ("XDG_DATA_DIRS", "/tmp/.mount_AuraAb/usr/share:/usr/share"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let got = scrub_appimage_env(vars, "/tmp/.mount_AuraAb");
        let get = |k: &str| got.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        for k in ["APPDIR", "APPIMAGE", "ARGV0", "OWD", "LD_LIBRARY_PATH", "GDK_PIXBUF_MODULE_FILE"] {
            assert_eq!(get(k), Some(None), "{k} removed");
        }
        assert_eq!(get("PATH"), Some(Some(OsString::from("/usr/bin:/bin"))));
        assert_eq!(get("XDG_DATA_DIRS"), Some(Some(OsString::from("/usr/share"))));
        assert_eq!(get("HOME"), None, "untouched");
        assert!(scrub_appimage_env([(OsString::from("PATH"), OsString::from("/x"))], "/").is_empty());
    }

    /// The real thing on a Mac: a disk image made with hdiutil from a fake
    /// app, mounted, copied, checked and swapped into a temporary
    /// "Applications"; the mount is gone afterwards, also when it fails.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_disk_image_is_installed_over_the_bundle() {
        let d = TempDir::new().unwrap();
        let src = d.path().join("src");
        fake_app(&src.join("Aura Engine.app"), "1.9.2", b"new");
        std::os::unix::fs::symlink("/Applications", src.join("Applications")).unwrap();
        let dmg = d.path().join("aura.dmg");
        let st = Command::new(HDIUTIL)
            .args(["create", "-quiet", "-fs", "HFS+", "-volname", "Aura Engine", "-format", "UDZO", "-srcfolder"])
            .arg(&src)
            .arg(&dmg)
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create");
        let apps = d.path().join("Applications");
        let live = apps.join("Aura Engine.app");
        fake_app(&live, "1.9.1", b"old");
        let work = d.path().join("work");
        let sha = crate::updater::sha256_file(&dmg).unwrap();
        // not the verified image: nothing is mounted
        let e = install_mac_app(&dmg, &"0".repeat(64), &work, &live, "1.9.2").unwrap_err();
        assert!(e.contains("changed after it was checked"), "{e}");
        assert_eq!(bundle_version(&live).unwrap(), "1.9.1");
        assert_eq!(std::fs::read_dir(&work).map(|r| r.count()).unwrap_or(0), 0, "nothing left in work");
        // the wrong version: nothing changes, nothing stays mounted
        let e = install_mac_app(&dmg, &sha, &work, &live, "1.9.3").unwrap_err();
        assert!(e.contains("1.9.2"), "{e}");
        assert_eq!(bundle_version(&live).unwrap(), "1.9.1");
        assert!(!sibling(&live, "new").unwrap().exists());
        assert_eq!(std::fs::read_dir(&work).map(|r| r.count()).unwrap_or(0), 0, "unmounted and removed");
        let old = install_mac_app(&dmg, &sha, &work, &live, "1.9.2").unwrap();
        assert_eq!(bundle_version(&live).unwrap(), "1.9.2");
        assert_eq!(std::fs::read(live.join("Contents/MacOS/aura-engine")).unwrap(), b"new");
        assert_eq!(bundle_version(&old).unwrap(), "1.9.1");
        assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0);
        clean_after_update(&InstallKind::MacApp(live.clone()));
        assert!(!old.exists());
    }

    /// The real thing on Linux: an executable file swapped while it runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_running_appimage_is_replaced_under_it() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new().unwrap();
        let live = d.path().join("Aura.AppImage");
        // `read` at the end of input fails, hence the `exit 0`.
        std::fs::write(&live, b"#!/bin/sh\necho up\nread x\nexit 0\n").unwrap();
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut running = Command::new(&live)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The shell opens the script by its name: once it speaks, it has the old one open.
        let mut up = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(running.stdout.take().unwrap()), &mut up).unwrap();
        assert_eq!(up, "up\n");
        let dl = d.path().join("dl");
        std::fs::write(&dl, b"#!/bin/sh\nexit 7\n").unwrap();
        let sha = crate::updater::sha256_file(&dl).unwrap();
        place_file(&dl, &live, &sha).unwrap();
        assert_eq!(Command::new(&live).status().unwrap().code(), Some(7), "the new one runs");
        drop(running.stdin.take());
        assert!(running.wait().unwrap().success(), "the old one ran on to its end");
    }

    /// The AppImage runtime's keep-alive pipe is inherited without
    /// close-on-exec; the new app must not get it.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_inherited_descriptor_is_not_passed_to_the_new_app() {
        let mut p = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        let cloexec = |fd: i32| unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC != 0;
        assert!(!cloexec(p[0]) && !cloexec(p[1]));
        cloexec_from(p[0].min(p[1]));
        assert!(cloexec(p[0]) && cloexec(p[1]));
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn waiting_for_a_process_returns_once_it_is_gone() {
        let mut child = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().unwrap();
        child.wait().unwrap();
        let t = std::time::Instant::now();
        wait_for_pid(child.id(), 5000);
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
        // ourselves: waits the whole timeout
        let t = std::time::Instant::now();
        wait_for_pid(std::process::id(), 120);
        assert!(t.elapsed() >= std::time::Duration::from_millis(100));
    }
}
