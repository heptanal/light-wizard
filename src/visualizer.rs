use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

use crate::config::{ColorMode, VisualizerConfig, parse_hex_color};

pub const PITCH_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

#[derive(Debug, Clone, Copy, Default)]
pub struct AnalysisFrame {
    pub intensity: f32,
    pub dbfs: f32,
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
    pub chroma: [f32; 12],
    pub tonal_confidence: f32,
    pub onset: bool,
    pub beat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightFrame {
    pub rgb: [u8; 3],
    pub dimming: u8,
}

impl LightFrame {
    pub fn differs_from(self, other: Self, threshold: u16) -> bool {
        let color_delta = self
            .rgb
            .into_iter()
            .zip(other.rgb)
            .map(|(left, right)| left.abs_diff(right) as u16)
            .sum::<u16>();
        color_delta >= threshold || self.dimming.abs_diff(other.dimming) >= 2
    }
}

pub fn spawn_analysis_worker(
    samples: Receiver<Vec<f32>>,
    frames: Sender<AnalysisFrame>,
    config: VisualizerConfig,
    running: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut analyzer = Analyzer::new(&config);
        loop {
            match samples.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    for frame in analyzer.push(&chunk) {
                        if frames.send(frame).is_err() {
                            return;
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

struct Analyzer {
    sample_rate: f32,
    fft_size: usize,
    hop_size: usize,
    sensitivity: f32,
    floor_db: f32,
    ceiling_db: f32,
    beat_threshold: f32,
    beat_cooldown: Duration,
    pitch_smoothing: f32,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pending: Vec<f32>,
    spectrum: Vec<Complex32>,
    previous_log_magnitudes: Vec<f32>,
    onset_baseline: f32,
    onset_deviation: f32,
    onset_cooldown_windows: u64,
    last_onset_window: Option<u64>,
    long_energy: f32,
    smoothed_chroma: [f32; 12],
    windows_seen: u64,
    last_beat: Option<Instant>,
}

impl Analyzer {
    fn new(config: &VisualizerConfig) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(config.fft_size);
        let denominator = (config.fft_size - 1) as f32;
        let window = (0..config.fft_size)
            .map(|index| 0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / denominator).cos())
            .collect();
        Self {
            sample_rate: config.sample_rate as f32,
            fft_size: config.fft_size,
            hop_size: config.fft_size / 2,
            sensitivity: config.sensitivity,
            floor_db: config.floor_db,
            ceiling_db: config.ceiling_db,
            beat_threshold: config.beat_threshold,
            beat_cooldown: Duration::from_millis(config.beat_cooldown_ms),
            pitch_smoothing: 1.0
                - (-((config.fft_size / 2) as f32)
                    / config.sample_rate as f32
                    / (config.pitch_smoothing_ms / 1_000.0))
                    .exp(),
            fft,
            window,
            pending: Vec::with_capacity(config.fft_size * 2),
            spectrum: vec![Complex32::new(0.0, 0.0); config.fft_size],
            previous_log_magnitudes: vec![0.0; config.fft_size / 2],
            onset_baseline: 0.0,
            onset_deviation: 0.0,
            onset_cooldown_windows: ((0.1 * config.sample_rate as f32
                / (config.fft_size / 2) as f32)
                .ceil() as u64)
                .max(1),
            last_onset_window: None,
            long_energy: 0.0,
            smoothed_chroma: [0.0; 12],
            windows_seen: 0,
            last_beat: None,
        }
    }

    fn push(&mut self, samples: &[f32]) -> Vec<AnalysisFrame> {
        self.pending.extend_from_slice(samples);
        let mut frames = Vec::new();
        while self.pending.len() >= self.fft_size {
            let window_samples = self.pending[..self.fft_size].to_vec();
            frames.push(self.analyze(&window_samples));
            self.pending.drain(..self.hop_size);
        }
        frames
    }

    fn analyze(&mut self, samples: &[f32]) -> AnalysisFrame {
        let mean_square =
            samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
        let rms = mean_square.sqrt();
        let dbfs = 20.0 * (rms * self.sensitivity).max(1.0e-7).log10();
        let intensity =
            ((dbfs - self.floor_db) / (self.ceiling_db - self.floor_db)).clamp(0.0, 1.0);

        for ((output, sample), window) in self.spectrum.iter_mut().zip(samples).zip(&self.window) {
            *output = Complex32::new(sample * window, 0.0);
        }
        self.fft.process(&mut self.spectrum);
        let spectral_flux = self.measure_spectral_flux();

        let mut bass_power = 0.0;
        let mut mid_power = 0.0;
        let mut treble_power = 0.0;
        for (bin, value) in self.spectrum[..self.fft_size / 2]
            .iter()
            .enumerate()
            .skip(1)
        {
            let frequency = bin as f32 * self.sample_rate / self.fft_size as f32;
            let power = value.norm_sqr();
            match frequency {
                frequency if (35.0..200.0).contains(&frequency) => bass_power += power,
                frequency if (200.0..2_000.0).contains(&frequency) => mid_power += power,
                frequency if (2_000.0..=10_000.0).contains(&frequency) => treble_power += power,
                _ => {}
            }
        }
        let total_power = bass_power + mid_power + treble_power;
        let (bass, mid, treble) = if total_power > 1.0e-12 {
            (
                bass_power / total_power,
                mid_power / total_power,
                treble_power / total_power,
            )
        } else {
            (0.0, 0.0, 0.0)
        };

        let chroma = calculate_chroma(&self.spectrum, self.sample_rate, self.fft_size);
        let has_pitch_energy = chroma.iter().sum::<f32>() > 0.5 && intensity > 0.01;
        if has_pitch_energy {
            if self.windows_seen == 0 {
                self.smoothed_chroma = chroma;
            } else {
                for (smoothed, current) in self.smoothed_chroma.iter_mut().zip(chroma) {
                    *smoothed += (current - *smoothed) * self.pitch_smoothing;
                }
                normalize_chroma(&mut self.smoothed_chroma);
            }
        }
        let tonal_confidence = if has_pitch_energy {
            chroma_confidence(&self.smoothed_chroma)
        } else {
            0.0
        };

        let onset_cooldown_elapsed = self.last_onset_window.is_none_or(|last| {
            self.windows_seen.saturating_sub(last) >= self.onset_cooldown_windows
        });
        let onset_threshold = (self.onset_baseline + self.onset_deviation * 2.0).max(0.01);
        let onset = self.windows_seen > 12
            && intensity > 0.03
            && spectral_flux > onset_threshold
            && onset_cooldown_elapsed;
        if onset {
            self.last_onset_window = Some(self.windows_seen);
        }
        if self.windows_seen == 0 {
            self.onset_baseline = spectral_flux;
            self.onset_deviation = spectral_flux * 0.5;
        } else {
            let deviation = (spectral_flux - self.onset_baseline).abs();
            self.onset_baseline = self.onset_baseline * 0.95 + spectral_flux * 0.05;
            self.onset_deviation = self.onset_deviation * 0.95 + deviation * 0.05;
        }

        let now = Instant::now();
        let baseline = self.long_energy.max(1.0e-7);
        let cooldown_elapsed = self
            .last_beat
            .is_none_or(|last| now.duration_since(last) >= self.beat_cooldown);
        let beat = self.windows_seen > 12
            && intensity > 0.08
            && rms > baseline * self.beat_threshold
            && cooldown_elapsed;
        if beat {
            self.last_beat = Some(now);
        }
        self.long_energy = if self.windows_seen == 0 {
            rms
        } else {
            self.long_energy * 0.975 + rms * 0.025
        };
        self.windows_seen += 1;

        AnalysisFrame {
            intensity,
            dbfs,
            bass,
            mid,
            treble,
            chroma: self.smoothed_chroma,
            tonal_confidence,
            onset,
            beat,
        }
    }

    fn measure_spectral_flux(&mut self) -> f32 {
        let first_bin = (80.0 * self.fft_size as f32 / self.sample_rate).ceil() as usize;
        let last_bin = (5_000.0 * self.fft_size as f32 / self.sample_rate).floor() as usize;
        let last_bin = last_bin.min(self.fft_size / 2 - 1);
        if first_bin > last_bin {
            return 0.0;
        }

        let mut flux = 0.0;
        for bin in first_bin..=last_bin {
            let current = self.spectrum[bin].norm().ln_1p();
            flux += (current - self.previous_log_magnitudes[bin]).max(0.0);
            self.previous_log_magnitudes[bin] = current;
        }
        flux / (last_bin - first_bin + 1) as f32
    }
}

fn calculate_chroma(spectrum: &[Complex32], sample_rate: f32, fft_size: usize) -> [f32; 12] {
    let mut chroma = [0.0; 12];
    let magnitudes: Vec<f32> = spectrum[..fft_size / 2]
        .iter()
        .map(|value| value.norm())
        .collect();

    for bin in 2..magnitudes.len().saturating_sub(1) {
        let center = magnitudes[bin];
        let left = magnitudes[bin - 1];
        let right = magnitudes[bin + 1];
        if center <= left || center < right {
            continue;
        }

        // Quadratic peak interpolation provides useful sub-bin pitch accuracy,
        // especially with the default 2048-sample FFT.
        let curvature = left - 2.0 * center + right;
        let offset = if curvature.abs() > 1.0e-12 {
            (0.5 * (left - right) / curvature).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let frequency = (bin as f32 + offset) * sample_rate / fft_size as f32;
        if !(55.0..=5_000.0).contains(&frequency) {
            continue;
        }

        let midi_note = 69.0 + 12.0 * (frequency / 440.0).log2();
        let lower_note = midi_note.floor() as i32;
        let fraction = midi_note.fract();
        let lower_class = lower_note.rem_euclid(12) as usize;
        let upper_class = (lower_class + 1) % 12;
        // Lower spectral peaks are more likely to be fundamentals. This also
        // reduces the tendency of bright upper harmonics to steal the color.
        let weight = center * (55.0 / frequency).sqrt();
        chroma[lower_class] += weight * (1.0 - fraction);
        chroma[upper_class] += weight * fraction;
    }

    normalize_chroma(&mut chroma);
    chroma
}

fn normalize_chroma(chroma: &mut [f32; 12]) {
    let total = chroma.iter().sum::<f32>();
    if total > 1.0e-12 {
        for value in chroma {
            *value /= total;
        }
    }
}

fn chroma_confidence(chroma: &[f32; 12]) -> f32 {
    let entropy = -chroma
        .iter()
        .filter(|value| **value > 1.0e-12)
        .map(|value| value * value.ln())
        .sum::<f32>()
        / (12.0_f32).ln();
    (1.0 - entropy).clamp(0.0, 1.0)
}

pub struct VisualMapper {
    config: VisualizerConfig,
    palette: Vec<[f32; 3]>,
    started: Instant,
    last_update: Instant,
    smoothed_intensity: f32,
    beat_until: Option<Instant>,
    held_chroma: [f32; 12],
    pitch_root: Option<usize>,
    candidate_root: Option<usize>,
    candidate_frames: u32,
    chord_color_offset: usize,
}

impl VisualMapper {
    pub fn new(config: &VisualizerConfig) -> Self {
        let palette = config
            .palette
            .iter()
            .map(|color| parse_hex_color(color).expect("validated palette"))
            .map(|color| color.map(|channel| channel as f32))
            .collect();
        let now = Instant::now();
        Self {
            config: config.clone(),
            palette,
            started: now,
            last_update: now,
            smoothed_intensity: 0.0,
            beat_until: None,
            held_chroma: [0.0; 12],
            pitch_root: None,
            candidate_root: None,
            candidate_frames: 0,
            chord_color_offset: 0,
        }
    }

    pub fn render(&mut self, analysis: AnalysisFrame, light_count: usize) -> Vec<LightFrame> {
        self.render_at(analysis, light_count, Instant::now())
    }

    fn render_at(
        &mut self,
        analysis: AnalysisFrame,
        light_count: usize,
        now: Instant,
    ) -> Vec<LightFrame> {
        let delta = now.duration_since(self.last_update).as_secs_f32();
        self.last_update = now;
        if analysis.beat {
            self.beat_until = now.checked_add(Duration::from_millis(self.config.beat_duration_ms));
        }
        let target = analysis.intensity;
        let time_constant = if target > self.smoothed_intensity {
            self.config.attack_ms / 1_000.0
        } else {
            self.config.release_ms / 1_000.0
        };
        let smoothing = 1.0 - (-delta / time_constant.max(0.001)).exp();
        self.smoothed_intensity += (target - self.smoothed_intensity) * smoothing;

        let beat_active = self.beat_until.is_some_and(|until| now < until);
        if !beat_active {
            self.beat_until = None;
        }
        let output_intensity = if beat_active {
            (self.smoothed_intensity + self.config.beat_boost).min(1.0)
        } else {
            self.smoothed_intensity
        };
        let brightness_span = self.config.brightness_max - self.config.brightness_min;
        let dimming = (self.config.brightness_min as f32
            + brightness_span as f32 * output_intensity)
            .round()
            .clamp(1.0, 100.0) as u8;
        match self.config.color_mode {
            ColorMode::Pitch => self.render_pitch(analysis, light_count, dimming),
            ColorMode::Drift => self.render_drift(analysis, light_count, dimming),
        }
    }

    fn render_pitch(
        &mut self,
        analysis: AnalysisFrame,
        light_count: usize,
        dimming: u8,
    ) -> Vec<LightFrame> {
        let has_pitch_energy =
            analysis.intensity > 0.01 && analysis.chroma.iter().sum::<f32>() > 0.5;
        if has_pitch_energy {
            let confidence_response = if self.config.pitch_min_confidence <= f32::EPSILON {
                1.0
            } else {
                (analysis.tonal_confidence / self.config.pitch_min_confidence).clamp(0.0, 1.0)
            };
            let response = if analysis.onset {
                1.0
            } else {
                0.15 + confidence_response * 0.85
            };
            if self.held_chroma.iter().sum::<f32>() <= 1.0e-12 {
                self.held_chroma = analysis.chroma;
            } else {
                for (held, current) in self.held_chroma.iter_mut().zip(analysis.chroma) {
                    *held += (current - *held) * response;
                }
                normalize_chroma(&mut self.held_chroma);
            }
            self.update_pitch_root(confidence_response, analysis.onset);
        }

        if analysis.beat && self.config.rotate_colors_on_beat {
            self.chord_color_offset = self.chord_color_offset.wrapping_add(1);
        }

        let tones = select_pitch_classes(&self.held_chroma, self.pitch_root);
        if tones.is_empty() {
            return vec![
                LightFrame {
                    rgb: sample_palette(&self.palette, 0.0),
                    dimming,
                };
                light_count
            ];
        }

        if light_count == 1 {
            let colors_and_weights: Vec<_> = tones
                .iter()
                .map(|pitch| {
                    (
                        sample_palette(&self.palette, *pitch as f32 / 12.0),
                        self.held_chroma[*pitch],
                    )
                })
                .collect();
            return vec![LightFrame {
                rgb: blend_colors(&colors_and_weights),
                dimming,
            }];
        }

        (0..light_count)
            .map(|index| LightFrame {
                rgb: sample_palette(
                    &self.palette,
                    tones[(index + self.chord_color_offset) % tones.len()] as f32 / 12.0,
                ),
                dimming,
            })
            .collect()
    }

    fn update_pitch_root(&mut self, confidence_response: f32, onset: bool) {
        let Some(candidate) = strongest_pitch(&self.held_chroma) else {
            return;
        };
        let Some(current) = self.pitch_root else {
            self.pitch_root = Some(candidate);
            self.candidate_root = None;
            self.candidate_frames = 0;
            return;
        };
        if candidate == current {
            self.candidate_root = None;
            self.candidate_frames = 0;
            return;
        }

        let required_ratio = 1.02 + 0.08 * (1.0 - confidence_response);
        if self.held_chroma[candidate] < self.held_chroma[current] * required_ratio {
            self.candidate_root = None;
            self.candidate_frames = 0;
            return;
        }
        if onset {
            self.pitch_root = Some(candidate);
            self.candidate_root = None;
            self.candidate_frames = 0;
            return;
        }

        if self.candidate_root == Some(candidate) {
            self.candidate_frames += 1;
        } else {
            self.candidate_root = Some(candidate);
            self.candidate_frames = 1;
        }
        let hold_ms = (self.config.pitch_smoothing_ms * 0.5).clamp(50.0, 150.0);
        let required_frames = ((hold_ms / 1_000.0) * self.config.fps as f32)
            .ceil()
            .max(1.0) as u32;
        if self.candidate_frames >= required_frames {
            self.pitch_root = Some(candidate);
            self.candidate_root = None;
            self.candidate_frames = 0;
        }
    }

    fn render_drift(
        &self,
        analysis: AnalysisFrame,
        light_count: usize,
        dimming: u8,
    ) -> Vec<LightFrame> {
        let spectral_position = analysis.mid * 0.5 + analysis.treble;
        let base_position = self.started.elapsed().as_secs_f32() * self.config.color_speed
            + spectral_position * self.config.color_influence;
        (0..light_count)
            .map(|index| LightFrame {
                rgb: sample_palette(
                    &self.palette,
                    base_position + index as f32 * self.config.spatial_spread,
                ),
                dimming,
            })
            .collect()
    }
}

fn strongest_pitch(chroma: &[f32; 12]) -> Option<usize> {
    chroma
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.total_cmp(right.1))
        .filter(|(_, value)| **value > 1.0e-12)
        .map(|(index, _)| index)
}

pub fn select_pitch_classes(chroma: &[f32; 12], preferred: Option<usize>) -> Vec<usize> {
    let Some(strongest) = strongest_pitch(chroma) else {
        return Vec::new();
    };
    let root = preferred.unwrap_or(strongest);
    let threshold = chroma[strongest] * 0.35;
    let mut remaining: Vec<_> = (0..12)
        .filter(|pitch| *pitch != root && chroma[*pitch] >= threshold)
        .collect();
    remaining.sort_by(|left, right| chroma[*right].total_cmp(&chroma[*left]));

    let mut tones = vec![root];
    tones.extend(remaining.into_iter().take(2));
    tones
}

fn blend_colors(colors_and_weights: &[([u8; 3], f32)]) -> [u8; 3] {
    let total_weight = colors_and_weights
        .iter()
        .map(|(_, weight)| weight)
        .sum::<f32>();
    if total_weight <= 1.0e-12 {
        return [0, 0, 0];
    }
    std::array::from_fn(|channel| {
        let linear = colors_and_weights
            .iter()
            .map(|(color, weight)| srgb_to_linear(color[channel]) * weight)
            .sum::<f32>()
            / total_weight;
        linear_to_srgb(linear)
    })
}

fn srgb_to_linear(channel: u8) -> f32 {
    let value = channel as f32 / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(channel: f32) -> u8 {
    let value = if channel <= 0.003_130_8 {
        channel * 12.92
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    };
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn sample_palette(palette: &[[f32; 3]], position: f32) -> [u8; 3] {
    let wrapped = position.rem_euclid(1.0);
    let scaled = wrapped * palette.len() as f32;
    let left_index = scaled.floor() as usize % palette.len();
    let right_index = (left_index + 1) % palette.len();
    let fraction = scaled.fract();
    std::array::from_fn(|channel| {
        (palette[left_index][channel] * (1.0 - fraction) + palette[right_index][channel] * fraction)
            .round()
            .clamp(0.0, 255.0) as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_samples(config: &VisualizerConfig, frequencies: &[(f32, f32)]) -> Vec<f32> {
        (0..config.fft_size)
            .map(|index| {
                frequencies
                    .iter()
                    .map(|(frequency, amplitude)| {
                        (std::f32::consts::TAU * frequency * index as f32
                            / config.sample_rate as f32)
                            .sin()
                            * amplitude
                    })
                    .sum()
            })
            .collect()
    }

    fn pitch_frame(tones: &[(usize, f32)], confidence: f32) -> AnalysisFrame {
        let mut chroma = [0.0; 12];
        for (pitch, strength) in tones {
            chroma[*pitch] = *strength;
        }
        normalize_chroma(&mut chroma);
        AnalysisFrame {
            intensity: 0.5,
            chroma,
            tonal_confidence: confidence,
            ..AnalysisFrame::default()
        }
    }

    #[test]
    fn palette_wraps_and_interpolates() {
        let palette = vec![[0.0, 0.0, 0.0], [200.0, 100.0, 50.0]];
        assert_eq!(sample_palette(&palette, 0.0), [0, 0, 0]);
        assert_eq!(sample_palette(&palette, 0.25), [100, 50, 25]);
        assert_eq!(sample_palette(&palette, 1.0), [0, 0, 0]);
    }

    #[test]
    fn change_detection_ignores_tiny_updates() {
        let first = LightFrame {
            rgb: [100, 100, 100],
            dimming: 50,
        };
        let tiny = LightFrame {
            rgb: [101, 100, 100],
            dimming: 51,
        };
        let visible = LightFrame {
            rgb: [110, 100, 100],
            dimming: 50,
        };
        assert!(!tiny.differs_from(first, 5));
        assert!(visible.differs_from(first, 5));
    }

    #[test]
    fn fft_classifies_a_bass_tone() {
        let config = VisualizerConfig {
            fft_size: 2_048,
            ..VisualizerConfig::default()
        };
        let mut analyzer = Analyzer::new(&config);
        let samples: Vec<_> = (0..config.fft_size)
            .map(|index| {
                (std::f32::consts::TAU * 100.0 * index as f32 / config.sample_rate as f32).sin()
                    * 0.2
            })
            .collect();
        let frame = analyzer.analyze(&samples);
        assert!(frame.bass > 0.9, "bass ratio was {}", frame.bass);
        assert!(frame.intensity > 0.0);
    }

    #[test]
    fn fft_identifies_all_twelve_pitch_classes() {
        let config = VisualizerConfig {
            fft_size: 2_048,
            pitch_smoothing_ms: 10.0,
            ..VisualizerConfig::default()
        };
        for semitone in 0..12 {
            let frequency = 220.0 * 2.0_f32.powf(semitone as f32 / 12.0);
            let mut analyzer = Analyzer::new(&config);
            let frame = analyzer.analyze(&sine_samples(&config, &[(frequency, 0.2)]));
            let expected = (9 + semitone) % 12;
            assert_eq!(
                strongest_pitch(&frame.chroma),
                Some(expected),
                "{frequency:.2} Hz should be {} but chroma was {:?}",
                PITCH_NAMES[expected],
                frame.chroma
            );
            assert!(
                frame.tonal_confidence >= config.pitch_min_confidence,
                "{} confidence was {}",
                PITCH_NAMES[expected],
                frame.tonal_confidence
            );
        }
    }

    #[test]
    fn pitch_class_is_octave_invariant() {
        let config = VisualizerConfig {
            fft_size: 8_192,
            ..VisualizerConfig::default()
        };
        let mut low_analyzer = Analyzer::new(&config);
        let low = low_analyzer.analyze(&sine_samples(&config, &[(220.0, 0.2)]));
        let mut high_analyzer = Analyzer::new(&config);
        let high = high_analyzer.analyze(&sine_samples(&config, &[(440.0, 0.2)]));
        assert_eq!(strongest_pitch(&low.chroma), Some(9));
        assert_eq!(strongest_pitch(&high.chroma), Some(9));
    }

    #[test]
    fn fft_extracts_the_tones_of_a_major_chord() {
        let config = VisualizerConfig {
            fft_size: 8_192,
            ..VisualizerConfig::default()
        };
        let mut analyzer = Analyzer::new(&config);
        let frame = analyzer.analyze(&sine_samples(
            &config,
            &[(261.63, 0.12), (329.63, 0.12), (392.0, 0.12)],
        ));
        let tones = select_pitch_classes(&frame.chroma, Some(0));
        assert_eq!(tones, vec![0, 4, 7], "chroma was {:?}", frame.chroma);
    }

    #[test]
    fn broadband_noise_has_low_tonal_confidence() {
        let config = VisualizerConfig {
            fft_size: 8_192,
            ..VisualizerConfig::default()
        };
        let mut state = 0x1234_5678_u32;
        let samples: Vec<_> = (0..config.fft_size)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * 0.2
            })
            .collect();
        let mut analyzer = Analyzer::new(&config);
        let frame = analyzer.analyze(&samples);
        assert!(
            frame.tonal_confidence < config.pitch_min_confidence,
            "noise confidence was {}",
            frame.tonal_confidence
        );
    }

    #[test]
    fn spectral_flux_detects_a_new_transient() {
        let config = VisualizerConfig::default();
        let steady = sine_samples(&config, &[(220.0, 0.1)]);
        let mut analyzer = Analyzer::new(&config);
        for _ in 0..14 {
            let frame = analyzer.analyze(&steady);
            assert!(!frame.onset);
        }

        let mut transient = steady;
        transient[config.fft_size / 2] += 0.9;
        let frame = analyzer.analyze(&transient);
        assert!(frame.onset);
    }

    #[test]
    fn pitch_mode_distributes_chord_tones_across_lights() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        let outputs = mapper.render(pitch_frame(&[(0, 0.45), (4, 0.3), (7, 0.25)], 1.0), 3);
        assert_eq!(outputs[0].rgb, sample_palette(&mapper.palette, 0.0));
        assert_eq!(outputs[1].rgb, sample_palette(&mapper.palette, 4.0 / 12.0));
        assert_eq!(outputs[2].rgb, sample_palette(&mapper.palette, 7.0 / 12.0));
    }

    #[test]
    fn one_light_blends_chord_tones_in_linear_rgb() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        let output = mapper.render(pitch_frame(&[(0, 0.5), (4, 0.3), (7, 0.25)], 1.0), 1)[0];
        let expected = blend_colors(&[
            (sample_palette(&mapper.palette, 0.0), 0.5),
            (sample_palette(&mapper.palette, 4.0 / 12.0), 0.3),
            (sample_palette(&mapper.palette, 7.0 / 12.0), 0.25),
        ]);
        assert_eq!(output.rgb, expected);
    }

    #[test]
    fn low_confidence_harmony_changes_color_progressively() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        let confident = mapper.render(pitch_frame(&[(0, 1.0)], 1.0), 2);
        let mut uncertain = confident.clone();
        for _ in 0..12 {
            uncertain = mapper.render(pitch_frame(&[(7, 1.0)], 0.0), 2);
        }
        assert_ne!(uncertain[0].rgb, confident[0].rgb);
        assert_eq!(mapper.pitch_root, Some(7));
    }

    #[test]
    fn silence_keeps_the_last_harmonic_color() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        let confident = mapper.render(pitch_frame(&[(0, 1.0)], 1.0), 2);
        let mut silence = pitch_frame(&[(7, 1.0)], 1.0);
        silence.intensity = 0.0;
        silence.onset = true;
        let silent = mapper.render(silence, 2);
        assert_eq!(silent[0].rgb, confident[0].rgb);
        assert_eq!(silent[1].rgb, confident[1].rgb);
        assert_eq!(mapper.pitch_root, Some(0));
    }

    #[test]
    fn root_change_requires_persistence_without_an_onset() {
        let config = VisualizerConfig {
            fps: 30,
            ..VisualizerConfig::default()
        };
        let mut mapper = VisualMapper::new(&config);
        mapper.render(pitch_frame(&[(0, 1.0)], 1.0), 2);
        mapper.render(pitch_frame(&[(7, 1.0)], 1.0), 2);
        assert_eq!(mapper.pitch_root, Some(0));
        mapper.render(pitch_frame(&[(7, 1.0)], 1.0), 2);
        mapper.render(pitch_frame(&[(7, 1.0)], 1.0), 2);
        assert_eq!(mapper.pitch_root, Some(7));
    }

    #[test]
    fn onset_accelerates_a_dominant_root_change() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        mapper.render(pitch_frame(&[(0, 1.0)], 1.0), 2);
        let mut changed = pitch_frame(&[(7, 1.0)], 1.0);
        changed.onset = true;
        mapper.render(changed, 2);
        assert_eq!(mapper.pitch_root, Some(7));
    }

    #[test]
    fn uniform_percussive_onset_does_not_replace_the_root() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        mapper.render(pitch_frame(&[(0, 1.0)], 1.0), 2);
        let mut percussion = pitch_frame(
            &[
                (0, 1.0),
                (1, 1.0),
                (2, 1.0),
                (3, 1.0),
                (4, 1.0),
                (5, 1.0),
                (6, 1.0),
                (7, 1.0),
                (8, 1.0),
                (9, 1.0),
                (10, 1.0),
                (11, 1.0),
            ],
            0.0,
        );
        percussion.onset = true;
        mapper.render(percussion, 2);
        assert_eq!(mapper.pitch_root, Some(0));
    }

    #[test]
    fn chord_selection_includes_likely_secondary_tones() {
        let frame = pitch_frame(&[(0, 0.7), (4, 0.26), (7, 0.1)], 0.1);
        assert_eq!(select_pitch_classes(&frame.chroma, Some(0)), vec![0, 4]);
    }

    #[test]
    fn beats_rotate_chord_colors_between_lights() {
        let config = VisualizerConfig::default();
        let mut mapper = VisualMapper::new(&config);
        let chord = pitch_frame(&[(0, 0.45), (4, 0.3), (7, 0.25)], 1.0);
        let before = mapper.render(chord, 3);
        let mut beat = chord;
        beat.beat = true;
        let after = mapper.render(beat, 3);
        assert_eq!(after[0].rgb, before[1].rgb);
        assert_eq!(after[1].rgb, before[2].rgb);
        assert_eq!(after[2].rgb, before[0].rgb);
    }

    #[test]
    fn custom_beat_envelope_jumps_holds_and_expires_without_changing_smoothing() {
        let config = VisualizerConfig {
            brightness_min: 10,
            brightness_max: 100,
            beat_boost: 0.5,
            beat_duration_ms: 80,
            attack_ms: 1_000.0,
            ..VisualizerConfig::default()
        };
        let mut mapper = VisualMapper::new(&config);
        let started = mapper.last_update;
        let beat = AnalysisFrame {
            intensity: 0.0,
            beat: true,
            ..AnalysisFrame::default()
        };

        let initial = mapper.render_at(beat, 1, started + Duration::from_millis(10));
        assert_eq!(initial[0].dimming, 55);
        assert_eq!(mapper.smoothed_intensity, 0.0);

        let held = mapper.render_at(
            AnalysisFrame::default(),
            1,
            started + Duration::from_millis(89),
        );
        assert_eq!(held[0].dimming, 55);

        let expired = mapper.render_at(
            AnalysisFrame::default(),
            1,
            started + Duration::from_millis(90),
        );
        assert_eq!(expired[0].dimming, 10);
        assert!(mapper.beat_until.is_none());
        assert_eq!(mapper.smoothed_intensity, 0.0);
    }

    #[test]
    fn drift_mode_preserves_palette_spread() {
        let config = VisualizerConfig {
            color_mode: ColorMode::Drift,
            color_speed: 0.0,
            color_influence: 0.0,
            spatial_spread: 0.25,
            ..VisualizerConfig::default()
        };
        let mut mapper = VisualMapper::new(&config);
        let outputs = mapper.render(AnalysisFrame::default(), 2);
        assert_eq!(outputs[0].rgb, sample_palette(&mapper.palette, 0.0));
        assert_eq!(outputs[1].rgb, sample_palette(&mapper.palette, 0.25));
    }
}
