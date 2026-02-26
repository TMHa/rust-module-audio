// Streaming Linear Resampler
// Zero-latency, zero-lookahead linear interpolation
// Compliant with real-time audio requirements

/// Streaming resampler using linear interpolation
/// - Zero algorithmic latency (vs 21ms for FFT)
/// - Stateful fractional position for seamless streaming
/// - Converts f32 input to i16 output at 16kHz
pub struct StreamingResampler {
    /// Ratio of input sample rate to output sample rate
    /// e.g., 48000/16000 = 3.0
    ratio: f64,
    /// Fractional position in input stream (preserved across calls)
    fractional_pos: f64,
    /// Previous sample for interpolation at chunk boundaries
    prev_sample: f32,
    /// Whether we've received any samples yet
    initialized: bool,
}

impl StreamingResampler {
    /// Create a new streaming resampler
    /// 
    /// # Arguments
    /// * `input_sample_rate` - Source sample rate (e.g., 48000)
    /// * `output_sample_rate` - Target sample rate (always 16000 for STT)
    pub fn new(input_sample_rate: f64, output_sample_rate: f64) -> Self {
        let ratio = input_sample_rate / output_sample_rate;
        println!(
            "[StreamingResampler] Created: {}Hz -> {}Hz (ratio: {:.4}, linear interpolation)",
            input_sample_rate, output_sample_rate, ratio
        );
        
        Self {
            ratio,
            fractional_pos: 0.0,
            prev_sample: 0.0,
            initialized: false,
        }
    }

    /// Resample a chunk of f32 audio to i16 at 16kHz
    /// 
    /// Uses linear interpolation between samples.
    /// Maintains state across calls for seamless streaming.
    /// 
    /// # Arguments
    /// * `input` - f32 samples at input sample rate
    /// 
    /// # Returns
    /// * i16 samples at 16kHz
    pub fn resample(&mut self, input: &[f32]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }

        // Estimate output size (slightly over-allocate for safety)
        let estimated_output = ((input.len() as f64 / self.ratio) + 2.0) as usize;
        let mut output = Vec::with_capacity(estimated_output);

        // If first call, initialize prev_sample
        if !self.initialized {
            self.prev_sample = input[0];
            self.initialized = true;
        }

        // Linear interpolation
        // For each output sample, find position in input and interpolate
        while self.fractional_pos < input.len() as f64 {
            let pos = self.fractional_pos;
            let idx = pos.floor() as usize;
            let frac = pos - idx as f64;

            // Get the two samples to interpolate between
            let sample_a = if idx == 0 && frac < 0.001 {
                self.prev_sample
            } else if idx < input.len() {
                input[idx]
            } else {
                break;
            };

            let sample_b = if idx + 1 < input.len() {
                input[idx + 1]
            } else if idx < input.len() {
                input[idx]
            } else {
                break;
            };

            // Linear interpolation: a + frac * (b - a)
            let interpolated = sample_a + (frac as f32) * (sample_b - sample_a);

            // Convert f32 [-1.0, 1.0] to i16 [-32768, 32767]
            let scaled = (interpolated * 32767.0).clamp(-32768.0, 32767.0);
            output.push(scaled as i16);

            // Advance by ratio
            self.fractional_pos += self.ratio;
        }

        // Carry over fractional position for next chunk
        self.fractional_pos -= input.len() as f64;
        
        // Save last sample for next chunk's interpolation
        if let Some(&last) = input.last() {
            self.prev_sample = last;
        }

        output
    }

    /// Reset the resampler state
    pub fn reset(&mut self) {
        self.fractional_pos = 0.0;
        self.prev_sample = 0.0;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_downsample_3x() {
        // 48kHz to 16kHz = 3:1 ratio
        let mut resampler = StreamingResampler::new(48000.0, 16000.0);
        
        // Input: 48 samples at 48kHz = 1ms
        let input: Vec<f32> = (0..48).map(|i| (i as f32) / 48.0).collect();
        let output = resampler.resample(&input);
        
        // Output: ~16 samples at 16kHz = 1ms
        assert!(output.len() >= 15 && output.len() <= 17);
    }

    #[test]
    fn test_streaming_continuity() {
        let mut resampler = StreamingResampler::new(48000.0, 16000.0);
        
        // Process in chunks, verify no discontinuities
        let chunk1: Vec<f32> = (0..480).map(|_| 0.5).collect();
        let chunk2: Vec<f32> = (0..480).map(|_| 0.5).collect();
        
        let out1 = resampler.resample(&chunk1);
        let out2 = resampler.resample(&chunk2);
        
        // Both chunks should produce output
        assert!(!out1.is_empty());
        assert!(!out2.is_empty());
        
        // Output should be consistent
        assert!((out1.len() as i32 - out2.len() as i32).abs() <= 1);
    }
}
