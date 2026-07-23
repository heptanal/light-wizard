mod audio;
mod cli;
mod color_cycle;
mod config;
mod visualizer;
mod wiz;
mod wizard;

use std::{
    collections::BTreeSet,
    io::{self, Write},
    net::Ipv4Addr,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossbeam_channel::bounded;

use crate::{
    audio::{AudioFileMetadata, FilePlayback, PreparedAudioFile, SystemAudioCapture},
    cli::{AppCommand, Cli, ColorCycleArgs, LightSelectionArgs, VisualizerArgs},
    config::AppConfig,
    visualizer::{
        AnalysisFrame, LightFrame, PITCH_NAMES, VisualMapper, select_pitch_classes,
        spawn_analysis_worker,
    },
    wiz::{StateSnapshot, WizClient, WizLight, discover},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.as_deref();
    match cli.command {
        AppCommand::DefaultConfig => {
            print!("{}", AppConfig::default().to_toml()?);
            Ok(())
        }
        AppCommand::Configure => run_configure(config_path),
        AppCommand::Discover(selection) => run_discover(config_path, &selection),
        AppCommand::Visualizer(args) => run_visualizer(config_path, args),
        AppCommand::ColorCycle(args) => run_color_cycle(config_path, args),
    }
}

fn run_configure(config_path: Option<&Path>) -> Result<()> {
    let path = config_path.unwrap_or_else(|| Path::new("light-wizard.toml"));
    let initial = if path.is_file() {
        AppConfig::load(path)?
    } else {
        AppConfig::default()
    };
    wizard::run(initial, path)
}

fn run_discover(config_path: Option<&Path>, selection: &LightSelectionArgs) -> Result<()> {
    let mut config = load_config(config_path)?;
    apply_selection_overrides(&mut config, selection);
    config.validate()?;

    if config.network.lights.is_empty() {
        let lights = discover(
            config.network.port,
            Duration::from_secs_f32(config.network.discovery_seconds),
            &config.network.broadcasts,
        )?;
        print_discovery(&lights);
    } else {
        let client = WizClient::new(
            config.network.port,
            Duration::from_millis(config.network.request_timeout_ms),
        )?;
        let mut lights = configured_lights(&config.network.lights);
        for light in &mut lights {
            client.identify(light);
        }
        print_discovery(&lights);
    }
    Ok(())
}

fn run_visualizer(config_path: Option<&Path>, args: VisualizerArgs) -> Result<()> {
    let mut config = load_config(config_path)?;
    apply_selection_overrides(&mut config, &args.selection);
    apply_visualizer_overrides(&mut config, &args);
    config.validate()?;

    // File and output-device errors are reported before network discovery or
    // light state changes. Constructing this does not start playback.
    let prepared_file = args
        .audio_file
        .as_deref()
        .map(|path| PreparedAudioFile::open(path, config.player.playback_delay_ms))
        .transpose()?;

    let ModeSetup {
        client,
        lights,
        snapshots,
        running,
    } = prepare_mode(&config, args.dry_run)?;

    let result = match prepared_file {
        Some(prepared) => {
            run_file_mode(prepared, client.as_ref(), &lights, &config, &running, &args)
        }
        None => run_system_audio_mode(client.as_ref(), &lights, &config, &running, &args),
    };
    if !args.quiet {
        println!();
    }
    if let Some(client) = &client {
        restore_states(client, &snapshots);
    }
    result
}

fn run_color_cycle(config_path: Option<&Path>, args: ColorCycleArgs) -> Result<()> {
    let mut config = load_config(config_path)?;
    apply_selection_overrides(&mut config, &args.selection);
    apply_color_cycle_overrides(&mut config, &args);
    config.validate()?;

    let ModeSetup {
        client,
        lights,
        snapshots,
        running,
    } = prepare_mode(&config, args.dry_run)?;
    println!(
        "Cycling colors at {:.1} changes per second with {} timing; press Ctrl+C to stop.{}",
        config.color_cycle.frequency_hz,
        config.color_cycle.pattern,
        if args.dry_run { " (dry run)" } else { "" }
    );
    if (3.0..=30.0).contains(&config.color_cycle.frequency_hz) {
        eprintln!(
            "warning: rapid light color changes in the 3-30 Hz range may trigger photosensitive seizures"
        );
    }
    let result = color_cycle::run(
        client.as_ref(),
        &lights,
        &config.color_cycle,
        &running,
        args.quiet,
    );
    if !args.quiet {
        println!();
    }
    if let Some(client) = &client {
        restore_states(client, &snapshots);
    }
    result
}

fn load_config(config_path: Option<&Path>) -> Result<AppConfig> {
    if let Some(path) = config_path {
        return AppConfig::load(path);
    }
    let conventional = Path::new("light-wizard.toml");
    if conventional.is_file() {
        AppConfig::load(conventional)
    } else {
        Ok(AppConfig::default())
    }
}

fn apply_selection_overrides(config: &mut AppConfig, selection: &LightSelectionArgs) {
    config
        .network
        .lights
        .extend(selection.lights.iter().copied());
    config.network.lights.sort_unstable();
    config.network.lights.dedup();
    config
        .network
        .broadcasts
        .extend(selection.broadcasts.iter().copied());
    config.network.broadcasts.sort_unstable();
    config.network.broadcasts.dedup();
}

fn apply_visualizer_overrides(config: &mut AppConfig, args: &VisualizerArgs) {
    if let Some(fps) = args.fps {
        config.visualizer.fps = fps;
    }
    if let Some(sensitivity) = args.sensitivity {
        config.visualizer.sensitivity = sensitivity;
    }
    if let Some(palette) = &args.palette {
        config.visualizer.palette.clone_from(palette);
    }
    if let Some(playback_delay_ms) = args.playback_delay_ms {
        config.player.playback_delay_ms = playback_delay_ms;
    }
    if args.no_restore {
        config.network.restore_state = false;
    }
}

fn apply_color_cycle_overrides(config: &mut AppConfig, args: &ColorCycleArgs) {
    if let Some(frequency_hz) = args.frequency_hz {
        config.color_cycle.frequency_hz = frequency_hz;
    }
    if let Some(palette) = &args.palette {
        config.color_cycle.palette.clone_from(palette);
    }
    if let Some(brightness) = args.brightness {
        config.color_cycle.brightness = brightness;
    }
    if let Some(pattern) = args.pattern {
        config.color_cycle.pattern = pattern;
    }
    if args.no_restore {
        config.network.restore_state = false;
    }
}

struct ModeSetup {
    client: Option<WizClient>,
    lights: Vec<WizLight>,
    snapshots: Vec<StateSnapshot>,
    running: Arc<AtomicBool>,
}

fn prepare_mode(config: &AppConfig, dry_run: bool) -> Result<ModeSetup> {
    let client = if dry_run {
        None
    } else {
        Some(WizClient::new(
            config.network.port,
            Duration::from_millis(config.network.request_timeout_ms),
        )?)
    };
    let mut lights = configured_lights(&config.network.lights);
    if lights.is_empty() && !dry_run {
        println!(
            "Searching the local network for WiZ lights for {:.1} seconds...",
            config.network.discovery_seconds
        );
        lights = discover(
            config.network.port,
            Duration::from_secs_f32(config.network.discovery_seconds),
            &config.network.broadcasts,
        )?;
    }
    if lights.is_empty() && !dry_run {
        bail!(
            "no WiZ lights were discovered; verify that this Mac and the lights are on the same LAN, or pass each address with --light <IP>"
        );
    }
    if let Some(client) = &client {
        for light in &mut lights {
            client.identify(light);
        }
    }
    if !lights.is_empty() {
        println!("Using {} WiZ light(s):", lights.len());
        for light in &lights {
            println!("  {}", light.display_name());
        }
    }

    let snapshots = if config.network.restore_state {
        client
            .as_ref()
            .map(|client| capture_states(client, &lights))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::clone(&running);
    ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))
        .context("failed to install the Ctrl+C handler")?;
    Ok(ModeSetup {
        client,
        lights,
        snapshots,
        running,
    })
}

fn run_system_audio_mode(
    client: Option<&WizClient>,
    lights: &[WizLight],
    config: &AppConfig,
    running: &Arc<AtomicBool>,
    cli: &VisualizerArgs,
) -> Result<()> {
    let (sample_sender, sample_receiver) = bounded(6);
    let (analysis_sender, analysis_receiver) = bounded(256);
    let analysis_thread = spawn_analysis_worker(
        sample_receiver,
        analysis_sender,
        config.visualizer.clone(),
        Arc::clone(running),
    );
    println!(
        "Starting macOS system-audio capture (approve the permission prompt if one appears)..."
    );
    let capture = match SystemAudioCapture::start(
        sample_sender,
        config.visualizer.sample_rate,
        config.visualizer.channels,
    ) {
        Ok(capture) => capture,
        Err(error) => {
            running.store(false, Ordering::Relaxed);
            drop(analysis_receiver);
            let _ = analysis_thread.join();
            return Err(error);
        }
    };

    println!(
        "Listening to macOS system audio at {} Hz; press Ctrl+C to stop.{}",
        config.visualizer.sample_rate,
        if cli.dry_run { " (dry run)" } else { "" }
    );
    let result = visualization_loop(
        client,
        lights,
        config,
        &analysis_receiver,
        running,
        cli,
        None,
    );

    drop(capture);
    running.store(false, Ordering::Relaxed);
    drop(analysis_receiver);
    let _ = analysis_thread.join();
    result
}

fn run_file_mode(
    prepared: PreparedAudioFile,
    client: Option<&WizClient>,
    lights: &[WizLight],
    config: &AppConfig,
    running: &Arc<AtomicBool>,
    cli: &VisualizerArgs,
) -> Result<()> {
    let metadata = prepared.metadata().clone();
    print_file_startup(&metadata, config.player.playback_delay_ms, cli.dry_run);

    let mut analysis_config = config.visualizer.clone();
    analysis_config.sample_rate = metadata.sample_rate;
    let (sample_sender, sample_receiver) = bounded(128);
    let (analysis_sender, analysis_receiver) = bounded(256);
    let analysis_thread = spawn_analysis_worker(
        sample_receiver,
        analysis_sender,
        analysis_config,
        Arc::clone(running),
    );
    let playback = prepared.start(sample_sender);
    let result = visualization_loop(
        client,
        lights,
        config,
        &analysis_receiver,
        running,
        cli,
        Some(&playback),
    );

    running.store(false, Ordering::Relaxed);
    // Release frame backpressure before stopping the output callback. This
    // guarantees Ctrl+C and network-error cleanup cannot strand the decoder
    // behind a full analysis queue.
    drop(analysis_receiver);
    playback.stop();
    drop(playback);
    let _ = analysis_thread.join();
    result
}

fn print_file_startup(metadata: &AudioFileMetadata, delay_ms: u64, dry_run: bool) {
    println!("Audio file: {}", metadata.path.display());
    println!(
        "Format: {}; source: {} Hz, {} channel(s); duration: {}",
        metadata.format,
        metadata.sample_rate,
        metadata.channels,
        metadata
            .duration
            .map(format_duration)
            .unwrap_or_else(|| "unknown".into())
    );
    println!(
        "Analyzing decoded audio immediately and playing it {delay_ms} ms later through the default output device."
    );
    println!(
        "Playback will finish automatically; press Ctrl+C to cancel.{}",
        if dry_run {
            " (dry run: audio still plays; lights are not controlled)"
        } else {
            ""
        }
    );
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn configured_lights(addresses: &[Ipv4Addr]) -> Vec<WizLight> {
    addresses
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|ip| WizLight { ip, mac: None })
        .collect()
}

fn print_discovery(lights: &[WizLight]) {
    if lights.is_empty() {
        println!("No WiZ lights found.");
        println!("Try an explicit directed broadcast, for example --broadcast 192.168.1.255.");
        return;
    }
    println!("Found {} WiZ light(s):", lights.len());
    for light in lights {
        println!("{}", light.display_name());
    }
}

fn capture_states(client: &WizClient, lights: &[WizLight]) -> Vec<StateSnapshot> {
    lights
        .iter()
        .filter_map(|light| match client.query_pilot(light) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                eprintln!(
                    "warning: could not save the state of {}: {error:#}",
                    light.ip
                );
                None
            }
        })
        .collect()
}

fn visualization_loop(
    client: Option<&WizClient>,
    lights: &[WizLight],
    config: &AppConfig,
    analyses: &crossbeam_channel::Receiver<AnalysisFrame>,
    running: &AtomicBool,
    cli: &VisualizerArgs,
    playback: Option<&FilePlayback>,
) -> Result<()> {
    let source_name = if playback.is_some() {
        "audio-file"
    } else {
        "system-audio"
    };
    let frame_interval = Duration::from_secs_f64(1.0 / config.visualizer.fps as f64);
    let mut next_frame = Instant::now();
    let mut mapper = VisualMapper::new(&config.visualizer);
    let output_count = lights.len().max(1);
    let mut previous: Vec<Option<LightFrame>> = vec![None; output_count];
    let mut latest = AnalysisFrame::default();
    let mut have_audio = false;
    let mut pending_beat = false;
    let mut pending_onset = false;
    let mut last_send = Instant::now() - Duration::from_secs(5);
    let mut last_meter = Instant::now() - Duration::from_secs(1);

    while running.load(Ordering::Relaxed) {
        let mut analysis_disconnected = false;
        loop {
            match analyses.try_recv() {
                Ok(frame) => {
                    pending_beat |= frame.beat;
                    pending_onset |= frame.onset;
                    latest = frame;
                    have_audio = true;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    analysis_disconnected = true;
                    break;
                }
            }
        }
        if analysis_disconnected {
            if let Some(playback) = playback {
                if should_finish_file_mode(playback.is_finished(), analysis_disconnected) {
                    return Ok(());
                }
                if !have_audio {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            } else {
                bail!("{source_name} analysis stopped unexpectedly");
            }
        }
        if !have_audio {
            match analyses.recv_timeout(Duration::from_millis(50)) {
                Ok(frame) => {
                    pending_beat |= frame.beat;
                    pending_onset |= frame.onset;
                    latest = frame;
                    have_audio = true;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    if should_finish_file_mode(
                        playback.is_some_and(FilePlayback::is_finished),
                        true,
                    ) {
                        return Ok(());
                    }
                    bail!("{source_name} analysis stopped unexpectedly")
                }
            }
        }

        let now = Instant::now();
        next_frame = prioritize_beat_frame(next_frame, now, pending_beat);
        if now < next_frame {
            thread::sleep((next_frame - now).min(Duration::from_millis(10)));
            continue;
        }
        next_frame = now + frame_interval;
        let mut rendered_analysis = latest;
        rendered_analysis.beat = pending_beat;
        rendered_analysis.onset = pending_onset;
        let outputs = mapper.render(rendered_analysis, output_count);
        pending_beat = false;
        pending_onset = false;
        let keepalive = now.duration_since(last_send) >= Duration::from_secs(2);
        let mut sent_any = false;
        if let Some(client) = client {
            for (index, (light, output)) in lights.iter().zip(&outputs).enumerate() {
                let changed = previous[index]
                    .is_none_or(|old| output.differs_from(old, config.visualizer.change_threshold));
                if changed || keepalive {
                    client.send_pilot_one(light, output.rgb, output.dimming)?;
                    previous[index] = Some(*output);
                    sent_any = true;
                }
            }
            if rendered_analysis.beat && config.visualizer.pulse_on_beat {
                client.send_pulse(
                    lights,
                    config.visualizer.pulse_delta,
                    config.visualizer.pulse_duration_ms,
                )?;
                sent_any = true;
            }
        }
        if sent_any {
            last_send = now;
        }

        let meter_due = now.duration_since(last_meter) >= Duration::from_millis(100);
        if !cli.quiet && (meter_due || rendered_analysis.onset || rendered_analysis.beat) {
            print_meter(
                rendered_analysis,
                outputs[0],
                config.visualizer.pitch_min_confidence,
            );
            last_meter = now;
        }
    }
    Ok(())
}

fn prioritize_beat_frame(next_frame: Instant, now: Instant, pending_beat: bool) -> Instant {
    if pending_beat { now } else { next_frame }
}

fn should_finish_file_mode(playback_finished: bool, analysis_disconnected: bool) -> bool {
    playback_finished && analysis_disconnected
}

fn print_meter(analysis: AnalysisFrame, output: LightFrame, pitch_min_confidence: f32) {
    const WIDTH: usize = 24;
    let filled = (analysis.intensity * WIDTH as f32).round() as usize;
    let meter = format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled));
    let has_pitch_energy = analysis.intensity > 0.01 && analysis.chroma.iter().sum::<f32>() > 0.5;
    let notes = if has_pitch_energy {
        let detected = select_pitch_classes(&analysis.chroma, None)
            .into_iter()
            .map(|pitch| PITCH_NAMES[pitch])
            .collect::<Vec<_>>()
            .join("/");
        if analysis.tonal_confidence < pitch_min_confidence {
            format!("~{detected}")
        } else {
            detected
        }
    } else {
        "--".into()
    };
    print!(
        "\r\x1b[2K[{meter}] {:>6.1} dB  B {:>3.0}% M {:>3.0}% T {:>3.0}%  N {:<9} {:>3.0}%  #{:02x}{:02x}{:02x} {:>3}%{}{}",
        analysis.dbfs,
        analysis.bass * 100.0,
        analysis.mid * 100.0,
        analysis.treble * 100.0,
        notes,
        analysis.tonal_confidence * 100.0,
        output.rgb[0],
        output.rgb[1],
        output.rgb[2],
        output.dimming,
        if analysis.onset { " ONSET" } else { "" },
        if analysis.beat { " BEAT" } else { "" },
    );
    let _ = io::stdout().flush();
}

fn restore_states(client: &WizClient, snapshots: &[StateSnapshot]) {
    if snapshots.is_empty() {
        return;
    }
    println!("Restoring the previous light state...");
    for snapshot in snapshots {
        if let Err(error) = client.restore(snapshot) {
            eprintln!("warning: could not restore {}: {error:#}", snapshot.ip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_mode_waits_for_playback_and_analysis_completion() {
        assert!(!should_finish_file_mode(false, false));
        assert!(!should_finish_file_mode(true, false));
        assert!(!should_finish_file_mode(false, true));
        assert!(should_finish_file_mode(true, true));
    }

    #[test]
    fn duration_display_is_stable() {
        assert_eq!(format_duration(Duration::from_secs(185)), "3:05");
    }

    #[test]
    fn pending_beats_render_without_waiting_for_the_regular_deadline() {
        let now = Instant::now();
        let regular_deadline = now + Duration::from_millis(33);
        assert_eq!(prioritize_beat_frame(regular_deadline, now, true), now);
        assert_eq!(
            prioritize_beat_frame(regular_deadline, now, false),
            regular_deadline
        );
    }
}
