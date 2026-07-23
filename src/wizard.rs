use std::{
    fmt::Display,
    io::{self, BufRead, Write},
    net::Ipv4Addr,
    path::Path,
    str::FromStr,
};

use anyhow::{Result, anyhow, bail};

use crate::config::{AppConfig, ColorCyclePattern, ColorMode, parse_hex_color};

pub fn run(initial: AppConfig, output_path: &Path) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    writeln!(writer, "Light Wizard configuration wizard")?;
    writeln!(
        writer,
        "Each prompt shows the current value and the built-in default."
    )?;
    writeln!(
        writer,
        "Press Enter to keep the current value, or type 'default' to reset one field."
    )?;
    writeln!(writer, "No lights are controlled while this wizard runs.")?;

    let config = configure(&mut reader, &mut writer, initial)?;
    config.validate()?;
    let encoded = config.to_toml()?;

    writeln!(writer, "\n=== Configuration preview ===\n")?;
    writeln!(writer, "{encoded}")?;
    let action = if output_path.exists() {
        format!("Save and overwrite {}?", output_path.display())
    } else {
        format!("Save to {}?", output_path.display())
    };
    if confirm(&mut reader, &mut writer, &action, true)? {
        config.save(output_path)?;
        writeln!(writer, "Saved {}.", output_path.display())?;
        if output_path == Path::new("light-wizard.toml") {
            writeln!(writer, "Start the visualizer with: light-wizard visualizer")?;
        } else {
            writeln!(
                writer,
                "Start the visualizer with: light-wizard --config {:?} visualizer",
                output_path
            )?;
        }
    } else {
        writeln!(writer, "Configuration was not saved.")?;
    }
    Ok(())
}

fn configure<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut config: AppConfig,
) -> Result<AppConfig> {
    let defaults = AppConfig::default();

    section(
        writer,
        "Network and lights",
        "WiZ control stays on your LAN and uses small UDP JSON datagrams.",
    )?;
    config.network.lights = prompt_custom(
        reader,
        writer,
        "network.lights",
        "Fixed IPv4 addresses for the lights. Leave this on auto-discover unless broadcasts are blocked or DHCP reservations make the addresses stable. Enter addresses separated by commas; type 'auto' or 'none' to clear the list.",
        &config.network.lights,
        &defaults.network.lights,
        |addresses| format_ip_list(addresses),
        parse_ip_list,
    )?;
    config.network.broadcasts = prompt_custom(
        reader,
        writer,
        "network.broadcasts",
        "Extra directed-broadcast destinations used only during discovery. Interface broadcasts and 255.255.255.255 are already tried automatically. Usually this remains empty; an example is 192.168.1.255.",
        &config.network.broadcasts,
        &defaults.network.broadcasts,
        |addresses| format_optional_ip_list(addresses),
        parse_ip_list,
    )?;
    config.network.port = prompt_value(
        reader,
        writer,
        "network.port",
        "UDP destination port exposed by WiZ firmware. All commonly supported local-control lights use 38899, so change this only for protocol experiments.",
        config.network.port,
        defaults.network.port,
        |value| parse_range(value, 1, u16::MAX),
    )?;
    config.network.discovery_seconds = prompt_value(
        reader,
        writer,
        "network.discovery_seconds",
        "How long startup listens for discovery replies. Longer waits help sleepy or congested networks but delay startup when no lights are available. Valid range: 0.25–30 seconds.",
        config.network.discovery_seconds,
        defaults.network.discovery_seconds,
        |value| parse_f32_between(value, 0.25, 30.0),
    )?;
    config.network.request_timeout_ms = prompt_value(
        reader,
        writer,
        "network.request_timeout_ms",
        "Deadline for state queries such as getPilot. Real-time animation frames never wait for replies. Increase this on a lossy Wi-Fi network. Valid range: 50–10000 ms.",
        config.network.request_timeout_ms,
        defaults.network.request_timeout_ms,
        |value| parse_range(value, 50, 10_000),
    )?;
    config.network.restore_state = prompt_value(
        reader,
        writer,
        "network.restore_state",
        "When enabled, the visualizer reads each light before starting and restores its power, color/temperature or scene, and brightness after Ctrl+C or file playback completes. Disable it to leave the final visualizer frame active.",
        config.network.restore_state,
        defaults.network.restore_state,
        parse_bool,
    )?;

    section(
        writer,
        "Built-in audio file player",
        "File mode analyzes decoded audio first, then plays the same samples after a manual synchronization delay.",
    )?;
    config.player.playback_delay_ms = prompt_value(
        reader,
        writer,
        "player.playback_delay_ms",
        "Milliseconds that file audio waits behind light analysis. Increase this if audible events happen before the lights react; decrease it if the lights lead the audio. WiZ UDP commands are fire-and-forget, so network round-trip time cannot calibrate visible response. Valid range: 0–5000 ms.",
        config.player.playback_delay_ms,
        defaults.player.playback_delay_ms,
        |value| parse_range(value, 0, 5_000),
    )?;

    section(
        writer,
        "Audio analysis and update rate",
        "Audio is captured locally, downmixed to mono, and analyzed before light frames are sent.",
    )?;
    config.visualizer.fps = prompt_value(
        reader,
        writer,
        "visualizer.fps",
        "Maximum light-network updates per second—not the audio sample rate. 30 is the built-in default and supported maximum for the most responsive harmonic and beat motion. Lower values reduce scheduled work; unchanged frames are still suppressed. Valid range: 1–30.",
        config.visualizer.fps,
        defaults.visualizer.fps,
        |value| parse_range(value, 1, 30),
    )?;
    config.visualizer.sample_rate = prompt_value(
        reader,
        writer,
        "visualizer.sample_rate",
        "System-audio samples per second requested from ScreenCaptureKit. 48000 Hz matches typical macOS output. File mode ignores this setting and analyzes each file at its native rate. Valid range: 8000–192000 Hz.",
        config.visualizer.sample_rate,
        defaults.visualizer.sample_rate,
        |value| parse_range(value, 8_000, 192_000),
    )?;
    config.visualizer.channels = prompt_value(
        reader,
        writer,
        "visualizer.channels",
        "Channels requested from ScreenCaptureKit before they are downmixed to mono. File mode instead uses and preserves the source file's native channels for playback. Stereo (2) is appropriate for nearly all system audio. Valid range: 1–8.",
        config.visualizer.channels,
        defaults.visualizer.channels,
        |value| parse_range(value, 1, 8),
    )?;
    config.visualizer.fft_size = prompt_value(
        reader,
        writer,
        "visualizer.fft_size",
        "Samples in each frequency-analysis window. Larger windows distinguish bass more accurately but react later; 2048 samples at 48 kHz span about 43 ms. Must be a power of two from 256 through 16384.",
        config.visualizer.fft_size,
        defaults.visualizer.fft_size,
        parse_fft_size,
    )?;

    section(
        writer,
        "Loudness response",
        "These controls map measured dBFS audio levels to visual brightness.",
    )?;
    config.visualizer.sensitivity = prompt_value(
        reader,
        writer,
        "visualizer.sensitivity",
        "Linear gain applied before dB mapping. Raise it when the lights remain too dim; lower it when ordinary audio is constantly saturated. 1.0 is unity gain and 2.0 is roughly +6 dB.",
        config.visualizer.sensitivity,
        defaults.visualizer.sensitivity,
        parse_positive_f32,
    )?;
    config.visualizer.floor_db = prompt_value(
        reader,
        writer,
        "visualizer.floor_db",
        "Audio at or below this dBFS level maps to minimum brightness. A more negative value makes very quiet audio visible. Valid range in the wizard: -120–0 dBFS.",
        config.visualizer.floor_db,
        defaults.visualizer.floor_db,
        |value| parse_f32_between(value, -120.0, 0.0),
    )?;
    let floor_db = config.visualizer.floor_db;
    config.visualizer.ceiling_db = prompt_value_validated(
        reader,
        writer,
        "visualizer.ceiling_db",
        "Audio at or above this dBFS level maps to maximum brightness. 0 dBFS preserves the full input range; moving it downward makes the show reach full brightness sooner. It must be above floor_db and no higher than 0 dBFS.",
        config.visualizer.ceiling_db,
        defaults.visualizer.ceiling_db,
        |value| parse_f32_between(value, -120.0, 0.0),
        move |value| {
            if *value <= floor_db {
                bail!("ceiling_db must be above the selected floor_db ({floor_db})");
            }
            Ok(())
        },
    )?;
    config.visualizer.brightness_min = prompt_value(
        reader,
        writer,
        "visualizer.brightness_min",
        "WiZ dimming percentage used for silence or audio at floor_db. Keeping it above zero avoids rapid on/off commands. Valid range: 1–100.",
        config.visualizer.brightness_min,
        defaults.visualizer.brightness_min,
        |value| parse_range(value, 1, 100),
    )?;
    let brightness_min = config.visualizer.brightness_min;
    config.visualizer.brightness_max = prompt_value_validated(
        reader,
        writer,
        "visualizer.brightness_max",
        "WiZ dimming percentage used at ceiling_db and on the strongest peaks. It must be at least brightness_min and no greater than 100.",
        config.visualizer.brightness_max,
        defaults.visualizer.brightness_max,
        |value| parse_range(value, 1, 100),
        move |value| {
            if *value < brightness_min {
                bail!(
                    "brightness_max must be at least the selected brightness_min ({brightness_min})"
                );
            }
            Ok(())
        },
    )?;
    config.visualizer.attack_ms = prompt_value(
        reader,
        writer,
        "visualizer.attack_ms",
        "How quickly smoothed brightness rises toward a louder level. Small values make transients snap; larger values create gentler fades. Must be positive; 10–100 ms is a useful range.",
        config.visualizer.attack_ms,
        defaults.visualizer.attack_ms,
        parse_positive_f32,
    )?;
    config.visualizer.release_ms = prompt_value(
        reader,
        writer,
        "visualizer.release_ms",
        "How quickly brightness falls after audio becomes quieter. Longer release reduces flicker and produces flowing fades. Must be positive; 60–300 ms is a useful range.",
        config.visualizer.release_ms,
        defaults.visualizer.release_ms,
        parse_positive_f32,
    )?;

    section(
        writer,
        "Beat response",
        "Beat detection compares short-term energy with an adaptive recent baseline.",
    )?;
    config.visualizer.beat_threshold = prompt_value(
        reader,
        writer,
        "visualizer.beat_threshold",
        "Required energy ratio above the adaptive baseline. Lower values trigger more often; higher values favor only strong transients. Valid range: 1.0–5.0.",
        config.visualizer.beat_threshold,
        defaults.visualizer.beat_threshold,
        |value| parse_f32_between(value, 1.0, 5.0),
    )?;
    config.visualizer.beat_boost = prompt_value(
        reader,
        writer,
        "visualizer.beat_boost",
        "Temporary 0–1 intensity added directly to output brightness during the custom beat envelope. It bypasses attack smoothing for a sharper accent; 0 disables brightness emphasis.",
        config.visualizer.beat_boost,
        defaults.visualizer.beat_boost,
        |value| parse_f32_between(value, 0.0, 1.0),
    )?;
    config.visualizer.beat_cooldown_ms = prompt_value(
        reader,
        writer,
        "visualizer.beat_cooldown_ms",
        "Minimum time between detected beats. It prevents one kick or transient from firing repeatedly. Values around 120–250 ms work well for music. Valid range: 0–10000 ms.",
        config.visualizer.beat_cooldown_ms,
        defaults.visualizer.beat_cooldown_ms,
        |value| parse_range(value, 0, 10_000),
    )?;
    config.visualizer.beat_duration_ms = prompt_value(
        reader,
        writer,
        "visualizer.beat_duration_ms",
        "How long the application-controlled brightness boost remains active after a detected beat. The ordinary smoothed brightness continues underneath it. Valid range: 20–5000 ms.",
        config.visualizer.beat_duration_ms,
        defaults.visualizer.beat_duration_ms,
        |value| parse_range(value, 20, 5_000),
    )?;
    config.visualizer.rotate_colors_on_beat = prompt_value(
        reader,
        writer,
        "visualizer.rotate_colors_on_beat",
        "Move the currently detected chord-tone colors to the next light on each beat. This adds musical motion without introducing unrelated colors and has no visible effect with one light.",
        config.visualizer.rotate_colors_on_beat,
        defaults.visualizer.rotate_colors_on_beat,
        parse_bool,
    )?;

    section(
        writer,
        "Harmonic color and spatial motion",
        "Pitch mode maps detected notes to stable palette colors. Drift mode preserves the original continuously moving effect.",
    )?;
    config.visualizer.color_mode = prompt_value(
        reader,
        writer,
        "visualizer.color_mode",
        "Color-selection strategy. 'pitch' maps the twelve pitch classes C through B around the palette and spreads chord tones across lights. 'drift' uses automatic palette motion and coarse spectral balance.",
        config.visualizer.color_mode,
        defaults.visualizer.color_mode,
        parse_color_mode,
    )?;
    config.visualizer.palette = prompt_custom(
        reader,
        writer,
        "visualizer.palette",
        "Two or more six-digit RGB colors, separated by commas. Adjacent colors are smoothly interpolated and the last wraps to the first. Example: #ff0055, #642cff, #008cff.",
        &config.visualizer.palette,
        &defaults.visualizer.palette,
        |colors| colors.join(", "),
        parse_palette,
    )?;
    config.visualizer.pitch_smoothing_ms = prompt_value(
        reader,
        writer,
        "visualizer.pitch_smoothing_ms",
        "Time used to stabilize detected pitch energy. Lower values follow note changes faster; higher values resist flicker and percussion. Valid range: 10–2000 ms.",
        config.visualizer.pitch_smoothing_ms,
        defaults.visualizer.pitch_smoothing_ms,
        |value| parse_f32_between(value, 10.0, 2_000.0),
    )?;
    config.visualizer.pitch_min_confidence = prompt_value(
        reader,
        writer,
        "visualizer.pitch_min_confidence",
        "Confidence level at which pitch tracking reaches full response speed. Below it, colors still follow the likely harmony more gently; silence alone freezes color. Lower values are quicker, while higher values give uncertain mixes more smoothing. Valid range: 0–1.",
        config.visualizer.pitch_min_confidence,
        defaults.visualizer.pitch_min_confidence,
        |value| parse_f32_between(value, 0.0, 1.0),
    )?;
    config.visualizer.color_speed = prompt_value(
        reader,
        writer,
        "visualizer.color_speed",
        "Automatic palette revolutions per second in drift mode. 0 stops time-based color travel while spectral response remains active; negative values reverse direction. Ignored in pitch mode.",
        config.visualizer.color_speed,
        defaults.visualizer.color_speed,
        parse_finite_f32,
    )?;
    config.visualizer.color_influence = prompt_value(
        reader,
        writer,
        "visualizer.color_influence",
        "How far bass/mid/treble balance moves the palette position in drift mode. 0 makes drift color independent of frequency; negative values reverse the direction. Ignored in pitch mode.",
        config.visualizer.color_influence,
        defaults.visualizer.color_influence,
        parse_finite_f32,
    )?;
    config.visualizer.spatial_spread = prompt_value(
        reader,
        writer,
        "visualizer.spatial_spread",
        "Palette offset between adjacent lights in drift mode, measured in revolutions. Negative values reverse the light order. Pitch mode instead distributes chord tones, so this is ignored there.",
        config.visualizer.spatial_spread,
        defaults.visualizer.spatial_spread,
        parse_finite_f32,
    )?;
    config.visualizer.change_threshold = prompt_value(
        reader,
        writer,
        "visualizer.change_threshold",
        "Minimum summed RGB-channel difference before another frame is sent. Higher values reduce Wi-Fi traffic but can make gradients step; 0 sends every scheduled frame. Brightness changes of 2% send independently, and unchanged lights receive a keepalive every two seconds.",
        config.visualizer.change_threshold,
        defaults.visualizer.change_threshold,
        |value| parse_range(value, 0, u16::MAX),
    )?;

    section(
        writer,
        "Native WiZ beat pulse",
        "Normal animation always uses setPilot. These optional settings add WiZ's pulse command on detected beats.",
    )?;
    config.visualizer.pulse_on_beat = prompt_value(
        reader,
        writer,
        "visualizer.pulse_on_beat",
        "Also send the firmware-native pulse effect on detected beats. This is enabled by default and intentionally stacks with Light Wizard's own timed brightness envelope for sharper accents. Appearance varies by light model.",
        config.visualizer.pulse_on_beat,
        defaults.visualizer.pulse_on_beat,
        parse_bool,
    )?;
    config.visualizer.pulse_delta = prompt_value(
        reader,
        writer,
        "visualizer.pulse_delta",
        "Signed brightness change requested by the pulse command. Positive values flash brighter and negative values dip darker. Used only when pulse_on_beat is enabled. Valid range: -100–100.",
        config.visualizer.pulse_delta,
        defaults.visualizer.pulse_delta,
        |value| parse_range(value, -100, 100),
    )?;
    config.visualizer.pulse_duration_ms = prompt_value(
        reader,
        writer,
        "visualizer.pulse_duration_ms",
        "Duration of the native pulse. Short values feel percussive; long values overlap more of the music. Used only when pulse_on_beat is enabled. Valid range: 20–5000 ms.",
        config.visualizer.pulse_duration_ms,
        defaults.visualizer.pulse_duration_ms,
        |value| parse_range(value, 20, 5_000),
    )?;

    section(
        writer,
        "Color-cycle mode",
        "Color-cycle mode keeps every selected light on and jumps through an independent color spectrum without capturing audio.",
    )?;
    config.color_cycle.frequency_hz = prompt_value(
        reader,
        writer,
        "color_cycle.frequency_hz",
        "Hard color changes per second for each light. High rates are best-effort because bulb firmware and Wi-Fi conditions vary. Rapid changes from 3 through 30 Hz may trigger photosensitive seizures. Valid range: 0.1–30 Hz.",
        config.color_cycle.frequency_hz,
        defaults.color_cycle.frequency_hz,
        |value| parse_f32_between(value, 0.1, 30.0),
    )?;
    config.color_cycle.palette = prompt_custom(
        reader,
        writer,
        "color_cycle.palette",
        "Two or more six-digit RGB colors, separated by commas. The mode jumps through them in order and wraps from the last back to the first.",
        &config.color_cycle.palette,
        &defaults.color_cycle.palette,
        |colors| colors.join(", "),
        parse_palette,
    )?;
    config.color_cycle.brightness = prompt_value(
        reader,
        writer,
        "color_cycle.brightness",
        "Constant brightness used while colors change. Lights are never deliberately powered off. Valid range: 1–100%.",
        config.color_cycle.brightness,
        defaults.color_cycle.brightness,
        |value| parse_range(value, 1, 100),
    )?;
    config.color_cycle.pattern = prompt_value(
        reader,
        writer,
        "color_cycle.pattern",
        "Palette relationship between lights: sync uses the same color, alternate offsets odd/even lights by half the palette, and chase distributes starting colors across the palette.",
        config.color_cycle.pattern,
        defaults.color_cycle.pattern,
        parse_color_cycle_pattern,
    )?;

    Ok(config)
}

fn section<W: Write>(writer: &mut W, title: &str, description: &str) -> Result<()> {
    writeln!(writer, "\n=== {title} ===")?;
    writeln!(writer, "{description}")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prompt_custom<T, R, W, Render, Parse>(
    reader: &mut R,
    writer: &mut W,
    name: &str,
    description: &str,
    current: &T,
    default: &T,
    render: Render,
    parse: Parse,
) -> Result<T>
where
    T: Clone,
    R: BufRead,
    W: Write,
    Render: Fn(&T) -> String,
    Parse: Fn(&str) -> Result<T>,
{
    prompt_custom_validated(
        reader,
        writer,
        name,
        description,
        current,
        default,
        render,
        parse,
        |_| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn prompt_custom_validated<T, R, W, Render, Parse, Validate>(
    reader: &mut R,
    writer: &mut W,
    name: &str,
    description: &str,
    current: &T,
    default: &T,
    render: Render,
    parse: Parse,
    validate: Validate,
) -> Result<T>
where
    T: Clone,
    R: BufRead,
    W: Write,
    Render: Fn(&T) -> String,
    Parse: Fn(&str) -> Result<T>,
    Validate: Fn(&T) -> Result<()>,
{
    writeln!(writer, "\n{name}")?;
    writeln!(writer, "  {description}")?;
    writeln!(writer, "  Current: {}", render(current))?;
    writeln!(writer, "  Built-in default: {}", render(default))?;
    loop {
        write!(writer, "  New value [keep current]: ")?;
        writer.flush()?;
        let input = read_line(reader)?;
        let input = input.trim();
        let candidate = if input.is_empty() {
            Ok(current.clone())
        } else if input.eq_ignore_ascii_case("default") {
            Ok(default.clone())
        } else {
            parse(input)
        };
        match candidate.and_then(|value| validate(&value).map(|()| value)) {
            Ok(value) => return Ok(value),
            Err(error) => writeln!(writer, "  Invalid value: {error}")?,
        }
    }
}

fn prompt_value<T, R, W, Parse>(
    reader: &mut R,
    writer: &mut W,
    name: &str,
    description: &str,
    current: T,
    default: T,
    parse: Parse,
) -> Result<T>
where
    T: Clone + Display,
    R: BufRead,
    W: Write,
    Parse: Fn(&str) -> Result<T>,
{
    prompt_custom(
        reader,
        writer,
        name,
        description,
        &current,
        &default,
        ToString::to_string,
        parse,
    )
}

#[allow(clippy::too_many_arguments)]
fn prompt_value_validated<T, R, W, Parse, Validate>(
    reader: &mut R,
    writer: &mut W,
    name: &str,
    description: &str,
    current: T,
    default: T,
    parse: Parse,
    validate: Validate,
) -> Result<T>
where
    T: Clone + Display,
    R: BufRead,
    W: Write,
    Parse: Fn(&str) -> Result<T>,
    Validate: Fn(&T) -> Result<()>,
{
    prompt_custom_validated(
        reader,
        writer,
        name,
        description,
        &current,
        &default,
        ToString::to_string,
        parse,
        validate,
    )
}

fn confirm<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    question: &str,
    default: bool,
) -> Result<bool> {
    loop {
        write!(
            writer,
            "{question} [{}]: ",
            if default { "Y/n" } else { "y/N" }
        )?;
        writer.flush()?;
        let input = read_line(reader)?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match parse_bool(input.trim()) {
            Ok(value) => return Ok(value),
            Err(error) => writeln!(writer, "Invalid response: {error}")?,
        }
    }
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        bail!("configuration wizard cancelled because input was closed");
    }
    Ok(line)
}

fn parse_range<T>(input: &str, minimum: T, maximum: T) -> Result<T>
where
    T: FromStr + PartialOrd + Display + Copy,
    T::Err: Display,
{
    let value = input
        .parse::<T>()
        .map_err(|error| anyhow!("expected a number: {error}"))?;
    if value < minimum || value > maximum {
        bail!("expected a value from {minimum} through {maximum}");
    }
    Ok(value)
}

fn parse_finite_f32(input: &str) -> Result<f32> {
    let value = input
        .parse::<f32>()
        .map_err(|error| anyhow!("expected a decimal number: {error}"))?;
    if !value.is_finite() {
        bail!("the number must be finite");
    }
    Ok(value)
}

fn parse_positive_f32(input: &str) -> Result<f32> {
    let value = parse_finite_f32(input)?;
    if value <= 0.0 {
        bail!("the number must be greater than zero");
    }
    Ok(value)
}

fn parse_f32_between(input: &str, minimum: f32, maximum: f32) -> Result<f32> {
    let value = parse_finite_f32(input)?;
    if value < minimum || value > maximum {
        bail!("expected a value from {minimum} through {maximum}");
    }
    Ok(value)
}

fn parse_bool(input: &str) -> Result<bool> {
    match input.to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "on" | "1" => Ok(true),
        "n" | "no" | "false" | "off" | "0" => Ok(false),
        _ => bail!("enter yes or no"),
    }
}

fn parse_fft_size(input: &str) -> Result<usize> {
    let value: usize = parse_range(input, 256, 16_384)?;
    if !value.is_power_of_two() {
        bail!("FFT size must be a power of two");
    }
    Ok(value)
}

fn parse_color_mode(input: &str) -> Result<ColorMode> {
    input.parse().map_err(anyhow::Error::msg)
}

fn parse_color_cycle_pattern(input: &str) -> Result<ColorCyclePattern> {
    input.parse().map_err(anyhow::Error::msg)
}

fn parse_ip_list(input: &str) -> Result<Vec<Ipv4Addr>> {
    if input.eq_ignore_ascii_case("auto") || input.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut addresses = input
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<Ipv4Addr>()
                .map_err(|error| anyhow!("invalid IPv4 address {part:?}: {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if addresses.is_empty() {
        bail!("enter one or more IPv4 addresses, or 'auto'");
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

fn format_ip_list(addresses: &[Ipv4Addr]) -> String {
    if addresses.is_empty() {
        "auto-discover".into()
    } else {
        addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_optional_ip_list(addresses: &[Ipv4Addr]) -> String {
    if addresses.is_empty() {
        "none".into()
    } else {
        addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn parse_palette(input: &str) -> Result<Vec<String>> {
    let colors = input
        .split(',')
        .map(str::trim)
        .filter(|color| !color.is_empty())
        .map(|color| {
            parse_hex_color(color)?;
            let hex = color.strip_prefix('#').unwrap_or(color);
            Ok(format!("#{}", hex.to_ascii_lowercase()))
        })
        .collect::<Result<Vec<_>>>()?;
    if colors.len() < 2 {
        bail!("the palette needs at least two colors");
    }
    Ok(colors)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn blank_answers_preserve_every_default() {
        let expected = AppConfig::default();
        let mut input = Cursor::new("\n".repeat(64));
        let mut output = Vec::new();
        let configured = configure(&mut input, &mut output, expected.clone()).unwrap();
        assert_eq!(configured.to_toml().unwrap(), expected.to_toml().unwrap());
    }

    #[test]
    fn invalid_prompt_value_retries() {
        let mut input = Cursor::new("50\n20\n");
        let mut output = Vec::new();
        let value = prompt_value(
            &mut input,
            &mut output,
            "fps",
            "test",
            15_u32,
            15_u32,
            |input| parse_range(input, 1, 30),
        )
        .unwrap();
        assert_eq!(value, 20);
        assert!(String::from_utf8(output).unwrap().contains("Invalid value"));
    }

    #[test]
    fn parses_and_deduplicates_light_addresses() {
        let addresses = parse_ip_list("192.168.1.11, 192.168.1.10 192.168.1.11").unwrap();
        assert_eq!(
            addresses,
            vec![
                "192.168.1.10".parse::<Ipv4Addr>().unwrap(),
                "192.168.1.11".parse::<Ipv4Addr>().unwrap()
            ]
        );
        assert!(parse_ip_list("auto").unwrap().is_empty());
    }

    #[test]
    fn palette_is_normalized_and_validated() {
        assert_eq!(
            parse_palette("FF0055, #00aAeE").unwrap(),
            vec!["#ff0055", "#00aaee"]
        );
        assert!(parse_palette("#ff0055").is_err());
    }

    #[test]
    fn color_cycle_pattern_is_case_insensitive() {
        assert_eq!(
            parse_color_cycle_pattern("ChAsE").unwrap(),
            ColorCyclePattern::Chase
        );
        assert!(parse_color_cycle_pattern("random").is_err());
    }
}
