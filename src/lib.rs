#![deny(clippy::all)]

#[macro_use]
extern crate napi_derive;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use ringbuf::traits::Consumer;

pub mod audio_config;
pub mod license;
pub mod microphone;
pub mod silence_suppression;
pub mod speaker;

use crate::audio_config::DSP_POLL_MS;
use crate::silence_suppression::{FrameAction, SilenceSuppressionConfig, SilenceSuppressor};

// ============================================================================
// HELPERS — i16 slice → LE bytes
// ============================================================================

/// Convert an i16 slice to little-endian bytes.
#[inline]
fn i16_slice_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

/// Check if all i16 samples are zero (silence detection at native sample rate).
#[inline]
fn is_all_silence(samples: &[i16]) -> bool {
    samples.iter().all(|&s| s == 0)
}

// ============================================================================
// LINEAR RESAMPLER (matches v2.0.3 StreamingResampler behavior)
// Resamples f32 native-rate audio → i16 at 16kHz output for STT
// ============================================================================

struct LinearResampler {
    ratio: f64,
    fractional_pos: f64,
    prev_sample: f32,
    initialized: bool,
}

impl LinearResampler {
    fn new(input_sample_rate: f64) -> Self {
        Self {
            ratio: input_sample_rate / 16000.0,
            fractional_pos: 0.0,
            prev_sample: 0.0,
            initialized: false,
        }
    }

    /// Resample f32 native-rate input → i16 at 16kHz output.
    /// Maintains state across calls for seamless streaming.
    fn resample(&mut self, input: &[f32]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }
        let mut output = Vec::with_capacity(((input.len() as f64 / self.ratio) + 2.0) as usize);
        if !self.initialized {
            self.prev_sample = input[0];
            self.initialized = true;
        }
        while self.fractional_pos < input.len() as f64 {
            let pos = self.fractional_pos;
            let idx = pos.floor() as usize;
            let frac = pos - idx as f64;
            let sample_a = if idx == 0 && frac < 0.001 {
                self.prev_sample
            } else if idx < input.len() {
                input[idx]
            } else {
                break;
            };
            let sample_b = if idx + 1 < input.len() {
                input[idx + 1]
            } else {
                input[input.len() - 1]
            };
            let interpolated = sample_a + (sample_b - sample_a) * (frac as f32);
            let scaled = (interpolated * 32767.0).clamp(-32768.0, 32767);
            output.push(scaled as i16);
            self.fractional_pos += self.ratio;
        }
        if self.fractional_pos >= input.len() as f64 {
            self.prev_sample = input[input.len() - 1];
            self.fractional_pos -= input.len() as f64;
        }
        output
    }
}

// ============================================================================
// SYSTEM AUDIO CAPTURE (CoreAudio Tap / ScreenCaptureKit on macOS)
// ============================================================================

#[napi]
pub struct SystemAudioCapture {
    stop_signal: Arc<AtomicBool>,
    capture_thread: Option<thread::JoinHandle<()>>,
    /// Shared atomic sample rate — updated by the background thread once the
    /// native device is initialized. Callers always get the real hardware rate.
    sample_rate: Arc<AtomicU32>,
    device_id: Option<String>,
}

#[napi]
impl SystemAudioCapture {
    #[napi(constructor)]
    pub fn new(device_id: Option<String>) -> napi::Result<Self> {
        println!("[SystemAudioCapture] Created (device: {:?})", device_id);

        Ok(SystemAudioCapture {
            stop_signal: Arc::new(AtomicBool::new(false)),
            capture_thread: None,
            // Default to 48000 until the background thread reports the real rate.
            // 48kHz is the standard macOS CoreAudio rate.
            sample_rate: Arc::new(AtomicU32::new(48000)),
            device_id,
        })
    }

    #[napi]
    pub fn get_sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Acquire)
    }

    #[napi]
    pub fn start(
        &mut self,
        callback: ThreadsafeFunction<Buffer>,
        on_speech_ended: Option<ThreadsafeFunction<bool>>,
    ) -> napi::Result<()> {
        let tsfn = callback;
        let speech_ended_tsfn = on_speech_ended;

        self.stop_signal.store(false, Ordering::SeqCst);
        let stop_signal = self.stop_signal.clone();
        let device_id = self.device_id.clone();
        let sample_rate_shared = self.sample_rate.clone();

        // ★ ALL init + DSP runs in background thread — start() returns INSTANTLY
        // This prevents the 5-7 second main-thread block from SCK initialization.
        // STRATEGY: Try CoreAudio Tap first. If it produces consecutive zero-RMS frames,
        // fall back to ScreenCaptureKit (SCK) which is more reliable on some systems.
        self.capture_thread = Some(thread::spawn(move || {
            // 1. SCK Init (takes 5-7 seconds — runs OFF main thread)
            println!("[SystemAudioCapture] Background init starting...");
            let input = match speaker::SpeakerInput::new(device_id.clone()) {
                Ok(i) => i,
                Err(e) => {
                    println!("[SystemAudioCapture] CoreAudio Tap init failed: {}. Falling back to SCK...", e);
                    match speaker::SpeakerInput::new(None) {
                        Ok(i) => i,
                        Err(e2) => {
                            eprintln!(
                                "[SystemAudioCapture] FATAL: All init attempts failed: {}",
                                e2
                            );
                            return;
                        }
                    }
                }
            };

            let mut stream = input.stream();
            let mut consumer = match stream.take_consumer() {
                Some(c) => c,
                None => {
                    eprintln!("[SystemAudioCapture] FATAL: Failed to get consumer");
                    return;
                }
            };

            let native_rate = stream.sample_rate();
            // Publish the real native rate so JS can read it via get_sample_rate()
            sample_rate_shared.store(native_rate, Ordering::Release);
            println!(
                "[SystemAudioCapture] Background init complete. Initial Rate: {}Hz. DSP starting.",
                native_rate
            );

            // 2. DSP loop with silence suppression
            // Re-add resampling from native_rate → 16kHz like v2.0.3
            let mut resampler = LinearResampler::new(native_rate as f64);
            let mut suppressor = SilenceSuppressor::new(SilenceSuppressionConfig {
                native_sample_rate: 16000, // 16kHz after resampling
                ..SilenceSuppressionConfig::for_system_audio()
            });

            // 20ms chunks at 16kHz = 320 samples
            let chunk_size = 320;
            let mut frame_buffer: Vec<i16> = Vec::with_capacity(chunk_size * 4);
            let mut raw_batch: Vec<f32> = Vec::with_capacity(4096);

            loop {
                if stop_signal.load(Ordering::Relaxed) {
                    break;
                }

                // Drain ring buffer
                while let Some(sample) = consumer.try_pop() {
                    raw_batch.push(sample);
                    if raw_batch.len() >= 480 {
                        break;
                    }
                }

                // Resample f32 native-rate → i16 at 16kHz
                if !raw_batch.is_empty() {
                    let resampled = resampler.resample(&raw_batch);
                    frame_buffer.extend(resampled);
                    raw_batch.clear();
                }

                // Process frames with Silence Suppression (RMS-only, VAD disabled)
                while frame_buffer.len() >= chunk_size {
                    let frame: Vec<i16> = frame_buffer.drain(0..chunk_size).collect();
                    match suppressor.process(&frame) {
                        FrameAction::Send(data) => {
                            tsfn.call(
                                Ok(Buffer::from(i16_slice_to_le_bytes(&data))),
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                        FrameAction::SendSilence => {
                            let silence = vec![0u8; chunk_size * 2];
                            tsfn.call(
                                Ok(Buffer::from(silence)),
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                        FrameAction::Suppress => {
                            // Bandwidth saving
                        }
                    }
                }

                // Short sleep
                if frame_buffer.len() < chunk_size {
                    thread::sleep(Duration::from_millis(DSP_POLL_MS));
                }
            }

            println!("[SystemAudioCapture] DSP thread stopped.");
            // stream is dropped here → SpeakerStream::Drop calls stop_with_ch
        }));

        Ok(())
    }

    #[napi]
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
    }
}

// ============================================================================
// MICROPHONE CAPTURE (CPAL)
//
// Design: The MicrophoneStream (CPAL handle) is recreated on every start()
// call. This guarantees the ring buffer consumer is always fresh, allowing
// seamless stop→start restart cycles (e.g. between meetings).
// ============================================================================

#[napi]
pub struct MicrophoneCapture {
    stop_signal: Arc<AtomicBool>,
    capture_thread: Option<thread::JoinHandle<()>>,
    /// Shared atomic sample rate — updated once the CPAL device is opened.
    sample_rate: Arc<AtomicU32>,
    /// Stores the requested device ID for recreation on restart.
    device_id: Option<String>,
    /// Holds the live CPAL stream. Recreated on each start().
    input: Option<microphone::MicrophoneStream>,
}

#[napi]
impl MicrophoneCapture {
    #[napi(constructor)]
    pub fn new(device_id: Option<String>) -> napi::Result<Self> {
        // Eagerly create the stream to detect device errors early and read the
        // native sample rate.
        let input = match microphone::MicrophoneStream::new(device_id.clone()) {
            Ok(i) => i,
            Err(e) => return Err(napi::Error::from_reason(format!("Failed: {}", e))),
        };

        let native_rate = input.sample_rate();
        println!(
            "[MicrophoneCapture] Initialized. Device: {:?}, Rate: {}Hz",
            device_id, native_rate
        );

        Ok(MicrophoneCapture {
            stop_signal: Arc::new(AtomicBool::new(false)),
            capture_thread: None,
            sample_rate: Arc::new(AtomicU32::new(native_rate)),
            device_id,
            input: Some(input),
        })
    }

    #[napi]
    pub fn get_sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Acquire)
    }

    #[napi]
    pub fn start(
        &mut self,
        callback: ThreadsafeFunction<Buffer>,
        on_speech_ended: Option<ThreadsafeFunction<bool>>,
    ) -> napi::Result<()> {
        let tsfn = callback;
        let speech_ended_tsfn = on_speech_ended;

        self.stop_signal.store(false, Ordering::SeqCst);
        let stop_signal = self.stop_signal.clone();

        // If the stream was consumed by a previous start() cycle, recreate it.
        // This is the fix for the one-shot take_consumer() bug.
        if self.input.is_none() {
            println!("[MicrophoneCapture] Recreating CPAL stream for restart...");
            match microphone::MicrophoneStream::new(self.device_id.clone()) {
                Ok(i) => {
                    let rate = i.sample_rate();
                    self.sample_rate.store(rate, Ordering::Release);
                    self.input = Some(i);
                }
                Err(e) => {
                    return Err(napi::Error::from_reason(format!(
                        "[MicrophoneCapture] Failed to recreate stream: {}",
                        e
                    )));
                }
            }
        }

        let input_ref = self
            .input
            .as_mut()
            .ok_or_else(|| napi::Error::from_reason("Input missing"))?;

        input_ref
            .play()
            .map_err(|e| napi::Error::from_reason(format!("{}", e)))?;

        let native_rate = input_ref.sample_rate();
        self.sample_rate.store(native_rate, Ordering::Release);

        let mut consumer = input_ref
            .take_consumer()
            .ok_or_else(|| napi::Error::from_reason("Failed to get consumer"))?;

        // DSP thread with silence suppression + WebRTC VAD
        self.capture_thread = Some(thread::spawn(move || {
            let mut suppressor = SilenceSuppressor::new(SilenceSuppressionConfig {
                native_sample_rate: native_rate,
                ..SilenceSuppressionConfig::for_microphone()
            });

            // 20ms chunks at native rate
            let chunk_size = (native_rate as usize / 1000) * 20;
            let mut frame_buffer: Vec<i16> = Vec::with_capacity(chunk_size * 4);
            let mut raw_batch: Vec<f32> = Vec::with_capacity(4096);

            println!("[MicrophoneCapture] DSP thread started (VAD + suppression active, rate={}Hz, chunk={})", native_rate, chunk_size);

            loop {
                if stop_signal.load(Ordering::Relaxed) {
                    break;
                }

                // 1. Drain ALL available samples from ring buffer (lock-free)
                while let Some(sample) = consumer.try_pop() {
                    raw_batch.push(sample);
                }

                // 2. Convert f32 -> i16 at native sample rate
                if !raw_batch.is_empty() {
                    for &f in &raw_batch {
                        let scaled = (f * 32767.0).clamp(-32768.0, 32767.0);
                        frame_buffer.push(scaled as i16);
                    }
                    raw_batch.clear();
                }

                // 3. Process in 20ms chunks through the two-stage gate
                while frame_buffer.len() >= chunk_size {
                    let frame: Vec<i16> = frame_buffer.drain(0..chunk_size).collect();

                    let (action, speech_ended) = suppressor.process(&frame);

                    match action {
                        FrameAction::Send(data) => {
                            let bytes = i16_slice_to_le_bytes(&data);
                            tsfn.call(
                                Ok(Buffer::from(bytes)),
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                        FrameAction::SendSilence => {
                            let silence = vec![0u8; chunk_size * 2];
                            tsfn.call(
                                Ok(Buffer::from(silence)),
                                ThreadsafeFunctionCallMode::NonBlocking,
                            );
                        }
                        FrameAction::Suppress => {
                            // Do nothing
                        }
                    }

                    if speech_ended {
                        if let Some(ref se_tsfn) = speech_ended_tsfn {
                            se_tsfn.call(Ok(true), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    }
                }

                // 4. Short sleep
                thread::sleep(Duration::from_millis(DSP_POLL_MS));
            }

            println!("[MicrophoneCapture] DSP thread stopped.");
        }));

        Ok(())
    }

    #[napi]
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
        // Pause and destroy the CPAL stream so start() recreates it fresh.
        if let Some(ref input) = self.input {
            let _ = input.pause();
        }
        self.input = None;
    }
}

// ============================================================================
// DEVICE ENUMERATION
// ============================================================================

#[napi(object)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
}

#[napi]
pub fn get_input_devices() -> Vec<AudioDeviceInfo> {
    match microphone::list_input_devices() {
        Ok(devs) => devs
            .into_iter()
            .map(|(id, name)| AudioDeviceInfo { id, name })
            .collect(),
        Err(e) => {
            eprintln!("[get_input_devices] Error: {}", e);
            Vec::new()
        }
    }
}

#[napi]
pub fn get_output_devices() -> Vec<AudioDeviceInfo> {
    match speaker::list_output_devices() {
        Ok(devs) => devs
            .into_iter()
            .map(|(id, name)| AudioDeviceInfo { id, name })
            .collect(),
        Err(e) => {
            eprintln!("[get_output_devices] Error: {}", e);
            Vec::new()
        }
    }
}
