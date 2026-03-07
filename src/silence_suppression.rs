// Silence Suppression for Streaming STT - Low Latency Optimized
//
// DESIGN PRINCIPLES:
// 1. Google STT requires timing continuity - never send gaps
// 2. During silence, send keepalive frames every 100ms
// 3. During speech, send ALL frames immediately with NO delay
// 4. Hangover is f.  or cost savings only, NOT for first-word accuracy
//
// LATENCY BUDGET:
// - Speech onset: 0ms delay (immediate)
// - Hangover: Only affects AFTER speech ends (no latency impact)

use std::time::{Duration, Instant};  // Added for timing

/// Configuration for silence suppression
/// Optimized for low latency with adaptive threshold
pub struct SilenceSuppressionConfig {
    /// Initial RMS threshold for speech detection (i16 scale: 0-32767)
    /// Acts as starting value; adaptive tracking adjusts this over time.
    pub speech_threshold_rms: f32,
    
    /// Duration to continue sending full audio after speech ends
    /// This does NOT add latency - only affects when we switch to keepalives
    pub speech_hangover: Duration,
    
    /// How often to send a keepalive frame during silence
    pub silence_keepalive_interval: Duration,

    /// Multiplier above the noise floor EMA to detect speech (default: 3.0)
    pub adaptive_multiplier: f32,

    /// Minimum floor for the adaptive threshold (prevents false triggers in dead silence)
    pub adaptive_min_floor: f32,

    /// EMA smoothing factor (0..1). Lower = slower adaptation. Default 0.02.
    pub ema_alpha: f32,
}

impl Default for SilenceSuppressionConfig {
    fn default() -> Self {
        Self {
            speech_threshold_rms: 100.0,  // Initial threshold (will adapt)
            speech_hangover: Duration::from_millis(200),
            silence_keepalive_interval: Duration::from_millis(100),
            adaptive_multiplier: 3.0,
            adaptive_min_floor: 20.0,
            ema_alpha: 0.02,
        }
    }
}

impl SilenceSuppressionConfig {
    /// Create config for system audio (very permissive - system audio is quieter)
    pub fn for_system_audio() -> Self {
        Self {
            speech_threshold_rms: 30.0,  // Lower starting threshold
            speech_hangover: Duration::from_millis(300),
            silence_keepalive_interval: Duration::from_millis(100),
            adaptive_multiplier: 3.0,
            adaptive_min_floor: 10.0,  // Lower floor for system audio
            ema_alpha: 0.02,
        }
    }
    
    /// Create config for microphone (standard)
    pub fn for_microphone() -> Self {
        Self {
            speech_threshold_rms: 100.0,
            speech_hangover: Duration::from_millis(200),
            silence_keepalive_interval: Duration::from_millis(100),
            adaptive_multiplier: 3.0,
            adaptive_min_floor: 20.0,
            ema_alpha: 0.02,
        }
    }
}

/// Silence suppression state machine with adaptive threshold
pub struct SilenceSuppressor {
    config: SilenceSuppressionConfig,
    state: SuppressionState,
    last_speech_time: Instant,
    last_keepalive_time: Instant,
    frames_sent: u64,
    frames_suppressed: u64,
    /// Exponential moving average of ambient noise floor RMS
    noise_floor_ema: f32,
    /// Current adaptive speech threshold
    adaptive_threshold: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SuppressionState {
    Active,     // Speech detected, send everything
    Hangover,   // Speech ended recently, still sending
    Suppressed, // Confirmed silence, send keepalives only
}

/// Result of processing a frame
#[derive(Debug, Clone)]
pub enum FrameAction {
    /// Send this frame to STT
    Send(Vec<i16>),
    /// Replace with silence keepalive frame
    SendSilence,
    /// Suppress this frame (timing maintained by keepalives)
    Suppress,
}

impl SilenceSuppressor {
    pub fn new(config: SilenceSuppressionConfig) -> Self {
        let now = Instant::now();
        let initial_threshold = config.speech_threshold_rms;
        println!("[SilenceSuppressor] Created with threshold={} (adaptive), hangover={}ms, keepalive={}ms",
            config.speech_threshold_rms,
            config.speech_hangover.as_millis(),
            config.silence_keepalive_interval.as_millis()
        );
        Self {
            noise_floor_ema: config.adaptive_min_floor,
            adaptive_threshold: initial_threshold,
            config,
            state: SuppressionState::Active, // Start in active to not miss first words
            last_speech_time: now,
            last_keepalive_time: now,
            frames_sent: 0,
            frames_suppressed: 0,
        }
    }
    
    /// Process a frame and determine what to do with it
    /// CRITICAL: Speech frames are NEVER delayed
    pub fn process(&mut self, frame: &[i16]) -> FrameAction {
        let now = Instant::now();
        let rms = calculate_rms(frame);
        let has_speech = rms >= self.adaptive_threshold;
        
        // ALWAYS check for speech first - immediate response
        if has_speech {
            self.state = SuppressionState::Active;
            self.last_speech_time = now;
            self.frames_sent += 1;
            return FrameAction::Send(frame.to_vec());
        }
        
        // No speech detected - check state
        match self.state {
            SuppressionState::Active | SuppressionState::Hangover => {
                // Check if hangover period has elapsed
                if now.duration_since(self.last_speech_time) > self.config.speech_hangover {
                    self.state = SuppressionState::Suppressed;
                    // Fall through to check keepalive
                } else {
                    // Still in hangover - send full frame
                    self.state = SuppressionState::Hangover;
                    self.frames_sent += 1;
                    return FrameAction::Send(frame.to_vec());
                }
            }
            SuppressionState::Suppressed => {
                // Already suppressed
            }
        }
        
        // In suppressed state - update adaptive noise floor EMA
        // Only adapt during confirmed silence to avoid tracking speech levels
        let alpha = self.config.ema_alpha;
        self.noise_floor_ema = self.noise_floor_ema * (1.0 - alpha) + rms * alpha;
        self.adaptive_threshold = (self.noise_floor_ema * self.config.adaptive_multiplier)
            .max(self.config.adaptive_min_floor);

        // Check if time for keepalive
        if now.duration_since(self.last_keepalive_time) >= self.config.silence_keepalive_interval {
            self.last_keepalive_time = now;
            self.frames_sent += 1;
            FrameAction::SendSilence
        } else {
            self.frames_suppressed += 1;
            FrameAction::Suppress
        }
    }
    
    /// Get statistics
    pub fn stats(&self) -> (u64, u64) {
        (self.frames_sent, self.frames_suppressed)
    }
    
    /// Get current state for UI
    pub fn is_speech(&self) -> bool {
        matches!(self.state, SuppressionState::Active | SuppressionState::Hangover)
    }
    
    /// Reset state (e.g., when meeting ends)
    pub fn reset(&mut self) {
        let now = Instant::now();
        self.state = SuppressionState::Active;
        self.last_speech_time = now;
        self.last_keepalive_time = now;
        self.noise_floor_ema = self.config.adaptive_min_floor;
        self.adaptive_threshold = self.config.speech_threshold_rms;
    }
}

/// Calculate RMS of i16 samples efficiently
fn calculate_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    
    // Sample every 4th sample for speed (320/4 = 80 samples is plenty for RMS)
    let sum_of_squares: f64 = samples.iter()
        .step_by(4)
        .map(|&s| (s as f64) * (s as f64))
        .sum();
    
    let count = (samples.len() + 3) / 4;
    (sum_of_squares / count as f64).sqrt() as f32
}

/// Generate a silence frame of given size
pub fn generate_silence_frame(size: usize) -> Vec<i16> {
    vec![0i16; size]
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_speech_immediate() {
        let mut suppressor = SilenceSuppressor::new(SilenceSuppressionConfig::default());
        
        // Loud frame should be sent immediately
        let loud_frame: Vec<i16> = vec![500; 320];
        match suppressor.process(&loud_frame) {
            FrameAction::Send(_) => {}
            _ => panic!("Loud frame should be sent immediately"),
        }
        assert!(suppressor.is_speech());
    }
    
    #[test]
    fn test_silence_keepalive() {
        let mut suppressor = SilenceSuppressor::new(SilenceSuppressionConfig {
            speech_threshold_rms: 100.0,
            speech_hangover: Duration::from_millis(0),
            silence_keepalive_interval: Duration::from_millis(50),
            adaptive_multiplier: 3.0,
            adaptive_min_floor: 20.0,
            ema_alpha: 0.02,
        });
        
        let silent_frame: Vec<i16> = vec![0; 320];
        let action = suppressor.process(&silent_frame);
        assert!(matches!(action, FrameAction::SendSilence | FrameAction::Suppress));
    }
}
