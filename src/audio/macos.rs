use anyhow::{Context, Result, anyhow};
use crossbeam_channel::Sender;
use screencapturekit::AudioBufferList;
use screencapturekit::prelude::*;

pub struct SystemAudioCapture {
    stream: SCStream,
}

impl SystemAudioCapture {
    pub fn start(sender: Sender<Vec<f32>>, sample_rate: u32, channels: u32) -> Result<Self> {
        let content = SCShareableContent::get().map_err(|error| {
            anyhow!(
                "failed to access shareable macOS content: {error}. Grant Screen & System Audio Recording permission to your terminal or Light Wizard"
            )
        })?;
        let display = content
            .displays()
            .into_iter()
            .next()
            .context("macOS did not report a display for system-audio capture")?;
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        // ScreenCaptureKit exposes system audio through an SCStream. No screen
        // handler is installed; the tiny video dimensions minimize incidental
        // work while the audio output remains full quality.
        let configuration = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_sample_rate(sample_rate as i32)
            .with_channel_count(channels as i32);

        let mut stream = SCStream::new(&filter, &configuration);
        stream.add_output_handler(AudioHandler { sender }, SCStreamOutputType::Audio);
        stream.start_capture().map_err(|error| {
            anyhow!(
                "failed to start macOS system-audio capture: {error}. Open System Settings > Privacy & Security > Screen & System Audio Recording, enable your terminal or Light Wizard, and restart it"
            )
        })?;
        Ok(Self { stream })
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}

struct AudioHandler {
    sender: Sender<Vec<f32>>,
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if output_type != SCStreamOutputType::Audio || !sample.data_is_ready() {
            return;
        }
        let Some(buffers) = sample.audio_buffer_list() else {
            return;
        };
        let mono = downmix(&buffers);
        if mono.is_empty() {
            return;
        }
        // A full queue means the analyzer is behind. Never block an Apple
        // capture callback; a fresh buffer will arrive shortly.
        let _ = self.sender.try_send(mono);
    }
}

fn downmix(buffers: &AudioBufferList) -> Vec<f32> {
    if buffers.num_buffers() == 0 {
        return Vec::new();
    }

    if buffers.num_buffers() == 1 {
        let Some(buffer) = buffers.get(0) else {
            return Vec::new();
        };
        let channels = buffer.number_channels.max(1) as usize;
        let samples = f32_samples(buffer.data());
        let mut mono = Vec::with_capacity(samples.len() / channels);
        for frame in samples.chunks_exact(channels) {
            let value = frame.iter().copied().sum::<f32>() / channels as f32;
            mono.push(sanitize(value));
        }
        return mono;
    }

    // Non-interleaved PCM normally contains one buffer per channel.
    let channel_samples: Vec<Vec<f32>> = buffers
        .iter()
        .map(|buffer| f32_samples(buffer.data()))
        .filter(|samples| !samples.is_empty())
        .collect();
    let Some(frame_count) = channel_samples.iter().map(Vec::len).min() else {
        return Vec::new();
    };
    let mut mono = Vec::with_capacity(frame_count);
    for index in 0..frame_count {
        let value = channel_samples
            .iter()
            .map(|channel| channel[index])
            .sum::<f32>()
            / channel_samples.len() as f32;
        mono.push(sanitize(value));
    }
    mono
}

fn f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn sanitize(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_native_float_samples() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.25_f32.to_ne_bytes());
        bytes.extend_from_slice(&(-0.5_f32).to_ne_bytes());
        assert_eq!(f32_samples(&bytes), vec![0.25, -0.5]);
    }

    #[test]
    fn sanitizes_invalid_samples() {
        assert_eq!(sanitize(f32::NAN), 0.0);
        assert_eq!(sanitize(3.0), 1.0);
    }
}
