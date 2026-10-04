//! Disk room for the segmented route's temporary files.
//!
//! A long file on the segmented route keeps its intermediate renders next to
//! the output, f64 stereo at the output rate: at the peak two of them plus
//! the FLAC being written (`segmented::temp_peak_bytes`) — about 23 GB for
//! 29 minutes at ×8. Nothing checked for that room before the first write,
//! so a disk that ran out surfaced as a write error minutes in, and with
//! several long files at once as one of them failing part-way.
//!
//! `reserve` is the RAM gate's pattern (`memory::reserve_ram`) applied to a
//! volume: a file whose temporaries do not fit beside the other long files
//! in flight on that volume waits for them; one that does not fit even with
//! nothing else writing there is refused, with a message that says why.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use crate::audio::converter::decode::set_status;

const GIB: f64 = (1u64 << 30) as f64;

/// Bytes reserved per volume root by the long files in flight.
static RESERVED: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// RAII ticket: the room goes back when the file is done.
pub struct DiskReservation {
    volume: String,
    bytes: u64,
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        if self.bytes == 0 {
            return;
        }
        let mut r = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = r.as_mut() {
            if let Some(v) = map.get_mut(&self.volume) {
                *v = v.saturating_sub(self.bytes);
                if *v == 0 {
                    map.remove(&self.volume);
                }
            }
        }
    }
}

/// Wait until `need` bytes of temporaries fit on the volume of `dir` beside
/// what the other long files there have reserved, then reserve them.
///
/// Free space is read from the volume each time, so what the others have
/// already written is counted twice — in their reservations and in what is
/// no longer free. That errs towards waiting, never towards a full disk.
/// A volume that cannot be read is not held up: the write errors remain the
/// backstop, as before this gate existed. A cancel is never queued here.
pub fn reserve(dir: &Path, need: u64, label: &str) -> Result<DiskReservation, String> {
    let Some(volume) = volume_root(dir) else {
        return Ok(DiskReservation { volume: String::new(), bytes: 0 });
    };
    loop {
        {
            let mut r = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
            let map = r.get_or_insert_with(HashMap::new);
            let others = map.get(&volume).copied().unwrap_or(0);
            let Some(free) = free_bytes(dir) else {
                return Ok(DiskReservation { volume, bytes: 0 });
            };
            if free >= need.saturating_add(others) {
                *map.entry(volume.clone()).or_insert(0) += need;
                return Ok(DiskReservation { volume, bytes: need });
            }
            if others == 0 {
                return Err(refusal(need, free, &volume));
            }
        }
        if crate::audio::converter::state::CONV_CANCEL.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(DiskReservation { volume, bytes: 0 });
        }
        set_status(&format!(
            "{}: waiting for disk space for temporary files ({:.1} GB on {})...",
            label,
            need as f64 / GIB,
            volume
        ));
        std::thread::sleep(Duration::from_millis(1500));
    }
}

/// What the queue row says when a long file's temporaries cannot fit.
fn refusal(need: u64, free: u64, volume: &str) -> String {
    format!(
        "Not enough disk space for this file's temporary data: a file this long is converted \
         through temporary files next to it, about {:.0} GB at the peak, and {} has {:.0} GB free. \
         Free up space there, or move the file to a drive with more room.",
        need as f64 / GIB,
        volume,
        free as f64 / GIB
    )
}

#[cfg(windows)]
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// The root of the volume `dir` is on ("D:\", or a mounted folder).
#[cfg(windows)]
pub(crate) fn volume_root(dir: &Path) -> Option<String> {
    let path = wide(dir);
    let mut buf = [0u16; 1024];
    let ok = unsafe {
        winapi::um::fileapi::GetVolumePathNameW(path.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    if ok == 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]).to_lowercase())
}

/// Bytes free for this process on the volume of `dir` (quotas included).
#[cfg(windows)]
pub(crate) fn free_bytes(dir: &Path) -> Option<u64> {
    let path = wide(dir);
    let mut avail: u64 = 0;
    let ok = unsafe {
        winapi::um::fileapi::GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut avail as *mut u64 as *mut winapi::um::winnt::ULARGE_INTEGER,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

#[cfg(not(windows))]
pub(crate) fn volume_root(_dir: &Path) -> Option<String> {
    None
}

#[cfg(not(windows))]
pub(crate) fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// The volume of the build directory reads, and is not empty.
    #[test]
    fn the_build_volume_has_a_root_and_free_space() {
        let dir = std::env::current_dir().unwrap();
        let root = volume_root(&dir).expect("a local volume has a root");
        assert!(root.ends_with('\\'), "{root}");
        assert!(free_bytes(&dir).expect("and reads its free space") > 0);
    }

    /// More than any disk holds is refused at once, not waited for: nothing
    /// else is writing there to make room.
    #[test]
    fn a_file_larger_than_the_free_space_is_refused() {
        let dir = std::env::current_dir().unwrap();
        let err = reserve(&dir, u64::MAX / 2, "[1/1]").err().expect("refused");
        assert!(err.starts_with("Not enough disk space"), "{err}");
    }

    /// A reservation is returned when dropped, so the next file sees the room.
    #[test]
    fn a_reservation_is_released_on_drop() {
        let dir = std::env::current_dir().unwrap();
        let root = volume_root(&dir).unwrap();
        let held = |r: &str| {
            RESERVED
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|m| m.get(r).copied())
                .unwrap_or(0)
        };
        let before = held(&root);
        let t = reserve(&dir, 1 << 20, "[1/1]").expect("a megabyte fits");
        assert_eq!(held(&root), before + (1 << 20));
        drop(t);
        assert_eq!(held(&root), before);
    }
}
