//! The USB audio devices plugged in, as they name themselves on the bus.
//!
//! A DAC's driver often names no model: Chord's one driver serves the Mojo 2,
//! Hugo 2 and Qutest alike ("Chord Async USB 44.1kHz-768kHz"), under one
//! VID:PID (245f:0815). The model is only in the USB product string the DAC
//! reports ("Mojo2"). The UI looks the pair up in its list of known DACs.

use serde::Serialize;

/// One USB device with an audio function.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct UsbAudio {
    /// "vvvv:pppp", lowercase hex.
    pub usb: String,
    /// The product string the device reports ("Mojo2"); empty if none.
    pub product: String,
}

/// "vvvv:pppp" from a device instance id ("USB\VID_245F&PID_0815&MI_00\…")
/// or a sysfs pair.
pub(crate) fn vid_pid_of(instance_id: &str) -> Option<String> {
    let up = instance_id.to_ascii_uppercase();
    let hex4 = |key: &str| -> Option<String> {
        let at = up.find(key)? + key.len();
        let s = up.get(at..at + 4)?;
        s.chars().all(|c| c.is_ascii_hexdigit()).then(|| s.to_ascii_lowercase())
    };
    Some(format!("{}:{}", hex4("VID_")?, hex4("PID_")?))
}

/// Whether an instance id is one interface of a composite device ("&MI_00").
pub(crate) fn is_interface(instance_id: &str) -> bool {
    instance_id.to_ascii_uppercase().contains("&MI_")
}

/// One device found on the bus: its instance id, whether it is an audio
/// function, and the description the bus reports for it.
pub(crate) struct BusDevice {
    pub instance_id: String,
    pub audio: bool,
    pub bus_desc: String,
}

/// The audio devices among `all`: one row per VID:PID and product string. A
/// composite device's product string is its parent's (the interfaces report
/// their own interface names, when they have any).
pub(crate) fn audio_devices(all: &[BusDevice]) -> Vec<UsbAudio> {
    let mut out: Vec<UsbAudio> = Vec::new();
    for d in all.iter().filter(|d| d.audio) {
        let Some(usb) = vid_pid_of(&d.instance_id) else { continue };
        let parent = all
            .iter()
            .find(|p| !is_interface(&p.instance_id) && vid_pid_of(&p.instance_id).as_deref() == Some(&usb));
        let product = parent
            .map(|p| p.bus_desc.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| d.bus_desc.trim())
            .to_string();
        let row = UsbAudio { usb, product };
        if !out.contains(&row) {
            out.push(row);
        }
    }
    out
}

/// The USB audio devices present now. Empty where the system cannot tell.
pub fn list() -> Vec<UsbAudio> {
    let found = imp::bus_devices();
    let list = audio_devices(&found);
    let line: Vec<String> = list.iter().map(|d| format!("{} \"{}\"", d.usb, d.product)).collect();
    crate::aelog!("[USB] audio: {}", if line.is_empty() { "none".to_string() } else { line.join(", ") });
    list
}

#[cfg(windows)]
mod imp {
    use super::BusDevice;
    use std::ptr::null_mut;
    use winapi::shared::devguid::GUID_DEVCLASS_MEDIA;
    use winapi::shared::devpkey::DEVPKEY_Device_BusReportedDeviceDesc;
    use winapi::shared::guiddef::IsEqualGUID;
    use winapi::shared::minwindef::DWORD;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::setupapi::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
        SetupDiGetDevicePropertyW, DIGCF_ALLCLASSES, DIGCF_PRESENT, SP_DEVINFO_DATA,
    };

    fn utf16_z(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// Every USB device present (all classes), from SetupAPI.
    pub(super) fn bus_devices() -> Vec<BusDevice> {
        let mut out = Vec::new();
        let enumerator: Vec<u16> = "USB\0".encode_utf16().collect();
        // SAFETY: plain SetupAPI calls on buffers we own; the set is
        // destroyed before returning.
        unsafe {
            let set = SetupDiGetClassDevsW(
                std::ptr::null(),
                enumerator.as_ptr(),
                null_mut(),
                DIGCF_ALLCLASSES | DIGCF_PRESENT,
            );
            if set == INVALID_HANDLE_VALUE as _ {
                return out;
            }
            let mut i: DWORD = 0;
            loop {
                let mut data: SP_DEVINFO_DATA = std::mem::zeroed();
                data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as DWORD;
                if SetupDiEnumDeviceInfo(set, i, &mut data) == 0 {
                    break;
                }
                i += 1;
                let mut id = [0u16; 512];
                let mut need: DWORD = 0;
                if SetupDiGetDeviceInstanceIdW(set, &mut data, id.as_mut_ptr(), id.len() as DWORD, &mut need) == 0 {
                    continue;
                }
                let mut desc = [0u16; 256];
                let mut ty: DWORD = 0;
                let ok = SetupDiGetDevicePropertyW(
                    set,
                    &mut data,
                    &DEVPKEY_Device_BusReportedDeviceDesc,
                    &mut ty,
                    desc.as_mut_ptr() as *mut u8,
                    (desc.len() * 2) as DWORD,
                    &mut need,
                    0,
                ) != 0;
                out.push(BusDevice {
                    instance_id: utf16_z(&id),
                    audio: IsEqualGUID(&data.ClassGuid, &GUID_DEVCLASS_MEDIA),
                    bus_desc: if ok { utf16_z(&desc) } else { String::new() },
                });
            }
            SetupDiDestroyDeviceInfoList(set);
        }
        out
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::BusDevice;
    use std::fs;
    use std::path::Path;

    fn read(p: &Path) -> String {
        fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
    }

    /// Every USB device in sysfs with an audio interface (class 01).
    pub(super) fn bus_devices() -> Vec<BusDevice> {
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir("/sys/bus/usb/devices") else { return out };
        for e in dir.flatten() {
            let p = e.path();
            let (vid, pid) = (read(&p.join("idVendor")), read(&p.join("idProduct")));
            if vid.is_empty() || pid.is_empty() {
                continue;
            }
            let audio = fs::read_dir(&p).into_iter().flatten().flatten().any(|i| {
                let f = i.path().join("bInterfaceClass");
                read(&f) == "01"
            });
            out.push(BusDevice {
                instance_id: format!("USB\\VID_{vid}&PID_{pid}"),
                audio,
                bus_desc: read(&p.join("product")),
            });
        }
        out
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::BusDevice;
    /// macOS: not read yet (the DAC is told by its CoreAudio name there).
    pub(super) fn bus_devices() -> Vec<BusDevice> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, audio: bool, desc: &str) -> BusDevice {
        BusDevice { instance_id: id.into(), audio, bus_desc: desc.into() }
    }

    #[test]
    fn vid_pid_is_read_from_an_instance_id() {
        assert_eq!(vid_pid_of(r"USB\VID_245F&PID_0815\413-001").as_deref(), Some("245f:0815"));
        assert_eq!(vid_pid_of(r"USB\VID_2972&PID_0047&MI_00\7&1").as_deref(), Some("2972:0047"));
        assert_eq!(vid_pid_of(r"USB\ROOT_HUB30\4&1"), None);
        assert_eq!(vid_pid_of(r"USB\VID_12&PID_0815"), None);
    }

    #[test]
    fn the_product_string_is_the_parents_for_a_composite_device() {
        let all = [
            dev(r"USB\VID_245F&PID_0815\413-001", false, "Mojo2"),
            dev(r"USB\VID_245F&PID_0815&MI_00\8&2", true, ""),
            dev(r"USB\VID_245F&PID_0815&MI_01\8&2", true, "Chord Audio Control"),
            dev(r"USB\VID_046D&PID_C52B\5&1", false, "USB Receiver"),
        ];
        assert_eq!(audio_devices(&all), vec![UsbAudio { usb: "245f:0815".into(), product: "Mojo2".into() }]);
    }

    #[test]
    fn a_single_function_device_names_itself() {
        let all = [dev(r"USB\VID_2972&PID_0093\5&9", true, "FIIO KA17 ")];
        assert_eq!(audio_devices(&all), vec![UsbAudio { usb: "2972:0093".into(), product: "FIIO KA17".into() }]);
    }

    /// On a real machine: `cargo test … usb_audio -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn prints_the_audio_devices_of_this_machine() {
        for d in audio_devices(&imp::bus_devices()) {
            println!("USB audio: {} \"{}\"", d.usb, d.product);
        }
    }

    #[test]
    fn no_audio_device_gives_an_empty_list() {
        assert!(audio_devices(&[dev(r"USB\VID_046D&PID_C52B\5&1", false, "USB Receiver")]).is_empty());
    }
}
