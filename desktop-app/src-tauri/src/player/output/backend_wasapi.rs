use super::*;
use wasapi::{
    calculate_period_100ns, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
    WasapiError,
};

// AUDCLNT error HRESULTs (from Windows SDK audioclient.h / mmreg.h).
const HRESULT_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x88890019u32 as i32;
const HRESULT_DEVICE_IN_USE: i32 = 0x8889000Au32 as i32;
const HRESULT_EXCLUSIVE_NOT_ALLOWED: i32 = 0x8889000Eu32 as i32;

/// The AUDCLNT codes a render endpoint actually answers with, by name.
fn hresult_name(code: i32) -> Option<&'static str> {
    Some(match code as u32 {
        0x88890001 => "AUDCLNT_E_NOT_INITIALIZED",
        0x88890002 => "AUDCLNT_E_ALREADY_INITIALIZED",
        0x88890004 => "AUDCLNT_E_DEVICE_INVALIDATED",
        0x88890008 => "AUDCLNT_E_UNSUPPORTED_FORMAT",
        0x8889000A => "AUDCLNT_E_DEVICE_IN_USE",
        0x8889000E => "AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED",
        0x8889000F => "AUDCLNT_E_ENDPOINT_CREATE_FAILED",
        0x88890010 => "AUDCLNT_E_SERVICE_NOT_RUNNING",
        0x88890016 => "AUDCLNT_E_BUFFER_SIZE_ERROR",
        0x88890017 => "AUDCLNT_E_CPUUSAGE_EXCEEDED",
        0x88890019 => "AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED",
        0x88890020 => "AUDCLNT_E_INVALID_DEVICE_PERIOD",
        0x80070005 => "E_ACCESSDENIED",
        0x8007000E => "E_OUTOFMEMORY",
        0x80070057 => "E_INVALIDARG",
        _ => return None,
    })
}

/// An error as the endpoint said it: the AUDCLNT name when there is one,
/// the hex code otherwise, never just the crate's paraphrase.
fn describe(e: &WasapiError) -> String {
    match wasapi_hresult(e) {
        Some(c) => match hresult_name(c) {
            Some(n) => n.to_string(),
            None => format!("0x{:08X}", c as u32),
        },
        None => e.to_string(),
    }
}

// ── Format helpers ───────────────────────────────────────────────────────

/// Resolve the sample format from raw format fields.
///
/// `subformat_type` is Some for WAVEFORMATEXTENSIBLE (SubFormat is a known
/// GUID), None when SubFormat is absent or unrecognised (WAVEFORMATEX).
/// `format_tag` is the WAVEFORMATEX wFormatTag: 1=PCM, 3=IEEE_FLOAT — only
/// consulted when `subformat_type` is None.
/// `valid_bits` == 0 means "same as store_bits" (driver convention).
fn resolve_sample_fmt(
    subformat_type: Option<SampleType>,
    format_tag: u16,
    store_bits: u16,
    valid_bits: u16,
    blockalign: u32,
    channels: u16,
) -> Result<SampleFmt, String> {
    // 0 valid bits is driver shorthand for "every stored bit".
    let _valid = if valid_bits == 0 { store_bits } else { valid_bits };
    // The packers write interleaved stereo, store_bits per sample, no padding.
    if channels != 2 || blockalign != channels as u32 * store_bits as u32 / 8 {
        return Err(format!(
            "unsupported device format: {channels} channels, {store_bits} bits, \
             blockalign={blockalign} (packed stereo expected)"
        ));
    }

    let is_float = match subformat_type {
        Some(SampleType::Float) => true,
        Some(SampleType::Int) => false,
        None => {
            // WAVEFORMATEX: type is in wFormatTag only.
            match format_tag as u32 {
                1 => false, // WAVE_FORMAT_PCM
                3 => true,  // WAVE_FORMAT_IEEE_FLOAT
                _ => return Err(format!(
                    "unsupported device format: unknown wFormatTag={format_tag} \
                     bits={store_bits}"
                )),
            }
        }
    };

    if is_float {
        return if store_bits == 32 {
            Ok(SampleFmt::Float32)
        } else {
            Err(format!("unsupported device format: float{store_bits}"))
        };
    }

    match store_bits {
        16 => Ok(SampleFmt::Int16),
        24 => {
            let expected_ba = 3 * channels as u32;
            if blockalign == expected_ba {
                Ok(SampleFmt::Int24Packed)
            } else {
                Err(format!(
                    "unsupported device format: PCM 24-bit blockalign={blockalign} \
                     for {channels} channels (expected {expected_ba})"
                ))
            }
        }
        32 => Ok(SampleFmt::Int32),
        _ => Err(format!(
            "unsupported device format: int{store_bits}"
        )),
    }
}

impl SampleFmt {
    /// Determine the sample format from a WaveFormat returned by the driver.
    ///
    /// For WAVEFORMATEXTENSIBLE the SubFormat GUID names the type.
    /// For WAVEFORMATEX (returned via the quirks path: SubFormat zeroed,
    /// wValidBitsPerSample = 0) the type is in wFormatTag instead.
    fn from_wave(fmt: &WaveFormat) -> Result<Self, String> {
        resolve_sample_fmt(
            fmt.get_subformat().ok(),
            fmt.as_waveformatex_ref().wFormatTag,
            fmt.get_bitspersample(),
            fmt.get_validbitspersample(),
            fmt.get_blockalign(),
            fmt.get_nchannels(),
        )
    }
}

fn format_label(fmt: &WaveFormat) -> String {
    let store = fmt.get_bitspersample();
    // Some drivers (Realtek among them) leave wValidBitsPerSample at 0,
    // which means "every stored bit": 0 of 32 is plain int32.
    let valid = if fmt.get_validbitspersample() == 0 {
        store
    } else {
        fmt.get_validbitspersample()
    };
    match fmt.get_subformat() {
        Ok(SampleType::Float) => "float32".into(),
        Ok(SampleType::Int) => match (store, valid) {
            (16, 16) => "int16".into(),
            (24, 24) => "int24 (packed)".into(),
            (32, 24) => "int32 (24-in-32)".into(),
            (32, 32) => "int32".into(),
            (s, v) => format!("int{v} (in {s})"),
        },
        Err(_) => {
            // WAVEFORMATEX: SubFormat is absent; type lives in wFormatTag.
            let tag = fmt.as_waveformatex_ref().wFormatTag;
            match tag as u32 {
                1 /* PCM */ => match (store, valid) {
                    (16, 16) => "int16".into(),
                    (24, 24) => "int24 (packed)".into(),
                    (32, 32) => "int32".into(),
                    (s, v) => format!("int{v} (in {s})"),
                },
                3 /* IEEE_FLOAT */ => "float32".into(),
                _ => format!("unknown (tag={tag} bits={store})"),
            }
        }
    }
}

/// The WASAPI format a stream asks for (`super::FmtSpec`).
fn wave(spec: &FmtSpec, rate: u32) -> WaveFormat {
    let sample_type = if spec.float { SampleType::Float } else { SampleType::Int };
    WaveFormat::new(spec.store as usize, spec.valid as usize, &sample_type, rate as usize, 2, None)
}

fn waves(specs: &[FmtSpec], rate: u32) -> Vec<WaveFormat> {
    specs.iter().map(|s| wave(s, rate)).collect()
}

/// DSP-mode format priority (`super::dsp_formats`).
fn dsp_candidates(rate: u32) -> Vec<WaveFormat> {
    waves(super::dsp_formats(), rate)
}

/// DoP's format priority (`super::dop_formats`).
#[cfg(test)]
fn dop_candidates(rate: u32) -> Vec<WaveFormat> {
    waves(super::dop_formats(), rate)
}

/// Direct-mode format priority by source bit depth (`super::direct_formats`).
#[cfg(test)]
fn direct_candidates(rate: u32, source_bits: u32) -> Vec<WaveFormat> {
    waves(super::direct_formats(source_bits), rate)
}

fn wasapi_hresult(e: &WasapiError) -> Option<i32> {
    if let WasapiError::Windows(ref we) = e {
        Some(we.code().0)
    } else {
        None
    }
}

fn exclusive_err_msg(e: &WasapiError, rate: u32, device: &str) -> String {
    exclusive_err_text(wasapi_hresult(e), rate, device, || describe(e))
}

/// What the listener is told when exclusive mode fails: the HRESULT, if
/// the driver gave one, in plain words; `described` for any other.
fn exclusive_err_text(code: Option<i32>, rate: u32, device: &str, described: impl FnOnce() -> String) -> String {
    match code {
        Some(c) if c == HRESULT_DEVICE_IN_USE => format!(
            "{device} is busy: another application is playing through it \
             (a browser, a messenger, another player), and exclusive mode \
             needs the device to itself. Stop the sound there and press Play. \
             [AUDCLNT_E_DEVICE_IN_USE at {rate} Hz]"
        ),
        Some(c) if c == HRESULT_EXCLUSIVE_NOT_ALLOWED => format!(
            "{device} does not allow exclusive mode. Windows Sound settings → \
             {device} → Properties → Advanced → tick \"Allow applications to \
             take exclusive control of this device\". \
             [AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED at {rate} Hz]"
        ),
        Some(c) if c as u32 == 0x88890008 => format!(
            "{device} does not accept {rate} Hz in exclusive mode \
             (tried int24-in-32, int32, float32). Pick a lower FS multiplier. \
             [AUDCLNT_E_UNSUPPORTED_FORMAT]"
        ),
        Some(c) if c as u32 == 0x8889000F => format!(
            "{device} would not open at {rate} Hz: another program holds it \
             (an ASIO host), or the rate or the clock is locked in its \
             driver's settings. Free it and press Play. \
             [AUDCLNT_E_ENDPOINT_CREATE_FAILED at {rate} Hz]"
        ),
        _ => format!("{device}: exclusive mode failed at {rate} Hz — {}", described()),
    }
}

// ── Device enumeration ───────────────────────────────────────────────────

pub fn list_devices_impl() -> Result<Vec<DeviceInfo>, String> {
    let _ = wasapi::initialize_mta();

    let enumerator = DeviceEnumerator::new().map_err(|e| format!("{e}"))?;
    let default_id = enumerator
        .get_default_device(&Direction::Render)
        .and_then(|d| d.get_id())
        .unwrap_or_default();

    let collection = enumerator
        .get_device_collection(&Direction::Render)
        .map_err(|e| format!("get_device_collection: {e}"))?;

    let mut out = Vec::new();
    for device_res in &collection {
        let device = match device_res {
            Ok(d) => d,
            Err(_) => continue,
        };
        let id = device.get_id().unwrap_or_default();
        let name = device.get_friendlyname().unwrap_or_else(|_| id.clone());
        let is_default = id == default_id;
        out.push(DeviceInfo { id, name, is_default });
    }
    Ok(out)
}

// ── Format negotiation ───────────────────────────────────────────────────

/// Try each candidate in order, returning the first the driver accepts in
/// exclusive mode (via `is_supported_exclusive_with_quirks`).
// Used only by probe_formats_impl; kept for the DAC capabilities panel.
#[allow(dead_code)]
fn negotiate_exclusive(
    client: &wasapi::AudioClient,
    candidates: &[WaveFormat],
) -> Option<WaveFormat> {
    for fmt in candidates {
        if let Ok(actual) = client.is_supported_exclusive_with_quirks(fmt) {
            return Some(actual);
        }
    }
    None
}

/// Initialise an AudioClient for exclusive event-driven render, handling
/// the AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED retry loop recommended by MSDN.
fn init_exclusive(
    device: &wasapi::Device,
    fmt: &WaveFormat,
    period_hns: i64,
) -> Result<(wasapi::AudioClient, u32 /* buffer_frames */), WasapiError> {
    let mode = StreamMode::EventsExclusive { period_hns };
    let mut client = device.get_iaudioclient()?;

    match client.initialize_client(fmt, &Direction::Render, &mode) {
        Ok(()) => {
            let buf = client.get_buffer_size()?;
            Ok((client, buf))
        }
        Err(ref e) if wasapi_hresult(e) == Some(HRESULT_BUFFER_SIZE_NOT_ALIGNED) => {
            // The driver wants a different buffer size. Read what it actually
            // wants, compute the matching period in 100-ns units, get a fresh
            // AudioClient (the spec requires a new one after a failed Init),
            // and retry.
            let buf = client.get_buffer_size()?;
            let aligned_period =
                calculate_period_100ns(buf as i64, fmt.get_samplespersec() as i64);
            let mode2 = StreamMode::EventsExclusive { period_hns: aligned_period };
            let mut client2 = device.get_iaudioclient()?;
            client2.initialize_client(fmt, &Direction::Render, &mode2)?;
            let buf2 = client2.get_buffer_size()?;
            Ok((client2, buf2))
        }
        Err(e) => Err(e),
    }
}

// Used only by probe_formats; kept for the DAC capabilities panel.
#[allow(dead_code)]
pub fn probe_formats_impl(
    device_id: Option<&str>,
    rates: &[u32],
) -> Result<Vec<(u32, String)>, String> {
    let _ = wasapi::initialize_mta();

    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = match device_id {
        Some(id) => enumerator.get_device(id).map_err(|e| e.to_string())?,
        None => enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| e.to_string())?,
    };

    // One AudioClient is enough for format queries; IsFormatSupported does
    // not change the client's initialisation state.
    let client = device.get_iaudioclient().map_err(|e| e.to_string())?;

    let mut results = Vec::with_capacity(rates.len());
    for &rate in rates {
        match negotiate_exclusive(&client, &dsp_candidates(rate)) {
            Some(fmt) => results.push((rate, format_label(&fmt))),
            None => results.push((rate, "unsupported".to_string())),
        }
    }
    Ok(results)
}

// ── Audio thread ─────────────────────────────────────────────────────────

/// Everything the audio thread needs after successful initialisation.
struct StreamHandle {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    event: wasapi::Handle,
    fmt: SampleFmt,
    blockalign: usize,
    info: StreamInfo,
}

/// Event handle, render client and one period of silence for an
/// initialised exclusive client.
fn finish_exclusive(
    client: wasapi::AudioClient,
    buf: u32,
    actual: &WaveFormat,
    device_name: String,
    rate: u32,
    mode: &OutputMode,
    (def_period, min_period): (i64, i64),
) -> Result<StreamHandle, String> {
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    let fmt = SampleFmt::from_wave(actual)?;
    let blockalign = actual.get_blockalign() as usize;
    let label = format_label(actual);
    // Log everything the driver told us at this stream open.
    {
        let store = actual.get_bitspersample();
        let valid_raw = actual.get_validbitspersample();
        let valid = if valid_raw == 0 { store } else { valid_raw };
        let ba = actual.get_blockalign();
        let ch = actual.get_nchannels();
        let tag = actual.as_waveformatex_ref().wFormatTag;
        let subfmt_str = match actual.get_subformat().ok() {
            Some(SampleType::Float) => "IEEE_FLOAT/EXTENSIBLE".to_string(),
            Some(SampleType::Int) => "PCM/EXTENSIBLE".to_string(),
            None => format!("WAVEFORMATEX(tag={tag})"),
        };
        let mode_str = super::mode_label(mode);
        let ms = |hns: i64| hns as f64 / 10_000.0;
        crate::aelog!(
            "[PLAYER] stream open: device={device_name:?} rate={rate} ch={ch} \
             subfmt={subfmt_str} bits={store} valid={valid} blockalign={ba} \
             packer={fmt:?} mode={mode_str} period={buf} frames ({:.2} ms; \
             device default {:.2} ms, min {:.2} ms)",
            buf as f64 * 1000.0 / rate.max(1) as f64,
            ms(def_period),
            ms(min_period),
        );
    }
    let info = StreamInfo {
        device_name,
        exclusive: true,
        format: label,
        rate,
        period_frames: buf,
        buffer_frames: buf,
        // Exclusive mode: two hardware periods is a conservative latency
        // estimate (one in the hardware FIFO, one being filled).
        latency_frames: buf * 2,
    };
    Ok(StreamHandle { client, render, event, fmt, blockalign, info })
}

/// What the endpoint says about itself, in its own words: the shared-mode
/// mix format, the raw answer of IsFormatSupported(EXCLUSIVE) for every
/// DSP candidate at every rate, and one real exclusive Initialize per rate
/// — the only call that reports a device another application is using.
/// Nothing is started, so nothing is heard.
pub fn diagnose_impl(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
    use std::fmt::Write;
    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = match device_id {
        Some(id) => enumerator.get_device(id).map_err(|e| e.to_string())?,
        None => enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| e.to_string())?,
    };
    let name = device.get_friendlyname().unwrap_or_else(|_| "Unknown".into());
    let mut r = String::new();
    let _ = writeln!(r, "device: {name}");
    let client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    match client.get_mixformat() {
        Ok(m) => {
            let _ = writeln!(
                r,
                "shared mix format: {} Hz, {}, {} ch",
                m.get_samplespersec(),
                format_label(&m),
                m.get_nchannels()
            );
        }
        Err(e) => {
            let _ = writeln!(r, "shared mix format: {}", describe(&e));
        }
    }
    // The period the player asks for (`exclusive_period_hns`).
    let period = client
        .get_device_period()
        .map(|(d, m)| exclusive_period_hns(d, m))
        .unwrap_or(100_000);
    for &rate in rates {
        let mut line = format!("{rate:>6} Hz  support:");
        for fmt in dsp_candidates(rate) {
            let ans = match client.is_supported(&fmt, &wasapi::ShareMode::Exclusive) {
                Ok(_) => "yes".to_string(),
                Err(e) => describe(&e),
            };
            let _ = write!(line, " {}={}", format_label(&fmt), ans);
        }
        let first = &dsp_candidates(rate)[0];
        let init = match init_exclusive(&device, first, period) {
            Ok((_c, buf)) => format!("ok ({buf} frames)"),
            Err(e) => describe(&e),
        };
        let _ = writeln!(r, "{line}  |  Initialize {}: {init}", format_label(first));
    }
    Ok(r)
}

fn open_stream(cfg: &OutputConfig) -> Result<StreamHandle, String> {
    let _ = wasapi::initialize_mta();

    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;

    let device = match &cfg.device_id {
        Some(id) => enumerator.get_device(id).map_err(|e| e.to_string())?,
        None => enumerator
            .get_default_device(&Direction::Render)
            .map_err(|e| e.to_string())?,
    };

    let device_name = device.get_friendlyname().unwrap_or_else(|_| "Unknown".into());

    // Build the format candidate list for the requested mode.
    let candidates: Vec<WaveFormat> = waves(super::formats_for(&cfg.mode), cfg.rate);

    // Try exclusive mode with each candidate in priority order.
    let mut exclusive_err: Option<WasapiError> = None;
    for desired in &candidates {
        let client_probe = match device.get_iaudioclient() {
            Ok(c) => c,
            Err(e) => {
                exclusive_err = Some(e);
                continue;
            }
        };

        let actual = match client_probe.is_supported_exclusive_with_quirks(desired) {
            Ok(f) => f,
            Err(e) => {
                exclusive_err = Some(e);
                continue;
            }
        };

        let (def_period, min_period) = match client_probe.get_device_period() {
            Ok(p) => p,
            Err(e) => {
                exclusive_err = Some(e);
                continue;
            }
        };

        // The device's default period, 10 ms at least
        // (`exclusive_period_hns`); 128-byte alignment for Intel HDA and
        // similar devices.
        let desired_period = exclusive_period_hns(def_period, min_period);
        let aligned_period = match client_probe
            .calculate_aligned_period_near(desired_period, Some(128), &actual)
        {
            Ok(p) => p,
            Err(_) => desired_period,
        };

        // is_supported_exclusive_with_quirks checked format support but did
        // not call Initialize; we need a fresh client for initialization.
        drop(client_probe);

        match init_exclusive(&device, &actual, aligned_period) {
            Ok((client, buf)) => {
                return finish_exclusive(
                    client, buf, &actual, device_name, cfg.rate, &cfg.mode, (def_period, min_period),
                );
            }
            Err(e) => {
                exclusive_err = Some(e);
            }
        }
    }

    // IsFormatSupported said no to every candidate, and it says no without
    // saying why: a device another application is playing through answers
    // exactly like a device that cannot do the rate. Initialize is the
    // call that knows the difference — and the one that decides — so it
    // gets the last word, on the first candidate.
    let ambiguous = |e: &WasapiError| match wasapi_hresult(e) {
        None => true,                              // the crate's own "no compatible format"
        Some(c) => c as u32 == 0x88890008,         // AUDCLNT_E_UNSUPPORTED_FORMAT
    };
    if exclusive_err.as_ref().map_or(true, ambiguous) {
        if let Some(first) = candidates.first() {
            let periods = device.get_iaudioclient().and_then(|c| c.get_device_period());
            match periods.and_then(|(def, min)| {
                init_exclusive(&device, first, exclusive_period_hns(def, min)).map(|(c, buf)| (c, buf, (def, min)))
            }) {
                Ok((client, buf, periods)) => {
                    crate::aelog!(
                        "[PLAYER] {} refused every format in IsFormatSupported but accepted {} at Initialize",
                        device_name,
                        format_label(first)
                    );
                    return finish_exclusive(
                        client, buf, first, device_name, cfg.rate, &cfg.mode, periods,
                    );
                }
                Err(e) => exclusive_err = Some(e),
            }
        }
    }

    // All exclusive attempts failed. DoP never goes through the Windows
    // mixer: it would resample and mix the DoP words into noise.
    let dsd = cfg.mode.is_dop();
    if !cfg.allow_shared || dsd {
        let msg = exclusive_err
            .as_ref()
            .map(|e| exclusive_err_msg(e, cfg.rate, &device_name))
            .unwrap_or_else(|| format!("No format accepted by the device at {} Hz", cfg.rate));
        // A device that takes the carrier rate in no 24-bit integer
        // format: the controller plays PCM instead.
        let unsupported = exclusive_err.as_ref().map_or(true, |e| match wasapi_hresult(e) {
            None => true,
            Some(c) => c as u32 == 0x88890008, // AUDCLNT_E_UNSUPPORTED_FORMAT
        });
        if dsd && unsupported {
            return Err(format!("{DOP_FORMAT_ERR} at {} Hz on {device_name} ({msg})", cfg.rate));
        }
        return Err(msg);
    }

    // ── Shared-mode fallback ─────────────────────────────────────────────
    // Windows mixer resamples and converts; this path is not bit-perfect.
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let shared_fmt =
        WaveFormat::new(32, 32, &SampleType::Float, cfg.rate as usize, 2, None);
    let (def_period, _) = client.get_device_period().map_err(|e| e.to_string())?;
    let shared_stream_mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_period * 4,
    };
    client
        .initialize_client(&shared_fmt, &Direction::Render, &shared_stream_mode)
        .map_err(|e| format!("Shared fallback failed: {e}"))?;

    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    let buf = client.get_buffer_size().map_err(|e| e.to_string())?;
    let fmt = SampleFmt::Float32;
    let blockalign = shared_fmt.get_blockalign() as usize;
    let label = format_label(&shared_fmt);

    {
        let mode_str = match &cfg.mode {
            OutputMode::Dsp => "DSP (shared)".to_string(),
            OutputMode::Direct { source_bits: 0 } => "Direct(no integer depth)/shared".to_string(),
            OutputMode::Direct { source_bits } => {
                format!("Direct({}bit)/shared", source_bits)
            }
            OutputMode::Dsd { .. } | OutputMode::DsdDirect { .. } => unreachable!("DoP never opens in shared mode"),
        };
        crate::aelog!(
            "[PLAYER] stream open (shared): device={device_name:?} rate={} ch=2 \
             subfmt=IEEE_FLOAT/WAVEFORMATEXTENSIBLE bits=32 valid=32 blockalign={} \
             packer=Float32 mode={mode_str}",
            cfg.rate,
            blockalign,
        );
    }

    let info = StreamInfo {
        device_name,
        exclusive: false,
        format: label,
        rate: cfg.rate,
        period_frames: buf,
        buffer_frames: buf,
        // Shared mode: use the hardware buffer size as a latency estimate
        // (the Windows mixer adds at least one buffer period).
        latency_frames: buf,
    };
    Ok(StreamHandle { client, render, event, fmt, blockalign, info })
}

/// WASAPI as the output's backend (`super::Backend`).
pub(super) struct WasapiOut;

impl super::Backend for WasapiOut {
    type Stream = WasapiStream;

    fn list_devices() -> Result<Vec<DeviceInfo>, String> {
        list_devices_impl()
    }

    fn probe_formats(device_id: Option<&str>, rates: &[u32]) -> Result<Vec<(u32, String)>, String> {
        probe_formats_impl(device_id, rates)
    }

    fn diagnose(device_id: Option<&str>, rates: &[u32]) -> Result<String, String> {
        diagnose_impl(device_id, rates)
    }

    /// Initialises COM and opens the stream (`open_stream`).
    fn open(cfg: &OutputConfig) -> Result<Opened<WasapiStream>, String> {
        let StreamHandle { client, render, event, fmt, blockalign, info } = open_stream(cfg)?;
        let stream = WasapiStream { client, render, event, rate: info.rate, buffer_frames: info.buffer_frames };
        Ok(Opened { stream, fmt, blockalign, info })
    }
}

/// An initialised WASAPI client, not started.
pub(super) struct WasapiStream {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    event: wasapi::Handle,
    rate: u32,
    buffer_frames: u32,
}

impl super::Stream for WasapiStream {
    type Dev<'a> = Wasapi<'a>;

    fn prefill(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        self.render.write_to_device(frames, bytes, None).map_err(|e| e.to_string())
    }

    fn start<'a>(&'a mut self, shared: &'a OutputShared) -> Result<Wasapi<'a>, String> {
        self.client.start_stream().map_err(|e| format!("start_stream: {e}"))?;
        // The scratch buffers are sized to the hardware period (`Periods`):
        // no allocation happens inside the loop.
        let buf_frames = self.buffer_frames as usize;
        Ok(Wasapi {
            client: &self.client,
            render: &self.render,
            event: &self.event,
            shared,
            clock: self.client.get_audioclock().ok()
                .and_then(|c| c.get_frequency().ok().filter(|&f| f > 0).map(|f| (c, f))),
            rate: self.rate,
            written: self.buffer_frames as u64, // the silence pre-fill
            probe_every: ((self.rate as usize / 4) / buf_frames.max(1)).max(1),
            since_probe: 0,
            lat_ema: 0.0,
        })
    }

    fn stop(&mut self) {
        self.client.stop_stream().ok();
    }
}

/// The WASAPI render client as the render loop's device (`run`), the
/// output latency measured as it goes: frames handed to WASAPI minus the
/// device position (IAudioClock), every ~250 ms, smoothed. It replaces
/// the estimate for the spectrum and the analyzer. What the driver does
/// not report (the DAC's own filter, the speakers) is left to the page's
/// per-device offset.
pub(super) struct Wasapi<'a> {
    client: &'a wasapi::AudioClient,
    render: &'a wasapi::AudioRenderClient,
    event: &'a wasapi::Handle,
    shared: &'a OutputShared,
    clock: Option<(wasapi::AudioClock, u64)>,
    rate: u32,
    /// Frames handed to WASAPI.
    written: u64,
    probe_every: usize,
    since_probe: usize,
    lat_ema: f64,
}

impl super::Device for Wasapi<'_> {
    fn wait(&mut self) -> bool {
        self.event.wait_for_event(100).is_ok()
    }

    fn space(&mut self) -> Result<usize, String> {
        self.client.get_available_space_in_frames().map(|n| n as usize).map_err(|e| e.to_string())
    }

    fn write(&mut self, frames: usize, bytes: &[u8]) -> Result<(), String> {
        self.since_probe += 1;
        if self.since_probe >= self.probe_every {
            self.since_probe = 0;
            if let Some((clk, freq)) = &self.clock {
                if let Ok((pos, _)) = clk.get_position() {
                    let played = (pos as u128 * self.rate as u128 / *freq as u128) as u64;
                    if self.written > played {
                        let lat = (self.written - played) as f64;
                        self.lat_ema = if self.lat_ema == 0.0 { lat } else { self.lat_ema * 0.8 + lat * 0.2 };
                        self.shared.device_latency_frames.store(self.lat_ema.round() as u64, Ordering::Relaxed);
                    }
                }
            }
        }
        // Every period writes exactly `frames` (or stops).
        self.written += frames as u64;
        self.render.write_to_device(frames, bytes, None).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod err_text_tests {
    use super::exclusive_err_text;

    /// ENDPOINT_CREATE_FAILED says what holds the device: an RME kept
    /// by its ASIO host at 44.1 kHz refused ×4 with the bare code (ASR #139).
    #[test]
    fn endpoint_create_failed_says_what_holds_the_device() {
        let m = exclusive_err_text(Some(0x8889000Fu32 as i32), 176_400, "ADI-2/4 Pro SE", || unreachable!());
        assert!(m.starts_with("ADI-2/4 Pro SE would not open at 176400 Hz"), "{m}");
        assert!(m.contains("ASIO host") && m.contains("driver's settings") && m.contains("press Play"), "{m}");
        assert!(m.ends_with("[AUDCLNT_E_ENDPOINT_CREATE_FAILED at 176400 Hz]"), "{m}");
        // Any other code: the driver's words.
        let o = exclusive_err_text(Some(0x88890001u32 as i32), 44_100, "DAC", || "AUDCLNT_E_NOT_INITIALIZED".into());
        assert_eq!(o, "DAC: exclusive mode failed at 44100 Hz — AUDCLNT_E_NOT_INITIALIZED");
    }
}

#[cfg(test)]
mod format_label_tests {
    use super::format_label;
    use wasapi::{SampleType, WaveFormat};

    fn int(store: usize, valid: usize) -> WaveFormat {
        WaveFormat::new(store, valid, &SampleType::Int, 44_100, 2, None)
    }

    #[test]
    fn labels_every_format_the_player_opens() {
        assert_eq!(format_label(&int(16, 16)), "int16");
        assert_eq!(format_label(&int(24, 24)), "int24 (packed)");
        assert_eq!(format_label(&int(32, 24)), "int32 (24-in-32)");
        assert_eq!(format_label(&int(32, 32)), "int32");
        let float = WaveFormat::new(32, 32, &SampleType::Float, 44_100, 2, None);
        assert_eq!(format_label(&float), "float32");
    }

    /// wValidBitsPerSample = 0 (Realtek's answer) means every stored bit,
    /// not "int0 (in 32)".
    #[test]
    fn zero_valid_bits_means_all_of_them() {
        assert_eq!(format_label(&int(32, 0)), "int32");
        assert_eq!(format_label(&int(16, 0)), "int16");
        assert_eq!(format_label(&int(24, 0)), "int24 (packed)");
        let float = WaveFormat::new(32, 0, &SampleType::Float, 44_100, 2, None);
        assert_eq!(format_label(&float), "float32");
    }
}

// ── from_wave / resolve_sample_fmt tests ─────────────────────────────────

#[cfg(test)]
mod from_wave_tests {
    use super::{dsp_candidates, direct_candidates, resolve_sample_fmt, SampleFmt};
    use SampleFmt::*;
    use wasapi::{SampleType, WaveFormat};

    fn wave_from(fmt: &WaveFormat) -> Result<SampleFmt, String> {
        resolve_sample_fmt(
            fmt.get_subformat().ok(),
            fmt.as_waveformatex_ref().wFormatTag,
            fmt.get_bitspersample(),
            fmt.get_validbitspersample(),
            fmt.get_blockalign(),
            fmt.get_nchannels(),
        )
    }

    /// DSP float32 candidate converted to WAVEFORMATEX must resolve to Float32.
    #[test]
    fn waveformatex_float32_dsp_candidate() {
        let float_dsp = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let wfex = float_dsp.to_waveformatex().unwrap();
        assert_eq!(wave_from(&wfex), Ok(Float32));
    }

    /// WAVEFORMATEX PCM formats map to the correct packers.
    #[test]
    fn waveformatex_pcm_formats() {
        let pcm16 = WaveFormat::new(16, 16, &SampleType::Int, 44_100, 2, None);
        assert_eq!(wave_from(&pcm16.to_waveformatex().unwrap()), Ok(Int16));

        let pcm24 = WaveFormat::new(24, 24, &SampleType::Int, 44_100, 2, None);
        assert_eq!(wave_from(&pcm24.to_waveformatex().unwrap()), Ok(Int24Packed));

        let pcm32 = WaveFormat::new(32, 32, &SampleType::Int, 44_100, 2, None);
        assert_eq!(wave_from(&pcm32.to_waveformatex().unwrap()), Ok(Int32));
    }

    /// An unknown wFormatTag (neither 1=PCM nor 3=IEEE_FLOAT) must be an error.
    #[test]
    fn unknown_tag_is_error() {
        let result = resolve_sample_fmt(None, 7, 8, 8, 2, 2);
        assert!(result.is_err(), "expected Err for unknown tag, got Ok");
        let msg = result.unwrap_err();
        assert!(msg.contains("unknown wFormatTag"), "message: {msg}");
    }

    /// Every DSP candidate must resolve to a packer whose bytes_per_frame
    /// matches the WaveFormat's blockalign.
    #[test]
    fn dsp_candidates_packer_blockalign_consistent() {
        for fmt in dsp_candidates(48_000) {
            let sf = wave_from(&fmt).expect("DSP candidate must be supported");
            let expected_ba: u32 = match sf {
                Int16 => 4,
                Int24Packed => 6,
                Int32 | Float32 => 8,
            };
            assert_eq!(
                fmt.get_blockalign(), expected_ba,
                "DSP candidate {sf:?}: blockalign {} != expected {}",
                fmt.get_blockalign(), expected_ba
            );
        }
    }

    /// Every Direct-mode candidate must also resolve consistently.
    #[test]
    fn direct_candidates_packer_blockalign_consistent() {
        for source_bits in [0u32, 16, 24, 32] {
            for fmt in direct_candidates(48_000, source_bits) {
                let sf = wave_from(&fmt).expect("Direct candidate must be supported");
                let expected_ba: u32 = match sf {
                    Int16 => 4,
                    Int24Packed => 6,
                    Int32 | Float32 => 8,
                };
                assert_eq!(
                    fmt.get_blockalign(), expected_ba,
                    "Direct({}bit) candidate {sf:?}: blockalign {} != expected {}",
                    source_bits, fmt.get_blockalign(), expected_ba
                );
            }
        }
    }
}

// ── The shared format lists ──────────────────────────────────────────────

#[cfg(test)]
mod shared_formats_tests {
    use super::{direct_candidates, dop_candidates, dsp_candidates};
    use wasapi::{SampleType, WaveFormat};

    fn key(f: &WaveFormat) -> (u16, u16, bool, u32, u16, u32, u32) {
        let x = f.as_waveformatex_ref();
        (
            f.get_bitspersample(),
            f.get_validbitspersample(),
            matches!(f.get_subformat(), Ok(SampleType::Float)),
            f.get_samplespersec(),
            f.get_nchannels(),
            f.get_blockalign(),
            x.nAvgBytesPerSec,
        )
    }

    fn keys(v: &[WaveFormat]) -> Vec<(u16, u16, bool, u32, u16, u32, u32)> {
        v.iter().map(key).collect()
    }

    /// The shared format lists (`super::super::formats_for`) ask WASAPI for
    /// exactly the formats it was asked for before they were shared.
    #[test]
    fn the_shared_lists_ask_wasapi_for_the_same_formats() {
        let w = |s: usize, v: usize, t: &SampleType, r: u32| WaveFormat::new(s, v, t, r as usize, 2, None);
        let (i, f) = (&SampleType::Int, &SampleType::Float);
        for rate in [44_100u32, 96_000, 705_600, 768_000] {
            assert_eq!(keys(&dsp_candidates(rate)), keys(&[w(32, 24, i, rate), w(32, 32, i, rate), w(32, 32, f, rate)]));
            assert_eq!(keys(&dop_candidates(rate)), keys(&[w(32, 24, i, rate), w(24, 24, i, rate), w(32, 32, i, rate)]));
            assert_eq!(
                keys(&direct_candidates(rate, 0)),
                keys(&[w(32, 32, i, rate), w(32, 24, i, rate), w(24, 24, i, rate), w(16, 16, i, rate)])
            );
            assert_eq!(keys(&direct_candidates(rate, 16)), keys(&[w(16, 16, i, rate), w(32, 24, i, rate), w(32, 32, i, rate)]));
            assert_eq!(keys(&direct_candidates(rate, 24)), keys(&[w(32, 24, i, rate), w(24, 24, i, rate), w(32, 32, i, rate)]));
            assert_eq!(keys(&direct_candidates(rate, 32)), keys(&[w(32, 32, i, rate)]));
        }
    }
}
