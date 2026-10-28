//! Aura Player ASIO on this machine: whether players can find it (the 64-
//! and the 32-bit one), whether it is this build's own, and putting it in or
//! taking it out.
//!
//! The driver is a separate program (its own licence, desktop-app/asio): the
//! app never loads it, it only copies its two DLLs to
//! `%ProgramFiles%\AuraEngine\ASIO` and has Windows' regsvr32 register them —
//! the 64-bit regsvr32 for the 64-bit DLL, SysWOW64's for the 32-bit one —
//! in one elevated command, so Windows asks once. A player finds a driver
//! under HKLM\SOFTWARE\ASIO, in its own 64- or 32-bit view.
//!
//! Where the DLLs come from: `AURA_ASIO_DIR`, else `asio\` beside the app
//! (the bundle's), else the asio workspace's release builds when the app
//! runs from its source tree. A release build carries its driver's SHA-256s
//! (not the driver) and takes only DLLs that are those — from `asio\`
//! beside it or the app's data folder, else fetched from its own release
//! (an update from 1.5.x brings the exe alone).

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The driver as players see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverState {
    /// Not registered (or only half of it, its file gone).
    Missing,
    /// Registered for both kinds of player, the files this build's own.
    Installed,
    /// Registered, but older than this build, or for one kind only.
    Stale,
    /// Not on this system (made off Windows only).
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
}

/// How an install or a removal went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    /// Windows asked for permission and it was not given.
    Cancelled,
    /// The DLL copied in was not this build's own: nothing was replaced.
    Mismatch,
    Failed,
}

/// The driver's name in the registry (as the DLL registers it).
pub const NAME: &str = "Aura Player ASIO";
/// The DLLs' names where the app keeps them and where they are installed.
pub const DLL64: &str = "aura_player_asio64.dll";
pub const DLL32: &str = "aura_player_asio32.dll";

/// The SHA-256s (64-bit, 32-bit) of this build's own driver, when it was
/// made with them: a release build is (CI builds the DLLs first). The exe
/// carries the hashes, never the DLLs.
pub fn expected() -> Option<(&'static str, &'static str)> {
    let ok = |h: &str| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
    match (option_env!("AURA_ASIO_SHA256_64"), option_env!("AURA_ASIO_SHA256_32")) {
        (Some(a), Some(b)) if ok(a) && ok(b) => Some((a, b)),
        _ => None,
    }
}

fn hash_is(p: &Path, want: &str) -> bool {
    crate::updater::sha256_file(p).is_ok_and(|h| h.eq_ignore_ascii_case(want))
}

/// The id of the driver's sources (desktop-app/asio), shared with the
/// driver's build script: the marker and the hashing.
#[allow(dead_code)]
#[path = "../../../../asio/source_id.rs"]
mod source_id;

/// The id of the driver sources this build was made with (baked by the
/// build script): a registered DLL that carries it is this build's own
/// driver, whatever its bytes — a rebuild stamps another time into the file,
/// and a newer compiler makes other code of the same sources.
pub fn source_id() -> Option<&'static str> {
    option_env!("AURA_ASIO_SOURCE_ID").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The DLLs in the field from before the id (64-bit, 32-bit): CI's of 1.6.0
/// and 1.6.1, and a local build of the same sources. They differ from this
/// build's driver only by the link stamps or the compiler, so they count as
/// its own — while the sources are those `LEGACY_SOURCE` names (a test
/// holds the two together: a real change of the driver empties the table,
/// and these become older drivers).
const LEGACY: [(&str, &str); 3] = [
    // 1.6.0 (CI)
    ("3b0b870d0a4fa16f2d94e2aa553ff444ab96210ad73ec348fae3185dfb474d10", "599560b68154be88565ff2424fc23769b898b0a4b6f19bcd06a272352a822732"),
    // 1.6.1 (CI)
    ("8bbb0c43d9db11d967466a95707ac2ca3dc19e9bb06cabe0b01f91b7e0b04969", "cc8c4f51b199225a485450298e2fbe1765a32a65a7e3ac42f860aa12e78c3971"),
    // a local build of a0ea41c (17.10)
    ("3a9da9146d1a3fedc384f55fb525a7c5ce90fdc13072d26eebb4820cd5a2ede2", "171b59f72b7994635543b1f95d854a0b0f2ffaa2779b0c36a52c7b562bb544b0"),
];
#[cfg(test)]
const LEGACY_SOURCE: &str = "9b7d948c0c48c7ea0aaf3e1d9ab7883f95d44fce0add55721e1928a5a625a93c";

/// The most of a registered file read to know it (the DLLs are ~200 KB).
const READ_CAP: u64 = 16 << 20;

/// A file's bytes, if it is not larger than `READ_CAP`.
fn read_capped(p: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(p).ok()?.take(READ_CAP + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= READ_CAP).then_some(bytes)
}

/// The source id a DLL's bytes carry behind the marker, if any.
fn id_in(file: &[u8]) -> Option<&str> {
    let marker = source_id::MARKER.as_bytes();
    let at = file.windows(marker.len()).position(|w| w == marker)? + marker.len();
    let id = file.get(at..at + 64)?;
    id.iter().all(u8::is_ascii_hexdigit).then(|| std::str::from_utf8(id).ok()).flatten()
}

/// Whether a registered DLL (`file`, its bytes) is this build's own driver:
/// byte for byte what the build brings (`bundled`), or carrying its source
/// id (`id`), or one of the DLLs in the field from before the id that were
/// built from its sources (`legacy`).
fn is_own(file: &[u8], is64: bool, bundled: bool, id: Option<&str>, legacy: &[(&str, &str)]) -> bool {
    if bundled || id.is_some_and(|id| id_in(file).is_some_and(|f| f.eq_ignore_ascii_case(id))) {
        return true;
    }
    let sha = sha256_hex(file);
    legacy.iter().any(|(h64, h32)| sha.eq_ignore_ascii_case(if is64 { h64 } else { h32 }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `is_own` for the registered file `p`; `bundled` compares its bytes with
/// what the build brings.
#[cfg_attr(not(windows), allow(dead_code))]
fn own_file(p: &Path, is64: bool, bundled: impl Fn(&[u8]) -> bool) -> bool {
    read_capped(p).is_some_and(|f| is_own(&f, is64, bundled(&f), source_id(), &LEGACY))
}

/// Where a release build keeps its driver: `asio\` beside the app (the
/// zip's), else the app's data folder — where a fetched one goes when the
/// app's own folder cannot be written.
pub fn fetch_dirs() -> Vec<PathBuf> {
    let exe = std::env::current_exe().ok();
    let beside = exe.as_deref().and_then(Path::parent).map(|d| d.join("asio"));
    beside.into_iter().chain(crate::app_dir::root().map(|d| d.join("asio"))).collect()
}

/// The two DLLs this build brings, if it can find them. A release build
/// takes them only where both are its own (by their SHA-256s); a build
/// without the hashes, as before.
pub fn bundled() -> Option<(PathBuf, PathBuf)> {
    let env = std::env::var_os("AURA_ASIO_DIR").map(PathBuf::from);
    match expected() {
        Some(want) => verified_from(env.as_deref(), &fetch_dirs(), |p, is64| hash_is(p, if is64 { want.0 } else { want.1 })),
        None => {
            let exe = std::env::current_exe().ok();
            bundled_from(env.as_deref(), exe.as_deref(), |p| p.is_file())
        }
    }
}

/// The first of `env` and `dirs` whose two DLLs are this build's own.
fn verified_from(env: Option<&Path>, dirs: &[PathBuf], own: impl Fn(&Path, bool) -> bool) -> Option<(PathBuf, PathBuf)> {
    env.into_iter()
        .chain(dirs.iter().map(PathBuf::as_path))
        .map(|d| (d.join(DLL64), d.join(DLL32)))
        .find(|p| own(&p.0, true) && own(&p.1, false))
}

/// A release build without its driver at hand (an update from 1.5.x
/// brings the exe alone): it has to fetch it before it can install it.
pub fn needs_fetch() -> bool {
    expected().is_some() && bundled().is_none()
}

/// Fetches this build's driver from its release into `fetch_dirs()`.
pub async fn fetch() -> Result<(), crate::updater::AsioFetchError> {
    let Some(want) = expected() else {
        return Err(crate::updater::AsioFetchError::Mismatch("this build has no driver of its own".into()));
    };
    let url = crate::updater::asio_asset_url(env!("CARGO_PKG_VERSION"));
    crate::updater::fetch_asio_driver(&url, want, &fetch_dirs()).await.map(|_| ())
}

fn bundled_from(env: Option<&Path>, exe: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<(PathBuf, PathBuf)> {
    let pair = |d: &Path| (d.join(DLL64), d.join(DLL32));
    let both = |p: &(PathBuf, PathBuf)| exists(&p.0) && exists(&p.1);
    if let Some(d) = env {
        let p = pair(d);
        return both(&p).then_some(p);
    }
    let dir = exe?.parent()?;
    let p = pair(&dir.join("asio"));
    if both(&p) {
        return Some(p);
    }
    // From the source tree: …\desktop-app\src-tauri\target\<profile>\aura-engine.exe
    // and …\desktop-app\asio\target\<triple>\release\aura_player_asio.dll.
    let desktop = dir.ancestors().find(|a| exists(&a.join("asio").join("Cargo.toml")))?;
    let t = desktop.join("asio").join("target");
    let p = (
        t.join("x86_64-pc-windows-msvc").join("release").join("aura_player_asio.dll"),
        t.join("i686-pc-windows-msvc").join("release").join("aura_player_asio.dll"),
    );
    both(&p).then_some(p)
}

/// Where the DLLs are installed.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(|p| PathBuf::from(p).join("AuraEngine").join("ASIO"))
}

/// The state from what is registered and what this build brings: `reg64`
/// and `reg32` the DLLs the two views register (if they do), `same` whether
/// an installed file is the bundled one.
fn state_from(reg64: Option<&Path>, reg32: Option<&Path>, bundled: bool, exists: impl Fn(&Path) -> bool, same: impl Fn(&Path, bool) -> bool) -> DriverState {
    let r64 = reg64.filter(|p| exists(p));
    let r32 = reg32.filter(|p| exists(p));
    match (r64, r32) {
        (None, None) => DriverState::Missing,
        (Some(a), Some(b)) if !bundled || (same(a, true) && same(b, false)) => DriverState::Installed,
        _ => DriverState::Stale,
    }
}

/// The step that clears the way for a new `file`: deleted, or — while a
/// player has it loaded, when Windows refuses the delete but allows a rename
/// — moved aside as `<file>.<stamp>.old` (the player keeps the old one until
/// it lets go of it). Fails only when neither can be done.
fn aside_step(file: &Path, stamp: u64) -> String {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let q = format!("\"{}\"", file.display());
    format!("(if exist {q} (del /q {q} 2>nul & if exist {q} ren {q} \"{name}.{stamp}.old\"))")
}

/// The step that deletes what earlier installs moved aside, those no player
/// holds any longer; it never fails.
fn old_files_step(dir: &Path) -> String {
    format!("(del /q \"{}\" 2>nul & ver >nul)", dir.join("*.old").display())
}

/// The exit code of an install whose copied DLL was not the build's own.
pub const MISMATCH_EXIT: u32 = 9;

/// The step that checks `file` is `sha256` (lowercase hex) — certutil, as
/// the elevated command can run it; when it is not, `drop` is deleted and
/// the command ends with MISMATCH_EXIT.
fn hash_step(file: &Path, sha256: &str, drop: &[&Path], root: &Path) -> String {
    let q = |p: &Path| format!("\"{}\"", p.display());
    let sys = root.join("System32");
    let drop: Vec<String> = drop.iter().map(|p| q(p)).collect();
    format!(
        "({} -hashfile {} SHA256 | {} /i /x /c:\"{sha256}\" >nul || (del /q {} 2>nul & cmd /c exit {MISMATCH_EXIT}))",
        q(&sys.join("certutil.exe")),
        q(file),
        q(&sys.join("findstr.exe")),
        drop.join(" ")
    )
}

/// The command that copies the DLLs into `dir` and registers them; `root`
/// is Windows' folder, `stamp` names what a loaded old DLL is moved to.
/// They are copied in as `.new` first; with `want` (a release build's
/// SHA-256s) the copies — in a folder only an administrator writes — are
/// checked before anything is replaced or registered, so a DLL swapped in
/// the user's folder while Windows asks is never registered.
pub fn install_command(src: &(PathBuf, PathBuf), dir: &Path, root: &Path, stamp: u64, want: Option<(&str, &str)>) -> String {
    let (d64, d32) = (dir.join(DLL64), dir.join(DLL32));
    let (n64, n32) = (dir.join(format!("{DLL64}.new")), dir.join(format!("{DLL32}.new")));
    let q = |p: &Path| format!("\"{}\"", p.display());
    let mut steps = vec![
        format!("(if not exist {d} mkdir {d})", d = q(dir)),
        old_files_step(dir),
        format!("copy /y {} {} >nul", q(&src.0), q(&n64)),
        format!("copy /y {} {} >nul", q(&src.1), q(&n32)),
    ];
    if let Some((h64, h32)) = want {
        steps.push(hash_step(&n64, &h64.to_ascii_lowercase(), &[&n64, &n32], root));
        steps.push(hash_step(&n32, &h32.to_ascii_lowercase(), &[&n64, &n32], root));
    }
    steps.extend([
        aside_step(&d64, stamp),
        aside_step(&d32, stamp),
        format!("move /y {} {} >nul", q(&n64), q(&d64)),
        format!("move /y {} {} >nul", q(&n32), q(&d32)),
        format!("{} /s {}", q(&root.join("System32").join("regsvr32.exe")), q(&d64)),
        format!("{} /s {}", q(&root.join("SysWOW64").join("regsvr32.exe")), q(&d32)),
    ]);
    steps.join(" && ")
}

/// The command that unregisters `reg64` / `reg32` and deletes the
/// installed files (a file a player still has loaded is moved aside, and
/// goes with the next install or removal).
pub fn remove_command(reg64: Option<&Path>, reg32: Option<&Path>, dir: &Path, root: &Path, stamp: u64) -> String {
    let q = |p: &Path| format!("\"{}\"", p.display());
    let mut steps = Vec::new();
    if let Some(p) = reg64 {
        steps.push(format!("{} /u /s {}", q(&root.join("System32").join("regsvr32.exe")), q(p)));
    }
    if let Some(p) = reg32 {
        steps.push(format!("{} /u /s {}", q(&root.join("SysWOW64").join("regsvr32.exe")), q(p)));
    }
    steps.push(old_files_step(dir));
    steps.push(aside_step(&dir.join(DLL64), stamp));
    steps.push(aside_step(&dir.join(DLL32), stamp));
    steps.push(format!("rmdir {} 2>nul", q(dir)));
    // Every step runs: what is gone already does not stop the rest.
    steps.join(" & ")
}

/// A name for what is moved aside now, new with every install.
fn stamp_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(windows)]
mod os {
    use std::path::PathBuf;

    use winapi::shared::minwindef::HKEY;
    use winapi::um::winreg::{RegGetValueW, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE};

    use super::{Outcome, NAME};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;
    const RRF_SUBKEY_WOW6432KEY: u32 = 0x0002_0000;

    /// A string value, read in the 64- or the 32-bit view.
    fn reg_sz(root: HKEY, path: &str, name: Option<&str>, wow32: bool) -> Option<String> {
        let flags = RRF_RT_REG_SZ | if wow32 { RRF_SUBKEY_WOW6432KEY } else { RRF_SUBKEY_WOW6464KEY };
        let sub = wide(path);
        let val = name.map(wide);
        let val_ptr = val.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
        let mut len: u32 = 0;
        // SAFETY: null-terminated strings, valid out-pointers, the buffer's
        // length passed.
        unsafe {
            if RegGetValueW(root, sub.as_ptr(), val_ptr, flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut len) != 0 || len == 0 {
                return None;
            }
            let mut buf = vec![0u16; len as usize / 2 + 1];
            let mut len2 = len;
            if RegGetValueW(root, sub.as_ptr(), val_ptr, flags, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut len2) != 0 {
                return None;
            }
            let s: Vec<u16> = buf.into_iter().take_while(|&c| c != 0).collect();
            Some(String::from_utf16_lossy(&s))
        }
    }

    /// The DLL the 64- or 32-bit view registers for the driver.
    pub fn registered(wow32: bool) -> Option<PathBuf> {
        let clsid = reg_sz(HKEY_LOCAL_MACHINE, &format!("SOFTWARE\\ASIO\\{NAME}"), Some("CLSID"), wow32)?;
        let path = reg_sz(HKEY_CLASSES_ROOT, &format!("CLSID\\{clsid}\\InprocServer32"), None, wow32)?;
        Some(PathBuf::from(path))
    }

    /// Runs `cmd /s /c "<command>"` elevated, Windows asking once, and waits.
    pub fn run_elevated(command: &str) -> Outcome {
        use winapi::shared::winerror::ERROR_CANCELLED;
        use winapi::um::errhandlingapi::GetLastError;
        use winapi::um::processthreadsapi::GetExitCodeProcess;
        use winapi::um::shellapi::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
        use winapi::um::synchapi::WaitForSingleObject;
        use winapi::um::winbase::INFINITE;
        use winapi::um::winuser::SW_HIDE;

        let verb = wide("runas");
        let file = wide("cmd.exe");
        let params = wide(&format!("/s /c \"{command}\""));
        // SAFETY: the strings outlive the call; the process handle is closed.
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = params.as_ptr();
            info.nShow = SW_HIDE;
            if ShellExecuteExW(&mut info) == 0 {
                return if GetLastError() == ERROR_CANCELLED { Outcome::Cancelled } else { Outcome::Failed };
            }
            if info.hProcess.is_null() {
                return Outcome::Failed;
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 1u32;
            GetExitCodeProcess(info.hProcess, &mut code);
            winapi::um::handleapi::CloseHandle(info.hProcess);
            match code {
                0 => Outcome::Ok,
                super::MISMATCH_EXIT => Outcome::Mismatch,
                _ => Outcome::Failed,
            }
        }
    }

    pub fn windows_dir() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
    }
}

/// The driver's state on this machine.
pub fn state() -> DriverState {
    #[cfg(windows)]
    {
        let (r64, r32) = (os::registered(false), os::registered(true));
        // A release build knows its own driver by its SHA-256s, whether or
        // not its DLLs are at hand yet — or by the source id the DLL carries
        // (`own_file`).
        if let Some(want) = expected() {
            return state_from(r64.as_deref(), r32.as_deref(), true, |p| p.is_file(), |p, is64| {
                own_file(p, is64, |f| sha256_hex(f).eq_ignore_ascii_case(if is64 { want.0 } else { want.1 }))
            });
        }
        let b = bundled();
        state_from(r64.as_deref(), r32.as_deref(), b.is_some(), |p| p.is_file(), |p, is64| match &b {
            Some((b64, b32)) => own_file(p, is64, |f| std::fs::read(if is64 { b64 } else { b32 }).is_ok_and(|x| x == f)),
            None => true,
        })
    }
    #[cfg(not(windows))]
    {
        DriverState::Unsupported
    }
}

/// Installs (or updates) the driver: Windows asks for permission once.
pub fn install() -> Outcome {
    #[cfg(windows)]
    {
        let (Some(src), Some(dir)) = (bundled(), install_dir()) else { return Outcome::Failed };
        os::run_elevated(&install_command(&src, &dir, &os::windows_dir(), stamp_now(), expected()))
    }
    #[cfg(not(windows))]
    {
        Outcome::Failed
    }
}

/// Unregisters the driver and deletes its installed files.
pub fn remove() -> Outcome {
    #[cfg(windows)]
    {
        let Some(dir) = install_dir() else { return Outcome::Failed };
        let (r64, r32) = (os::registered(false), os::registered(true));
        os::run_elevated(&remove_command(r64.as_deref(), r32.as_deref(), &dir, &os::windows_dir(), stamp_now()))
    }
    #[cfg(not(windows))]
    {
        Outcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn the_dlls_are_looked_for_in_the_folder_named_beside_the_app_then_in_the_source_tree() {
        let all = |_: &Path| true;
        let named = bundled_from(Some(&p("X:/asio")), Some(&p("Y:/app/aura.exe")), all).unwrap();
        assert_eq!(named, (p("X:/asio").join(DLL64), p("X:/asio").join(DLL32)));
        let beside = bundled_from(None, Some(&p("Y:/app/aura.exe")), all).unwrap();
        assert_eq!(beside.0, p("Y:/app").join("asio").join(DLL64));
        let in_tree = |q: &Path| q == p("Y:/d/asio/Cargo.toml") || q.starts_with(p("Y:/d/asio/target"));
        let tree = bundled_from(None, Some(&p("Y:/d/src-tauri/target/release/aura.exe")), in_tree).unwrap();
        assert_eq!(tree.0, p("Y:/d/asio/target/x86_64-pc-windows-msvc/release/aura_player_asio.dll"));
        assert_eq!(tree.1, p("Y:/d/asio/target/i686-pc-windows-msvc/release/aura_player_asio.dll"));
        assert_eq!(bundled_from(Some(&p("X:/asio")), None, |_| false), None, "a folder named but empty is not searched past");
    }

    /// A release build takes DLLs only where both are its own: the folder
    /// named first, then `asio\` beside the app, then the data folder; a
    /// folder with one right and one wrong is passed over.
    #[test]
    fn a_release_build_takes_the_first_folder_whose_two_dlls_are_its_own() {
        let dirs = [p("Y:/app/asio"), p("Z:/data/asio")];
        let own_in = |good: &'static [&'static str]| move |q: &Path, _: bool| good.iter().any(|g| q.starts_with(g));
        let first = verified_from(None, &dirs, own_in(&["Y:/app/asio", "Z:/data/asio"])).unwrap();
        assert_eq!(first, (p("Y:/app/asio").join(DLL64), p("Y:/app/asio").join(DLL32)));
        let data = verified_from(None, &dirs, own_in(&["Z:/data/asio"])).unwrap();
        assert_eq!(data.0, p("Z:/data/asio").join(DLL64), "the last release's asio\\ beside the app is passed over");
        let named = verified_from(Some(&p("X:/d")), &dirs, own_in(&["X:/d", "Y:/app/asio"])).unwrap();
        assert_eq!(named.1, p("X:/d").join(DLL32));
        let half = |q: &Path, is64: bool| is64 || !q.starts_with("Y:/app/asio");
        assert_eq!(verified_from(None, &dirs[..1], half), None, "a 64-bit DLL its own and a 32-bit one not");
        assert_eq!(verified_from(None, &dirs, |_, _| false), None);
    }

    #[test]
    fn the_state_from_the_registry_and_the_files() {
        let all = |_: &Path| true;
        let same = |_: &Path, _: bool| true;
        let differ = |_: &Path, _: bool| false;
        let (a, b) = (p("C:/x/64.dll"), p("C:/x/32.dll"));
        assert_eq!(state_from(None, None, true, all, same), DriverState::Missing);
        assert_eq!(state_from(Some(&a), Some(&b), true, all, same), DriverState::Installed);
        assert_eq!(state_from(Some(&a), Some(&b), true, all, differ), DriverState::Stale, "an older build's files");
        assert_eq!(state_from(Some(&a), None, true, all, same), DriverState::Stale, "for 64-bit players only");
        assert_eq!(state_from(Some(&a), Some(&b), false, all, differ), DriverState::Installed, "nothing to compare with");
        assert_eq!(state_from(Some(&a), Some(&b), true, |_| false, same), DriverState::Missing, "registered, files gone");
    }

    fn dll_with(id: &str) -> Vec<u8> {
        let mut f = b"MZ\x90\0 code ".to_vec();
        f.extend_from_slice(source_id::MARKER.as_bytes());
        f.extend_from_slice(id.as_bytes());
        f.extend_from_slice(b"\0 more code");
        f
    }

    const ID: &str = "9b7d948c0c48c7ea0aaf3e1d9ab7883f95d44fce0add55721e1928a5a625a93c";
    const OTHER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_dlls_source_id_is_read_from_behind_its_marker() {
        assert_eq!(id_in(&dll_with(ID)), Some(ID));
        assert_eq!(id_in(b"MZ no marker at all"), None);
        let marker = source_id::MARKER.as_bytes();
        assert_eq!(id_in(marker), None, "the marker at the very end");
        assert_eq!(id_in(&[marker, b"9b7d948c"].concat()), None, "cut short");
        assert_eq!(id_in(&dll_with(&"z".repeat(64))), None, "not hex");
        assert_eq!(id_in(&[]), None);
        let garbage: Vec<u8> = (0..100_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        assert_eq!(id_in(&garbage), None);
    }

    /// A registered DLL is the build's own when it is the bundled one, when
    /// it carries the build's source id whatever its other bytes, or when it
    /// is one of the DLLs from before the id built from the same sources —
    /// for its own kind of player only.
    #[test]
    fn a_registered_dll_is_known_by_its_bytes_its_source_id_or_the_table_of_older_builds() {
        let none: [(&str, &str); 0] = [];
        assert!(is_own(b"anything", true, true, Some(ID), &none), "the bundled bytes");
        assert!(is_own(&dll_with(ID), true, false, Some(ID), &none), "a rebuild of the same sources");
        assert!(is_own(&dll_with(&ID.to_uppercase()), false, false, Some(ID), &none));
        assert!(!is_own(&dll_with(OTHER), true, false, Some(ID), &none), "other sources");
        assert!(!is_own(b"MZ an older driver, no id", true, false, Some(ID), &none));
        assert!(!is_own(&dll_with(ID), true, false, None, &none), "a build that knows no id");

        let old64 = b"MZ the 64-bit DLL of 1.6.0".to_vec();
        let old32 = b"MZ the 32-bit DLL of 1.6.0".to_vec();
        let table = [(sha256_hex(&old64), sha256_hex(&old32))];
        let table: Vec<(&str, &str)> = table.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert!(is_own(&old64, true, false, Some(ID), &table), "a DLL from the field");
        assert!(is_own(&old32, false, false, Some(ID), &table));
        assert!(!is_own(&old32, true, false, Some(ID), &table), "the 32-bit one registered for 64-bit players");
        assert!(!is_own(b"MZ another older driver", true, false, Some(ID), &table));
    }

    #[test]
    fn the_older_builds_in_the_field_are_the_published_and_the_local_ones() {
        let known = |h: &str| LEGACY.iter().any(|(a, b)| *a == h || *b == h);
        // 1.6.0 and 1.6.1 (asio-v1.6.1.sha256) and Anton's a0ea41c build.
        for h in ["3b0b870d0a4fa16f2d94e2aa553ff444ab96210ad73ec348fae3185dfb474d10", "599560b68154be88565ff2424fc23769b898b0a4b6f19bcd06a272352a822732",
                  "8bbb0c43d9db11d967466a95707ac2ca3dc19e9bb06cabe0b01f91b7e0b04969", "cc8c4f51b199225a485450298e2fbe1765a32a65a7e3ac42f860aa12e78c3971"] {
            assert!(known(h), "{h}");
        }
        assert!(LEGACY.iter().all(|(a, b)| a.len() == 64 && b.len() == 64 && a != b));
    }

    /// The table of older builds holds while the driver's sources are those
    /// it was written for. When this fails the driver has changed: if its
    /// change matters to players, empty `LEGACY` (the older DLLs are then
    /// older drivers, offered an update); if it does not (a comment, a
    /// test), put the new id in `LEGACY_SOURCE`.
    #[test]
    fn the_table_of_older_builds_is_for_these_driver_sources() {
        assert_eq!(source_id(), Some(LEGACY_SOURCE));
    }

    /// The id baked into the app is the one its sources give now, and the
    /// shared SHA-256 is SHA-256.
    #[test]
    fn the_source_id_is_the_sha256_of_the_driver_sources() {
        let asio = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("asio");
        let (id, watched) = source_id::compute(&asio).unwrap();
        assert_eq!(source_id(), Some(id.as_str()));
        assert!(watched.iter().any(|p| p.ends_with("driver/src/lib.rs")));
        assert!(!watched.iter().any(|p| p.components().any(|c| c.as_os_str() == "target")));
        for n in [0usize, 1, 3, 55, 56, 63, 64, 65, 119, 1000] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
            let ours: String = source_id::sha256(&data).iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(ours, sha256_hex(&data), "{n} bytes");
        }
        let abc: String = source_id::sha256(b"abc").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(abc, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn a_registered_file_is_read_whole_up_to_a_cap() {
        let dir = std::env::temp_dir().join(format!("aura-asio-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("small.dll");
        std::fs::write(&f, dll_with(ID)).unwrap();
        assert_eq!(read_capped(&f), Some(dll_with(ID)));
        assert_eq!(read_capped(&dir.join("gone.dll")), None);
        let big = dir.join("big.dll");
        std::fs::File::create(&big).unwrap().set_len(READ_CAP + 1).unwrap();
        assert_eq!(read_capped(&big), None, "too large to be the driver");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "cmd.exe steps on Windows paths")]
    fn the_install_copies_then_registers_each_dll_with_its_own_regsvr32() {
        let c = install_command(&(p("D:/b/64.dll"), p("D:/b/32.dll")), &p("C:/Program Files/AuraEngine/ASIO"), &p("C:/Windows"), 7, None);
        let steps: Vec<&str> = c.split(" && ").collect();
        assert_eq!(steps.len(), 10, "{c}");
        assert!(steps[0].starts_with("(if not exist \"C:/Program Files/AuraEngine/ASIO\" mkdir"));
        assert!(steps[1].contains("*.old"), "{}", steps[1]);
        assert!(steps[2].starts_with("copy /y \"D:/b/64.dll\"") && steps[2].contains(&format!("{DLL64}.new")), "{}", steps[2]);
        assert!(steps[4].contains(&format!("ren \"C:/Program Files/AuraEngine/ASIO\\{DLL64}\" \"{DLL64}.7.old\"")), "{}", steps[4]);
        assert!(steps[5].contains(&format!("{DLL32}.7.old")), "{}", steps[5]);
        assert!(steps[6].starts_with("move /y"), "{}", steps[6]);
        assert!(steps[8].contains("System32") && steps[8].ends_with(&format!("{DLL64}\"")), "{}", steps[8]);
        assert!(steps[9].contains("SysWOW64") && steps[9].ends_with(&format!("{DLL32}\"")), "{}", steps[9]);
        let h = install_command(&(p("D:/b/64.dll"), p("D:/b/32.dll")), &p("C:/i"), &p("C:/Windows"), 7, Some(("AB", "cd")));
        let hs: Vec<&str> = h.split(" && ").collect();
        assert_eq!(hs.len(), 12, "{h}");
        assert!(hs[4].contains("certutil.exe") && hs[4].contains("/c:\"ab\"") && hs[5].contains("/c:\"cd\""), "checked after the copy, before anything is replaced: {h}");
        let r = remove_command(Some(&p("C:/i/64.dll")), None, &p("C:/i"), &p("C:/Windows"), 7);
        assert!(r.contains("System32") && !r.contains("SysWOW64") && r.contains("del /q") && r.contains(".7.old") && r.contains("rmdir"), "{r}");
    }

    /// A DLL a player has loaded (mapped as an image: Windows refuses to
    /// delete it, allows a rename) is moved aside and the new one copied in;
    /// once let go of, the next install deletes what was moved aside. Run
    /// for real through cmd, as the elevated command runs, without the
    /// registration.
    #[cfg(windows)]
    #[test]
    fn a_dll_a_player_holds_is_moved_aside_and_replaced() {
        use std::os::windows::process::CommandExt;
        use winapi::um::libloaderapi::{FreeLibrary, LoadLibraryExW, LOAD_LIBRARY_AS_IMAGE_RESOURCE};

        let dir = tempfile::TempDir::new().unwrap();
        let live = dir.path().join(DLL64);
        let sys = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("version.dll");
        std::fs::copy(&sys, &live).unwrap();
        let new = dir.path().join("new.dll");
        std::fs::write(&new, b"the new build").unwrap();
        let wide: Vec<u16> = live.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        // SAFETY: a null-terminated path; mapped as an image resource only, no code runs.
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_AS_IMAGE_RESOURCE) };
        assert!(!module.is_null(), "the copy is mapped");
        assert!(std::fs::remove_file(&live).is_err(), "a mapped image cannot be deleted");

        let run = |steps: &[String]| {
            std::process::Command::new("cmd.exe").raw_arg(format!("/s /c \"{}\"", steps.join(" && "))).status().unwrap().success()
        };
        let copy = format!("copy /y \"{}\" \"{}\" >nul", new.display(), live.display());
        assert!(run(&[old_files_step(dir.path()), aside_step(&live, 42), copy.clone()]));
        assert_eq!(std::fs::read(&live).unwrap(), b"the new build");
        let aside = dir.path().join(format!("{DLL64}.42.old"));
        assert!(aside.is_file(), "the held one moved aside");

        assert!(run(&[old_files_step(dir.path())]), "a held old file does not fail the install");
        assert!(aside.is_file());
        // SAFETY: the module loaded above.
        unsafe { FreeLibrary(module) };
        assert!(run(&[old_files_step(dir.path()), aside_step(&live, 43), copy]));
        assert!(!aside.exists(), "let go of, it goes with the next install");
        assert!(!dir.path().join(format!("{DLL64}.43.old")).exists(), "a file nobody holds is deleted, not moved");
    }

    /// The install as the elevated command runs it, through cmd for real but
    /// without the registration: the copies are checked against the build's
    /// SHA-256s before anything is replaced. A copy that is not the build's
    /// own ends it with MISMATCH_EXIT, the copies deleted, the installed
    /// DLLs as they were; the right ones replace them.
    #[cfg(windows)]
    #[test]
    fn a_copy_that_is_not_the_builds_own_replaces_nothing() {
        use std::os::windows::process::CommandExt;
        use sha2::{Digest, Sha256};

        let t = tempfile::TempDir::new().unwrap();
        let (src, dir) = (t.path().join("user"), t.path().join("ASIO"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let s = (src.join("a64.dll"), src.join("a32.dll"));
        std::fs::write(&s.0, b"the 64-bit build").unwrap();
        std::fs::write(&s.1, b"the 32-bit build").unwrap();
        std::fs::write(dir.join(DLL64), b"installed 64").unwrap();
        std::fs::write(dir.join(DLL32), b"installed 32").unwrap();
        let hex = |d: &[u8]| Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect::<String>();
        let (h64, h32) = (hex(b"the 64-bit build").to_uppercase(), hex(b"the 32-bit build"));
        let root = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let run = |want: (&str, &str)| {
            let c = install_command(&s, &dir, &root, 5, Some(want));
            let steps: Vec<&str> = c.split(" && ").collect();
            let without_registering = steps[..steps.len() - 2].join(" && ");
            std::process::Command::new("cmd.exe").raw_arg(format!("/s /c \"{without_registering}\"")).status().unwrap().code()
        };

        let other = hex(b"another build");
        assert_eq!(run((&h64, &other)), Some(MISMATCH_EXIT as i32));
        assert_eq!(std::fs::read(dir.join(DLL64)).unwrap(), b"installed 64", "nothing replaced");
        assert_eq!(std::fs::read(dir.join(DLL32)).unwrap(), b"installed 32");
        assert!(!dir.join(format!("{DLL64}.new")).exists() && !dir.join(format!("{DLL32}.new")).exists(), "the copies deleted");

        assert_eq!(run((&h64, &h32)), Some(0));
        assert_eq!(std::fs::read(dir.join(DLL64)).unwrap(), b"the 64-bit build");
        assert_eq!(std::fs::read(dir.join(DLL32)).unwrap(), b"the 32-bit build");
        assert!(!dir.join(format!("{DLL64}.new")).exists());
    }
}
