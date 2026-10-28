//! A system with no sound backend yet: no devices, and a stream does not open.

use super::*;

pub(super) struct NoOutput;

impl Backend for NoOutput {
    type Stream = NoStream;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        Ok(Vec::new())
    }

    fn probe_formats(_device_id: Option<&str>, _rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        Ok(Vec::new())
    }

    fn diagnose(_device_id: Option<&str>, _rates: &[u32]) -> Result<String, String> {
        Ok(String::new())
    }

    fn open(_cfg: &OutputConfig) -> Result<Opened<NoStream>, String> {
        Err("Sound output is not available on this system yet".to_string())
    }
}

/// Never opened.
pub(super) enum NoStream {}

impl Stream for NoStream {
    type Dev<'a> = NoStream;

    fn prefill(&mut self, _frames: usize, _bytes: &[u8]) -> Result<(), String> {
        match *self {}
    }

    fn start<'a>(&'a mut self, _shared: &'a OutputShared) -> Result<NoStream, String> {
        match *self {}
    }

    fn stop(&mut self) {
        match *self {}
    }
}

impl Device for NoStream {
    fn wait(&mut self) -> bool {
        match *self {}
    }

    fn space(&mut self) -> Result<usize, String> {
        match *self {}
    }

    fn write(&mut self, _frames: usize, _bytes: &[u8]) -> Result<(), String> {
        match *self {}
    }
}
