//! Shared-mode WASAPI output. All COM calls and releases stay on the audio thread.
use super::{LATENCY_MS, RATE};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

const FRAME_BYTES: usize = 4; // stereo, signed 16-bit little endian

struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn open() -> windows::core::Result<Self> {
        // Only constructed on a dedicated thread; every successful init is balanced.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()?; }
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize(); }
    }
}

pub(super) struct Output {
    // Fields release in declaration order, before the apartment is uninitialized.
    render: IAudioRenderClient,
    client: IAudioClient,
    frames: u32,
    started: bool,
    _apartment: Apartment,
}

impl Output {
    pub(super) fn open() -> Option<Self> {
        match Self::try_open() {
            Ok(output) => {
                log::info!("HP audio: playing through Windows WASAPI (48 kHz stereo)");
                Some(output)
            }
            Err(e) => {
                log::warn!("HP audio: WASAPI unavailable: {e}");
                None
            }
        }
    }

    fn try_open() -> windows::core::Result<Self> {
        let apartment = Apartment::open()?;
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 2,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * FRAME_BYTES as u32,
                nBlockAlign: FRAME_BYTES as u16,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            // The Windows mixer converts to the endpoint's rate/channel/sample format.
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                i64::from(LATENCY_MS) * 10_000, 0, &format, None,
            )?;
            let frames = client.GetBufferSize()?;
            let render = client.GetService::<IAudioRenderClient>()?;
            Ok(Self { render, client, frames, started: false, _apartment: apartment })
        }
    }

    pub(super) fn write(&mut self, pcm: &[u8], stop: &AtomicBool) -> bool {
        if pcm.len() % FRAME_BYTES != 0 { return false; }
        match self.write_frames(pcm, stop) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("HP audio: WASAPI playback failed: {e}");
                false
            }
        }
    }

    fn write_frames(&mut self, mut pcm: &[u8], stop: &AtomicBool) -> windows::core::Result<()> {
        let mut deadline = Instant::now() + Duration::from_millis(250);
        while !pcm.is_empty() && !stop.load(Ordering::Relaxed) {
            unsafe {
                let available = self.frames.saturating_sub(self.client.GetCurrentPadding()?);
                let count = (pcm.len() / FRAME_BYTES).min(available as usize) as u32;
                if count == 0 {
                    // Bound both shutdown latency and waits on a stalled device.
                    if Instant::now() >= deadline {
                        return Err(windows::core::Error::new(windows::core::HRESULT(0x800705B4u32 as i32), "WASAPI playback timed out"));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let bytes = count as usize * FRAME_BYTES;
                let buffer = self.render.GetBuffer(count)?;
                // GetBuffer provides writable space for exactly `count` PCM frames.
                std::ptr::copy_nonoverlapping(pcm.as_ptr(), buffer, bytes);
                self.render.ReleaseBuffer(count, 0)?;
                if !self.started {
                    self.client.Start()?;
                    self.started = true;
                }
                pcm = &pcm[bytes..];
                deadline = Instant::now() + Duration::from_millis(250);
            }
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            if self.started { let _ = self.client.Stop(); }
            let _ = self.client.Reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an active Windows audio output device"]
    fn wasapi_device_plays_silence_and_releases_cleanly() {
        // Exercise real device initialization, backpressure, cancellation and reopen.
        for _ in 0..2 {
            let mut output = Output::try_open().expect("default Windows audio output");
            let stop = AtomicBool::new(false);
            assert!(!output.write(&[0; 3], &stop), "partial frames are rejected");
            let silence = vec![0; RATE as usize * FRAME_BYTES / 5]; // exceeds the 60 ms buffer
            assert!(output.write(&silence, &stop));
            assert!(output.started);
            let before = unsafe { output.client.GetCurrentPadding().unwrap() };
            assert!(before > 0);
            std::thread::sleep(Duration::from_millis(100));
            let after = unsafe { output.client.GetCurrentPadding().unwrap() };
            assert!(after < before, "the device consumes submitted frames");
            stop.store(true, Ordering::Relaxed);
            let now = Instant::now();
            assert!(output.write(&silence, &stop));
            assert!(now.elapsed() < Duration::from_millis(100));
        }
        // The production path creates and destroys COM on its worker thread.
        let audio = crate::hp::audio::Audio::start().expect("audio worker starts");
        for _ in 0..20 {
            audio.push(vec![0; 4]);
            std::thread::sleep(Duration::from_millis(10));
        }
        let now = Instant::now();
        drop(audio);
        assert!(now.elapsed() < Duration::from_millis(500), "worker shuts down promptly");
    }
}
