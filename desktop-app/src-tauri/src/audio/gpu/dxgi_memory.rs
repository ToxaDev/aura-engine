//! How much video memory the operating system will let this process use.
//!
//! wgpu 0.19 has no such query, and the convolvers run on Vulkan, which only
//! reports it through an extension wgpu does not enable. Windows answers the
//! question for every graphics API at once: the video memory manager keeps one
//! budget per process per adapter, and DXGI exposes it whether or not the
//! process ever created a Direct3D device.
//!
//! That budget is not what is free on the card, though. On an RTX 4090 with
//! 3.7 GB already held by other programs it read 23 374 MB of 24 142: the
//! budget is how far this process may go before Windows starts moving other
//! processes' memory out of the way, not how far it can go without that. So
//! the reading also carries what every process together holds on the adapter,
//! from the same performance counter Task Manager shows.
//!
//! The adapter is matched by PCI vendor and device id, the one identity wgpu
//! and DXGI both report. Two identical cards cannot be told apart that way, so
//! that case is an error rather than a guess — the caller keeps its floor.

/// One reading of the local (on-card) memory segment.
#[derive(Clone, Copy, Debug)]
pub struct LocalMemory {
    /// What the OS currently lets this process use. Shrinks when other
    /// processes need the card.
    pub budget: u64,
    /// What this process is using now, across every graphics API.
    pub usage: u64,
    /// Physical memory on the card.
    pub dedicated: u64,
    /// What every process together holds on the card, this one included.
    /// `None` when the performance counter could not be read.
    pub adapter_usage: Option<u64>,
}

impl LocalMemory {
    /// What this process can take without pushing anyone else aside: the
    /// smaller of what the OS budget leaves and what the card has free.
    pub fn free(&self) -> u64 {
        let by_budget = self.budget.saturating_sub(self.usage);
        match self.adapter_usage {
            Some(all) => by_budget.min(self.dedicated.saturating_sub(all)),
            None => by_budget,
        }
    }
}

#[cfg(windows)]
pub fn query(vendor_id: u32, device_id: u32) -> Result<LocalMemory, String> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use winapi::shared::dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_DESC1,
        DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    use winapi::shared::dxgi1_4::{
        IDXGIAdapter3, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    };
    use winapi::shared::winerror::DXGI_ERROR_NOT_FOUND;
    use winapi::um::unknwnbase::IUnknown;
    use winapi::Interface;

    /// Releases the COM reference on every exit path.
    struct Com<T>(*mut T);
    impl<T> Drop for Com<T> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { (*(self.0 as *mut IUnknown)).Release() };
            }
        }
    }

    unsafe {
        let mut raw: *mut IDXGIFactory1 = null_mut();
        let hr = CreateDXGIFactory1(
            &IDXGIFactory1::uuidof(),
            &mut raw as *mut *mut IDXGIFactory1 as *mut *mut c_void,
        );
        if hr < 0 || raw.is_null() {
            return Err(format!("CreateDXGIFactory1 failed (0x{:08X})", hr as u32));
        }
        let factory = Com(raw);

        let mut found: Option<(Com<IDXGIAdapter1>, DXGI_ADAPTER_DESC1)> = None;
        let mut matches = 0;
        for index in 0..64u32 {
            let mut raw: *mut IDXGIAdapter1 = null_mut();
            let hr = (*factory.0).EnumAdapters1(index, &mut raw);
            if hr == DXGI_ERROR_NOT_FOUND {
                break;
            }
            if hr < 0 || raw.is_null() {
                continue;
            }
            let adapter = Com(raw);
            let mut desc: DXGI_ADAPTER_DESC1 = std::mem::zeroed();
            if (*adapter.0).GetDesc1(&mut desc) < 0 {
                continue;
            }
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0 {
                continue;
            }
            if desc.VendorId == vendor_id && desc.DeviceId == device_id {
                matches += 1;
                if found.is_none() {
                    found = Some((adapter, desc));
                }
            }
        }

        let Some((adapter, desc)) = found else {
            return Err(format!(
                "no DXGI adapter with vendor 0x{:04X} device 0x{:04X}",
                vendor_id, device_id
            ));
        };
        if matches > 1 {
            return Err(format!(
                "{} adapters share vendor 0x{:04X} device 0x{:04X} — cannot tell which one is ours",
                matches, vendor_id, device_id
            ));
        }

        let mut raw: *mut IDXGIAdapter3 = null_mut();
        let hr = (*adapter.0).QueryInterface(
            &IDXGIAdapter3::uuidof(),
            &mut raw as *mut *mut IDXGIAdapter3 as *mut *mut c_void,
        );
        if hr < 0 || raw.is_null() {
            return Err(format!("adapter has no IDXGIAdapter3 (0x{:08X})", hr as u32));
        }
        let adapter3 = Com(raw);

        let mut info: DXGI_QUERY_VIDEO_MEMORY_INFO = std::mem::zeroed();
        let hr = (*adapter3.0).QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info);
        if hr < 0 {
            return Err(format!("QueryVideoMemoryInfo failed (0x{:08X})", hr as u32));
        }

        Ok(LocalMemory {
            budget: info.Budget,
            usage: info.CurrentUsage,
            dedicated: desc.DedicatedVideoMemory as u64,
            adapter_usage: adapter_dedicated_usage(desc.AdapterLuid).ok(),
        })
    }
}

/// `\GPU Adapter Memory(luid_…_phys_0)\Dedicated Usage`, added by its English
/// name so a localised Windows finds it too.
#[cfg(windows)]
fn adapter_dedicated_usage(luid: winapi::shared::ntdef::LUID) -> Result<u64, String> {
    use std::ptr::{null, null_mut};
    use winapi::um::pdh::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
        PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
    };

    struct Query(PDH_HQUERY);
    impl Drop for Query {
        fn drop(&mut self) {
            unsafe { PdhCloseQuery(self.0) };
        }
    }

    let path = format!(
        "\\GPU Adapter Memory(luid_0x{:08x}_0x{:08x}_phys_0)\\Dedicated Usage",
        luid.HighPart as u32, luid.LowPart
    );
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut raw: PDH_HQUERY = null_mut();
        let status = PdhOpenQueryW(null(), 0, &mut raw);
        if status != 0 {
            return Err(format!("PdhOpenQuery failed (0x{:08X})", status as u32));
        }
        let query = Query(raw);

        let mut counter: PDH_HCOUNTER = null_mut();
        let status = PdhAddEnglishCounterW(query.0, wide.as_ptr(), 0, &mut counter);
        if status != 0 {
            return Err(format!("no counter {} (0x{:08X})", path, status as u32));
        }
        let status = PdhCollectQueryData(query.0);
        if status != 0 {
            return Err(format!("PdhCollectQueryData failed (0x{:08X})", status as u32));
        }
        let mut value: PDH_FMT_COUNTERVALUE = std::mem::zeroed();
        let status = PdhGetFormattedCounterValue(counter, PDH_FMT_LARGE, null_mut(), &mut value);
        if status != 0 || value.CStatus != 0 {
            return Err(format!(
                "counter {} unreadable (0x{:08X}/0x{:08X})",
                path, status as u32, value.CStatus
            ));
        }
        Ok(*value.u.largeValue() as u64)
    }
}

#[cfg(not(windows))]
pub fn query(_vendor_id: u32, _device_id: u32) -> Result<LocalMemory, String> {
    Err("video memory budget is only queried on Windows".to_string())
}
