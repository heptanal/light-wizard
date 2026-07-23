use std::{
    io::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    config::{ColorCycleConfig, ColorCyclePattern, parse_hex_color},
    visualizer::LightFrame,
    wiz::{WizClient, WizLight},
};

pub fn run(
    client: Option<&WizClient>,
    lights: &[WizLight],
    config: &ColorCycleConfig,
    running: &AtomicBool,
    quiet: bool,
) -> Result<()> {
    let palette = config
        .palette
        .iter()
        .map(|color| parse_hex_color(color))
        .collect::<Result<Vec<_>>>()?;
    let output_count = lights.len().max(1);
    let mut previous = vec![None; output_count];
    let started = Instant::now();
    let mut last_status = started - Duration::from_secs(1);

    while running.load(Ordering::Relaxed) {
        let now = Instant::now();
        let elapsed = now.duration_since(started);
        let frames = (0..output_count)
            .map(|index| frame_at(config, &palette, elapsed, index, output_count))
            .collect::<Vec<_>>();

        let transitions = record_transitions(&mut previous, &frames);
        if let Some(client) = client {
            for (index, frame) in transitions {
                client.send_pilot_one(&lights[index], frame.rgb, frame.dimming)?;
            }
        }

        if !quiet && now.duration_since(last_status) >= Duration::from_millis(100) {
            print_status(config, frames[0], output_count);
            last_status = now;
        }

        let scheduler_elapsed = Instant::now().duration_since(started);
        thread::sleep(
            time_until_next_step(config.frequency_hz, scheduler_elapsed)
                .max(Duration::from_micros(100))
                .min(Duration::from_millis(10)),
        );
    }
    Ok(())
}

fn record_transitions(
    previous: &mut [Option<LightFrame>],
    current: &[LightFrame],
) -> Vec<(usize, LightFrame)> {
    previous
        .iter_mut()
        .zip(current)
        .enumerate()
        .filter_map(|(index, (previous, current))| {
            if *previous == Some(*current) {
                None
            } else {
                *previous = Some(*current);
                Some((index, *current))
            }
        })
        .collect()
}

fn palette_offset(
    pattern: ColorCyclePattern,
    index: usize,
    light_count: usize,
    palette_len: usize,
) -> usize {
    match pattern {
        ColorCyclePattern::Sync => 0,
        ColorCyclePattern::Alternate => {
            if index.is_multiple_of(2) {
                0
            } else {
                palette_len / 2
            }
        }
        ColorCyclePattern::Chase => index.saturating_mul(palette_len) / light_count.max(1),
    }
}

fn frame_at(
    config: &ColorCycleConfig,
    palette: &[[u8; 3]],
    elapsed: Duration,
    index: usize,
    light_count: usize,
) -> LightFrame {
    let step = (elapsed.as_secs_f64() * f64::from(config.frequency_hz)).floor() as usize;
    let offset = palette_offset(config.pattern, index, light_count, palette.len());
    LightFrame {
        rgb: palette[(step + offset) % palette.len()],
        dimming: config.brightness,
    }
}

fn time_until_next_step(frequency_hz: f32, elapsed: Duration) -> Duration {
    let frequency = f64::from(frequency_hz);
    let completed_steps = (elapsed.as_secs_f64() * frequency).floor();
    let next_step_at = (completed_steps + 1.0) / frequency;
    Duration::from_secs_f64((next_step_at - elapsed.as_secs_f64()).max(0.0))
}

fn print_status(config: &ColorCycleConfig, frame: LightFrame, total: usize) {
    print!(
        "\r\x1b[2KColor cycle {:>4.1} Hz  {}  colors {:>2}  #{:02x}{:02x}{:02x}  brightness {:>3}%  lights {total}",
        config.frequency_hz,
        config.pattern,
        config.palette.len(),
        frame.rgb[0],
        frame.rgb[1],
        frame.rgb[2],
        config.brightness,
    );
    let _ = io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: [[u8; 3]; 6] = [
        [255, 0, 0],
        [255, 255, 0],
        [0, 255, 0],
        [0, 255, 255],
        [0, 0, 255],
        [255, 0, 255],
    ];

    fn config(pattern: ColorCyclePattern) -> ColorCycleConfig {
        ColorCycleConfig {
            frequency_hz: 10.0,
            brightness: 75,
            pattern,
            ..ColorCycleConfig::default()
        }
    }

    #[test]
    fn frequency_counts_hard_color_changes_per_second() {
        let config = config(ColorCyclePattern::Sync);
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::ZERO, 0, 1).rgb,
            PALETTE[0]
        );
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::from_millis(101), 0, 1).rgb,
            PALETTE[1]
        );
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::from_millis(201), 0, 1).rgb,
            PALETTE[2]
        );
    }

    #[test]
    fn skipped_ticks_jump_to_the_current_absolute_step() {
        let config = config(ColorCyclePattern::Sync);
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::from_millis(501), 0, 1).rgb,
            PALETTE[5]
        );
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::from_millis(601), 0, 1).rgb,
            PALETTE[0]
        );
    }

    #[test]
    fn sync_uses_the_same_palette_index_for_every_light() {
        let config = config(ColorCyclePattern::Sync);
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::ZERO, 0, 4),
            frame_at(&config, &PALETTE, Duration::ZERO, 3, 4)
        );
    }

    #[test]
    fn alternate_offsets_odd_lights_by_half_the_palette() {
        let config = config(ColorCyclePattern::Alternate);
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::ZERO, 0, 4).rgb,
            PALETTE[0]
        );
        assert_eq!(
            frame_at(&config, &PALETTE, Duration::ZERO, 1, 4).rgb,
            PALETTE[3]
        );
    }

    #[test]
    fn chase_distributes_starting_colors_across_the_palette() {
        let config = config(ColorCyclePattern::Chase);
        let colors = (0..3)
            .map(|index| frame_at(&config, &PALETTE, Duration::ZERO, index, 3).rgb)
            .collect::<Vec<_>>();
        assert_eq!(colors, vec![PALETTE[0], PALETTE[2], PALETTE[4]]);
    }

    #[test]
    fn every_frame_keeps_the_configured_brightness() {
        let config = config(ColorCyclePattern::Chase);
        for index in 0..4 {
            assert_eq!(
                frame_at(&config, &PALETTE, Duration::from_millis(350), index, 4).dimming,
                75
            );
        }
    }

    #[test]
    fn unchanged_colors_do_not_create_more_transitions() {
        let red = LightFrame {
            rgb: [255, 0, 0],
            dimming: 75,
        };
        let blue = LightFrame {
            rgb: [0, 0, 255],
            dimming: 75,
        };
        let mut previous = vec![None, None];
        assert_eq!(
            record_transitions(&mut previous, &[red, blue]),
            vec![(0, red), (1, blue)]
        );
        assert!(record_transitions(&mut previous, &[red, blue]).is_empty());
    }

    #[test]
    fn next_step_uses_the_absolute_clock() {
        assert_eq!(
            time_until_next_step(10.0, Duration::ZERO),
            Duration::from_millis(100)
        );
        assert_eq!(
            time_until_next_step(10.0, Duration::from_millis(250)),
            Duration::from_millis(50)
        );
    }
}
