// Microphone Capture - Lock-Free Real-Time Compliant
// 
// Architecture:
// 1. CPAL callback: ONLY pushes to lock-free ring buffer
// 2. No mutexes, allocations, or DSP in callback
// 3. Background thread: drains buffer, resamples, emits to JS

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use ringbuf::{traits::{Producer, Consumer, Split}, HeapRb, HeapProd, HeapCons};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::audio_config::RING_BUFFER_SAMPLES;

/// List available input devices
pub fn list_input_devices() -> Result<Vec<(String, String)>> {
    let host = cpal::default_host();
    let mut list = Vec::new();
    list.push(("default".to_string(), "Default Microphone".to_string()));
    
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            if let Ok(name) = device.name() {
                list.push((name.clone(), name));
            }
        }
    }
    Ok(list)
}

/// Lock-free microphone stream
/// 
/// Callback pushes raw f32 samples to ring buffer.
/// Consumer is polled by DSP thread.
pub struct MicrophoneStream {
    stream: Option<Stream>,
    consumer: Option<HeapCons<f32>>,
    sample_rate: u32,
    is_running: Arc<AtomicBool>,
}

impl MicrophoneStream {
    pub fn new(_device_id: Option<String>) -> Result<Self> {
        let host = cpal::default_host();
        let device = host.default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No input device found"))?;
        
        let config = device.default_input_config()
            .map_err(|e| anyhow::anyhow!("Failed to get config: {}", e))?;
        
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        
        println!(
            "[Microphone] Device: {}, Rate: {}Hz, Channels: {}, Format: {:?}", 
            device.name().unwrap_or_default(), 
            sample_rate, 
            channels,
            config.sample_format()
        );
        
        // Create lock-free SPSC ring buffer
        let rb = HeapRb::<f32>::new(RING_BUFFER_SAMPLES);
        let (producer, consumer) = rb.split();
        
        let is_running = Arc::new(AtomicBool::new(false));
        let is_running_clone = is_running.clone();
        
        // Build the stream with minimal callback
        let stream = build_input_stream(
            &device, 
            &config, 
            producer, 
            channels, 
            is_running_clone
        )?;
        
        Ok(Self {
            stream: Some(stream),
            consumer: Some(consumer),
            sample_rate,
            is_running,
        })
    }

    /// Start capturing audio
    pub fn play(&self) -> Result<()> {
        if let Some(ref stream) = self.stream {
            stream.play().map_err(|e| anyhow::anyhow!("Failed to start stream: {}", e))?;
            self.is_running.store(true, Ordering::SeqCst);
            println!("[Microphone] Stream started");
        }
        Ok(())
    }

    /// Pause capturing
    pub fn pause(&self) -> Result<()> {
        if let Some(ref stream) = self.stream {
            stream.pause().map_err(|e| anyhow::anyhow!("Failed to pause stream: {}", e))?;
            self.is_running.store(false, Ordering::SeqCst);
            println!("[Microphone] Stream paused");
        }
        Ok(())
    }

    /// Get the input sample rate
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Take ownership of the consumer for the DSP thread
    pub fn take_consumer(&mut self) -> Option<HeapCons<f32>> {
        self.consumer.take()
    }
    
    /// Check if stream is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::SeqCst)
    }
}

/// Build input stream with lock-free callback
/// 
/// The callback ONLY pushes to the ring buffer.
/// No mutexes, allocations, or DSP.
fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut producer: HeapProd<f32>,
    channels: usize,
    is_running: Arc<AtomicBool>,
) -> Result<Stream> {
    let err_fn = |err| eprintln!("[Microphone] Stream error: {}", err);
    
    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream(
                &config.clone().into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if !is_running.load(Ordering::Relaxed) {
                        return;
                    }
                    // REAL-TIME SAFE: Only lock-free push
                    // Convert stereo to mono if needed, then push
                    if channels > 1 {
                        // Take first channel only (interleaved)
                        for chunk in data.chunks(channels) {
                            let _ = producer.try_push(chunk[0]);
                        }
                    } else {
                        let _ = producer.push_slice(data);
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I16 => {
            device.build_input_stream(
                &config.clone().into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    if !is_running.load(Ordering::Relaxed) {
                        return;
                    }
                    // REAL-TIME SAFE: Convert and push
                    if channels > 1 {
                        for chunk in data.chunks(channels) {
                            let sample = chunk[0] as f32 / 32768.0;
                            let _ = producer.try_push(sample);
                        }
                    } else {
                        for &sample in data {
                            let _ = producer.try_push(sample as f32 / 32768.0);
                        }
                    }
                },
                err_fn,
                None,
            )?
        }
        SampleFormat::I32 => {
            device.build_input_stream(
                &config.clone().into(),
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    if !is_running.load(Ordering::Relaxed) {
                        return;
                    }
                    // REAL-TIME SAFE: Convert and push
                    if channels > 1 {
                        for chunk in data.chunks(channels) {
                            let sample = chunk[0] as f32 / 2147483648.0;
                            let _ = producer.try_push(sample);
                        }
                    } else {
                        for &sample in data {
                            let _ = producer.try_push(sample as f32 / 2147483648.0);
                        }
                    }
                },
                err_fn,
                None,
            )?
        }
        format => {
            return Err(anyhow::anyhow!("Unsupported sample format: {:?}", format));
        }
    };
    
    Ok(stream)
}

impl Drop for MicrophoneStream {
    fn drop(&mut self) {
        self.is_running.store(false, Ordering::SeqCst);
        // Stream will be dropped and stopped automatically
    }
}
