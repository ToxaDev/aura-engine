//! A picture of a window's page: what its WebView shows, as PNG bytes.
//!
//! The analyzer's Screenshot saves it (the page adds a line of what was
//! measured and crops it to one graph when asked). WebView2 draws the page
//! itself (`CapturePreview`), so the picture is the page whatever covers the
//! window or wherever it stands on the screen, at the screen's pixels.

use std::time::Duration;

/// How long the WebView may take (a page of a few MB is ~50 ms).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// The PNG of `window`'s page.
#[cfg(windows)]
pub async fn capture(window: &tauri::Window) -> Result<Vec<u8>, String> {
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::IStream;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, String>>(1);
    window
        .with_webview(move |wv| {
            // On the main thread; WebView2 calls the handler back on it.
            let done = tx.clone();
            let start = move || -> windows::core::Result<()> {
                unsafe {
                    let core = wv.controller().CoreWebView2()?;
                    let stream: IStream = CreateStreamOnHGlobal(0, true)?;
                    let read = stream.clone();
                    let handler = CapturePreviewCompletedHandler::create(Box::new(move |res| {
                        let png = res.and_then(|()| read_all(&read)).map_err(|e| e.to_string());
                        let _ = done.try_send(png);
                        Ok(())
                    }));
                    core.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler)
                }
            };
            if let Err(e) = start() {
                let _ = tx.try_send(Err(e.to_string()));
            }
        })
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(CAPTURE_TIMEOUT, rx.recv()).await {
        Ok(Some(r)) => r,
        Ok(None) => Err("the window closed".into()),
        Err(_) => Err("the page did not answer".into()),
    }
}

#[cfg(not(windows))]
pub async fn capture(_window: &tauri::Window) -> Result<Vec<u8>, String> {
    Err("not on this system".into())
}

/// Everything in `s`, from its start.
#[cfg(windows)]
unsafe fn read_all(s: &windows::Win32::System::Com::IStream) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::System::Com::STREAM_SEEK_SET;
    s.Seek(0, STREAM_SEEK_SET)?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let mut n = 0u32;
        s.Read(buf.as_mut_ptr().cast(), buf.len() as u32, &mut n).ok()?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    Ok(out)
}
