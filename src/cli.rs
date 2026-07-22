use std::{net::Ipv4Addr, path::PathBuf};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "light-wizard",
    version,
    about = "Turn macOS system audio or an audio file into a WiZ light show"
)]
pub struct Cli {
    /// TOML configuration file. The wizard also saves to this path.
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Explain and interactively configure every option, then exit.
    #[arg(
        long,
        conflicts_with_all = [
            "lights",
            "broadcasts",
            "discover_only",
            "dry_run",
            "print_default_config",
            "fps",
            "sensitivity",
            "palette",
            "audio_file",
            "playback_delay_ms",
            "no_restore",
            "quiet"
        ]
    )]
    pub config_wizard: bool,

    /// Light IPv4 address. Repeat this option to bypass discovery.
    #[arg(short, long = "light")]
    pub lights: Vec<Ipv4Addr>,

    /// Additional IPv4 broadcast destination used for discovery.
    #[arg(long = "broadcast")]
    pub broadcasts: Vec<Ipv4Addr>,

    /// Play and visualize one local audio file instead of capturing system audio.
    #[arg(long, value_name = "PATH", conflicts_with = "discover_only")]
    pub audio_file: Option<PathBuf>,

    /// Override the file playback delay in milliseconds (0-5000).
    #[arg(
        long,
        value_name = "MS",
        requires = "audio_file",
        value_parser = clap::value_parser!(u64).range(0..=5_000)
    )]
    pub playback_delay_ms: Option<u64>,

    /// Discover and print WiZ lights without starting audio capture.
    #[arg(long)]
    pub discover_only: bool,

    /// Analyze and print levels without controlling lights; file audio still plays.
    #[arg(long)]
    pub dry_run: bool,

    /// Print the default TOML configuration and exit.
    #[arg(long)]
    pub print_default_config: bool,

    /// Override visualizer network frames per second (1-30).
    #[arg(long)]
    pub fps: Option<u32>,

    /// Override linear audio sensitivity (for example 1.8).
    #[arg(long)]
    pub sensitivity: Option<f32>,

    /// Override the palette with comma-separated hex colors.
    #[arg(long, value_delimiter = ',')]
    pub palette: Option<Vec<String>>,

    /// Leave the visualizer's final light state active on exit.
    #[arg(long)]
    pub no_restore: bool,

    /// Suppress the continuously updated terminal meter.
    #[arg(long, short)]
    pub quiet: bool,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, error::ErrorKind};

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_audio_file_and_delay() {
        let cli = Cli::try_parse_from([
            "light-wizard",
            "--audio-file",
            "song.mp3",
            "--playback-delay-ms",
            "425",
        ])
        .unwrap();
        assert_eq!(cli.audio_file, Some(PathBuf::from("song.mp3")));
        assert_eq!(cli.playback_delay_ms, Some(425));
    }

    #[test]
    fn playback_delay_requires_audio_file() {
        let error =
            Cli::try_parse_from(["light-wizard", "--playback-delay-ms", "425"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rejects_out_of_range_playback_delay() {
        let error = Cli::try_parse_from([
            "light-wizard",
            "--audio-file",
            "song.mp3",
            "--playback-delay-ms",
            "5001",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }
}
