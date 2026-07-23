use std::{net::Ipv4Addr, path::PathBuf};

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "light-wizard",
    version,
    about = "Control WiZ lights with audio-reactive and custom light modes",
    arg_required_else_help = true
)]
pub struct Cli {
    /// TOML configuration file. The configuration wizard also saves here.
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: AppCommand,
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Turn system audio or a local audio file into a WiZ light show.
    Visualizer(VisualizerArgs),

    /// Cycle WiZ lights through configurable colors at a fixed frequency.
    #[command(name = "color-cycle")]
    ColorCycle(ColorCycleArgs),

    /// Discover and print WiZ lights without controlling them.
    Discover(LightSelectionArgs),

    /// Interactively configure every option and save a TOML file.
    Configure,

    /// Print the complete built-in configuration.
    #[command(name = "default-config")]
    DefaultConfig,
}

#[derive(Debug, Clone, Args, Default)]
pub struct LightSelectionArgs {
    /// Light IPv4 address. Repeat this option to bypass discovery.
    #[arg(short, long = "light")]
    pub lights: Vec<Ipv4Addr>,

    /// Additional IPv4 broadcast destination used for discovery.
    #[arg(long = "broadcast")]
    pub broadcasts: Vec<Ipv4Addr>,
}

#[derive(Debug, Args)]
pub struct VisualizerArgs {
    #[command(flatten)]
    pub selection: LightSelectionArgs,

    /// Play and visualize one local audio file instead of capturing system audio.
    #[arg(long, value_name = "PATH")]
    pub audio_file: Option<PathBuf>,

    /// Override the file playback delay in milliseconds (0-5000).
    #[arg(
        long,
        value_name = "MS",
        requires = "audio_file",
        value_parser = clap::value_parser!(u64).range(0..=5_000)
    )]
    pub playback_delay_ms: Option<u64>,

    /// Analyze and print levels without discovering or controlling lights.
    #[arg(long)]
    pub dry_run: bool,

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

#[derive(Debug, Args)]
pub struct ColorCycleArgs {
    #[command(flatten)]
    pub selection: LightSelectionArgs,

    /// Override hard color changes per second for each light (0.1-30).
    #[arg(long, value_name = "HZ")]
    pub frequency_hz: Option<f32>,

    /// Override the spectrum with comma-separated hex colors.
    #[arg(long, value_delimiter = ',')]
    pub palette: Option<Vec<String>>,

    /// Override the constant brightness percentage (1-100).
    #[arg(long, value_name = "PERCENT")]
    pub brightness: Option<u8>,

    /// Override the multi-light phase pattern: sync, alternate, or chase.
    #[arg(long)]
    pub pattern: Option<crate::config::ColorCyclePattern>,

    /// Run and print color-cycle state without discovering or controlling lights.
    #[arg(long)]
    pub dry_run: bool,

    /// Leave the final color-cycle state active on exit.
    #[arg(long)]
    pub no_restore: bool,

    /// Suppress the continuously updated terminal status.
    #[arg(long, short)]
    pub quiet: bool,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, error::ErrorKind};

    use super::*;
    use crate::config::ColorCyclePattern;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn requires_an_explicit_subcommand() {
        let error = Cli::try_parse_from(["light-wizard"]).unwrap_err();
        assert_eq!(
            error.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn parses_visualizer_file_and_delay() {
        let cli = Cli::try_parse_from([
            "light-wizard",
            "visualizer",
            "--audio-file",
            "song.mp3",
            "--playback-delay-ms",
            "425",
        ])
        .unwrap();
        let AppCommand::Visualizer(args) = cli.command else {
            panic!("expected visualizer command");
        };
        assert_eq!(args.audio_file, Some(PathBuf::from("song.mp3")));
        assert_eq!(args.playback_delay_ms, Some(425));
    }

    #[test]
    fn playback_delay_requires_audio_file() {
        let error =
            Cli::try_parse_from(["light-wizard", "visualizer", "--playback-delay-ms", "425"])
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn rejects_out_of_range_playback_delay() {
        let error = Cli::try_parse_from([
            "light-wizard",
            "visualizer",
            "--audio-file",
            "song.mp3",
            "--playback-delay-ms",
            "5001",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn parses_color_cycle_overrides() {
        let cli = Cli::try_parse_from([
            "light-wizard",
            "--config",
            "studio.toml",
            "color-cycle",
            "--frequency-hz",
            "20",
            "--palette",
            "#ff0044,#00ff88,#2200ff",
            "--brightness",
            "75",
            "--pattern",
            "alternate",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("studio.toml")));
        let AppCommand::ColorCycle(args) = cli.command else {
            panic!("expected color-cycle command");
        };
        assert_eq!(args.frequency_hz, Some(20.0));
        assert_eq!(
            args.palette,
            Some(vec![
                "#ff0044".to_owned(),
                "#00ff88".to_owned(),
                "#2200ff".to_owned()
            ])
        );
        assert_eq!(args.brightness, Some(75));
        assert_eq!(args.pattern, Some(ColorCyclePattern::Alternate));
    }

    #[test]
    fn mode_specific_options_do_not_leak() {
        let error =
            Cli::try_parse_from(["light-wizard", "color-cycle", "--audio-file", "song.mp3"])
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }
}
