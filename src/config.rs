use std::{fmt, fs, net::Ipv4Addr, path::Path, str::FromStr};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub player: PlayerConfig,
    pub visualizer: VisualizerConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration from {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse configuration from {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.network.validate()?;
        self.player.validate()?;
        self.visualizer.validate()?;
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize configuration")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        fs::write(path, self.to_toml()?)
            .with_context(|| format!("failed to write configuration to {}", path.display()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    /// How far decoded audio playback trails light analysis in file mode.
    pub playback_delay_ms: u64,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            playback_delay_ms: 50,
        }
    }
}

impl PlayerConfig {
    fn validate(&self) -> Result<()> {
        if self.playback_delay_ms > 5_000 {
            bail!("player.playback_delay_ms must be between 0 and 5000");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Explicit light addresses. Leave empty to use discovery.
    pub lights: Vec<Ipv4Addr>,
    /// Additional broadcast destinations. Interface-directed broadcasts are
    /// detected automatically and 255.255.255.255 is always included.
    pub broadcasts: Vec<Ipv4Addr>,
    pub port: u16,
    pub discovery_seconds: f32,
    pub request_timeout_ms: u64,
    pub restore_state: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            lights: Vec::new(),
            broadcasts: Vec::new(),
            port: 38_899,
            discovery_seconds: 3.0,
            request_timeout_ms: 750,
            restore_state: true,
        }
    }
}

impl NetworkConfig {
    fn validate(&self) -> Result<()> {
        if self.port == 0 {
            bail!("network.port must be non-zero");
        }
        if !(0.25..=30.0).contains(&self.discovery_seconds) {
            bail!("network.discovery_seconds must be between 0.25 and 30");
        }
        if !(50..=10_000).contains(&self.request_timeout_ms) {
            bail!("network.request_timeout_ms must be between 50 and 10000");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    #[default]
    Pitch,
    Drift,
}

impl fmt::Display for ColorMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pitch => formatter.write_str("pitch"),
            Self::Drift => formatter.write_str("drift"),
        }
    }
}

impl FromStr for ColorMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "pitch" => Ok(Self::Pitch),
            "drift" => Ok(Self::Drift),
            _ => Err("expected 'pitch' or 'drift'".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualizerConfig {
    /// Network update rate. WiZ lights do not need audio-rate updates.
    pub fps: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub fft_size: usize,
    /// Linear input gain. 1.0 means no gain.
    pub sensitivity: f32,
    /// Audio dBFS values mapped to the configured brightness interval.
    pub floor_db: f32,
    pub ceiling_db: f32,
    pub brightness_min: u8,
    pub brightness_max: u8,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub beat_threshold: f32,
    pub beat_boost: f32,
    pub beat_cooldown_ms: u64,
    /// Move detected chord-tone colors between lights on each beat.
    pub rotate_colors_on_beat: bool,
    /// How audio chooses colors. Pitch is harmonic; drift is the legacy motion.
    pub color_mode: ColorMode,
    /// Hex RGB colors, interpolated in order and wrapped.
    pub palette: Vec<String>,
    /// Time constant for stabilizing the twelve pitch-class energies.
    pub pitch_smoothing_ms: f32,
    /// Confidence at which pitch tracking reaches its full response speed.
    pub pitch_min_confidence: f32,
    /// Palette revolutions per second independent of audio.
    pub color_speed: f32,
    /// How much spectral balance moves through the palette.
    pub color_influence: f32,
    /// Palette offset between adjacent lights, in revolutions.
    pub spatial_spread: f32,
    /// Minimum aggregate RGB change before sending another update.
    pub change_threshold: u16,
    /// Send the WiZ pulse command on detected beats.
    pub pulse_on_beat: bool,
    pub pulse_delta: i16,
    pub pulse_duration_ms: u32,
}

impl Default for VisualizerConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            sample_rate: 48_000,
            channels: 2,
            fft_size: 2_048,
            sensitivity: 1.35,
            floor_db: -58.0,
            ceiling_db: 0.0,
            brightness_min: 8,
            brightness_max: 100,
            attack_ms: 45.0,
            release_ms: 280.0,
            beat_threshold: 1.4,
            beat_boost: 0.22,
            beat_cooldown_ms: 180,
            rotate_colors_on_beat: true,
            color_mode: ColorMode::Pitch,
            palette: vec![
                "#ff2a68".into(),
                "#7b2cff".into(),
                "#008cff".into(),
                "#00e5b0".into(),
                "#ffd43b".into(),
            ],
            pitch_smoothing_ms: 160.0,
            pitch_min_confidence: 0.18,
            color_speed: 0.035,
            color_influence: 0.65,
            spatial_spread: 0.14,
            change_threshold: 5,
            pulse_on_beat: false,
            pulse_delta: 18,
            pulse_duration_ms: 140,
        }
    }
}

impl VisualizerConfig {
    fn validate(&self) -> Result<()> {
        if !(1..=30).contains(&self.fps) {
            bail!("visualizer.fps must be between 1 and 30");
        }
        if !(8_000..=192_000).contains(&self.sample_rate) {
            bail!("visualizer.sample_rate must be between 8000 and 192000");
        }
        if !(1..=8).contains(&self.channels) {
            bail!("visualizer.channels must be between 1 and 8");
        }
        if !(256..=16_384).contains(&self.fft_size) || !self.fft_size.is_power_of_two() {
            bail!("visualizer.fft_size must be a power of two between 256 and 16384");
        }
        if !self.sensitivity.is_finite() || self.sensitivity <= 0.0 {
            bail!("visualizer.sensitivity must be a positive finite number");
        }
        if !self.floor_db.is_finite()
            || !self.ceiling_db.is_finite()
            || !(-120.0..=0.0).contains(&self.floor_db)
            || !(-120.0..=0.0).contains(&self.ceiling_db)
            || self.floor_db >= self.ceiling_db
        {
            bail!("visualizer floor_db and ceiling_db must satisfy -120 <= floor < ceiling <= 0");
        }
        if !(1..=100).contains(&self.brightness_min)
            || !(1..=100).contains(&self.brightness_max)
            || self.brightness_min > self.brightness_max
        {
            bail!("visualizer brightness must satisfy 1 <= min <= max <= 100");
        }
        if !self.attack_ms.is_finite()
            || !self.release_ms.is_finite()
            || self.attack_ms <= 0.0
            || self.release_ms <= 0.0
        {
            bail!("visualizer attack_ms and release_ms must be positive finite numbers");
        }
        if !(1.0..=5.0).contains(&self.beat_threshold) {
            bail!("visualizer.beat_threshold must be between 1 and 5");
        }
        if !(0.0..=1.0).contains(&self.beat_boost) {
            bail!("visualizer.beat_boost must be between 0 and 1");
        }
        if self.beat_cooldown_ms > 10_000 {
            bail!("visualizer.beat_cooldown_ms must be between 0 and 10000");
        }
        if self.palette.len() < 2 {
            bail!("visualizer.palette must contain at least two colors");
        }
        for color in &self.palette {
            parse_hex_color(color)
                .with_context(|| format!("invalid color {color:?} in visualizer.palette"))?;
        }
        if !self.pitch_smoothing_ms.is_finite()
            || !(10.0..=2_000.0).contains(&self.pitch_smoothing_ms)
        {
            bail!("visualizer.pitch_smoothing_ms must be between 10 and 2000");
        }
        if !self.pitch_min_confidence.is_finite()
            || !(0.0..=1.0).contains(&self.pitch_min_confidence)
        {
            bail!("visualizer.pitch_min_confidence must be between 0 and 1");
        }
        if !self.color_speed.is_finite()
            || !self.color_influence.is_finite()
            || !self.spatial_spread.is_finite()
        {
            bail!("visualizer color controls must be finite numbers");
        }
        if !(-100..=100).contains(&self.pulse_delta) {
            bail!("visualizer.pulse_delta must be between -100 and 100");
        }
        if !(20..=5_000).contains(&self.pulse_duration_ms) {
            bail!("visualizer.pulse_duration_ms must be between 20 and 5000");
        }
        Ok(())
    }
}

pub fn parse_hex_color(value: &str) -> Result<[u8; 3]> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected a six-digit RGB color such as #ff0066");
    }
    Ok([
        u8::from_str_radix(&hex[0..2], 16)?,
        u8::from_str_radix(&hex[2..4], 16)?,
        u8::from_str_radix(&hex[4..6], 16)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let defaults = AppConfig::default();
        defaults.validate().unwrap();
        assert_eq!(defaults.visualizer.fps, 30);
        assert_eq!(defaults.visualizer.ceiling_db, 0.0);
        assert_eq!(defaults.player.playback_delay_ms, 50);
    }

    #[test]
    fn config_round_trips() {
        let config = AppConfig::default();
        let encoded = config.to_toml().unwrap();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.visualizer.fft_size, config.visualizer.fft_size);
        assert_eq!(decoded.visualizer.color_mode, ColorMode::Pitch);
        assert_eq!(decoded.player.playback_delay_ms, 50);
    }

    #[test]
    fn older_configs_default_to_pitch_mode() {
        let decoded: AppConfig = toml::from_str(
            r#"
                [visualizer]
                fft_size = 4096
            "#,
        )
        .unwrap();
        assert_eq!(decoded.visualizer.color_mode, ColorMode::Pitch);
        assert_eq!(decoded.visualizer.pitch_smoothing_ms, 160.0);
        assert!(decoded.visualizer.rotate_colors_on_beat);
        assert_eq!(decoded.player.playback_delay_ms, 50);
        decoded.validate().unwrap();
    }

    #[test]
    fn example_config_matches_built_in_defaults() {
        let example: AppConfig =
            toml::from_str(include_str!("../light-wizard.example.toml")).unwrap();
        example.validate().unwrap();
        assert_eq!(
            example.to_toml().unwrap(),
            AppConfig::default().to_toml().unwrap()
        );
    }

    #[test]
    fn file_validation_matches_wizard_numeric_limits() {
        let mut config = AppConfig::default();
        config.visualizer.floor_db = -121.0;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.visualizer.attack_ms = f32::NAN;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.visualizer.beat_cooldown_ms = 10_001;
        assert!(config.validate().is_err());

        let mut config = AppConfig::default();
        config.player.playback_delay_ms = 5_001;
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex_color("#12aBef").unwrap(), [0x12, 0xab, 0xef]);
        assert!(parse_hex_color("nope").is_err());
    }
}
