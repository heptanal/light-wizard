use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};

use crate::{
    config::{InputReactiveConfig, parse_hex_color},
    visualizer::LightFrame,
    wiz::{WizClient, WizLight},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    KeyPress,
    MouseClick,
}

#[derive(Debug, Default, Clone, Copy)]
struct ActivityBatch {
    key_presses: u32,
    mouse_clicks: u32,
}

impl ActivityBatch {
    fn record(&mut self, event: InputEvent) {
        match event {
            InputEvent::KeyPress => self.key_presses = self.key_presses.saturating_add(1),
            InputEvent::MouseClick => self.mouse_clicks = self.mouse_clicks.saturating_add(1),
        }
    }

    fn is_empty(self) -> bool {
        self.key_presses == 0 && self.mouse_clicks == 0
    }
}

pub struct InputMapper {
    config: InputReactiveConfig,
    palette: Vec<[f32; 3]>,
    last_update: Instant,
    activity: f32,
    palette_phase: f32,
}

impl InputMapper {
    pub fn new(config: &InputReactiveConfig) -> Self {
        Self::new_at(config, Instant::now())
    }

    fn new_at(config: &InputReactiveConfig, now: Instant) -> Self {
        let palette = config
            .palette
            .iter()
            .map(|color| parse_hex_color(color).expect("validated input-reactive palette"))
            .map(|color| color.map(|channel| channel as f32))
            .collect();
        Self {
            config: config.clone(),
            palette,
            last_update: now,
            activity: 0.0,
            palette_phase: 0.0,
        }
    }

    fn render(&mut self, events: ActivityBatch, light_count: usize) -> Vec<LightFrame> {
        self.render_at(events, light_count, Instant::now())
    }

    fn render_at(
        &mut self,
        events: ActivityBatch,
        light_count: usize,
        now: Instant,
    ) -> Vec<LightFrame> {
        let elapsed = now.duration_since(self.last_update).as_secs_f32();
        self.last_update = now;

        let release_seconds = self.config.release_ms / 1_000.0;
        self.activity *= (-elapsed / release_seconds).exp();
        self.activity += events.key_presses as f32 * self.config.key_boost
            + events.mouse_clicks as f32 * self.config.click_boost;
        self.activity = self.activity.clamp(0.0, 1.0);

        let phase_speed =
            self.config.color_speed + self.activity * self.config.activity_color_speed;
        self.palette_phase = (self.palette_phase + elapsed * phase_speed).rem_euclid(1.0);

        let brightness_span = self.config.brightness_max - self.config.brightness_min;
        let dimming = (self.config.brightness_min as f32 + brightness_span as f32 * self.activity)
            .round()
            .clamp(1.0, 100.0) as u8;
        (0..light_count)
            .map(|index| LightFrame {
                rgb: sample_palette(
                    &self.palette,
                    self.palette_phase + index as f32 * self.config.spatial_spread,
                ),
                dimming,
            })
            .collect()
    }

    fn activity(&self) -> f32 {
        self.activity
    }
}

pub fn run(
    client: Option<&WizClient>,
    lights: &[WizLight],
    config: &InputReactiveConfig,
    events: &Receiver<InputEvent>,
    running: &AtomicBool,
    quiet: bool,
) -> Result<()> {
    let frame_interval = Duration::from_secs_f64(1.0 / config.fps as f64);
    let output_count = lights.len().max(1);
    let mut mapper = InputMapper::new(config);
    let mut previous = vec![None; output_count];
    let mut pending = ActivityBatch::default();
    let mut total_keys = 0_u64;
    let mut total_clicks = 0_u64;
    let mut next_frame = Instant::now();
    let mut last_send = Instant::now() - Duration::from_secs(5);
    let mut last_status = Instant::now() - Duration::from_secs(1);

    while running.load(Ordering::Relaxed) {
        loop {
            match events.try_recv() {
                Ok(event) => {
                    pending.record(event);
                    match event {
                        InputEvent::KeyPress => total_keys = total_keys.saturating_add(1),
                        InputEvent::MouseClick => total_clicks = total_clicks.saturating_add(1),
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    if running.load(Ordering::Relaxed) {
                        bail!("macOS input monitoring stopped unexpectedly");
                    }
                    return Ok(());
                }
            }
        }

        let now = Instant::now();
        if now < next_frame {
            thread::sleep((next_frame - now).min(Duration::from_millis(10)));
            continue;
        }
        next_frame = now + frame_interval;
        let had_input = !pending.is_empty();
        let outputs = mapper.render(pending, output_count);
        pending = ActivityBatch::default();

        let keepalive = now.duration_since(last_send) >= Duration::from_secs(2);
        let mut sent_any = false;
        if let Some(client) = client {
            for (index, (light, output)) in lights.iter().zip(&outputs).enumerate() {
                let changed = previous[index]
                    .is_none_or(|old| output.differs_from(old, config.change_threshold));
                if changed || keepalive {
                    client.send_pilot_one(light, output.rgb, output.dimming)?;
                    previous[index] = Some(*output);
                    sent_any = true;
                }
            }
        }
        if sent_any {
            last_send = now;
        }

        let status_due = now.duration_since(last_status) >= Duration::from_millis(100);
        if !quiet && (status_due || had_input) {
            print_status(
                mapper.activity(),
                outputs[0],
                total_keys,
                total_clicks,
                output_count,
            );
            last_status = now;
        }
    }
    Ok(())
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

fn print_status(
    activity: f32,
    output: LightFrame,
    total_keys: u64,
    total_clicks: u64,
    light_count: usize,
) {
    const WIDTH: usize = 24;
    let filled = (activity * WIDTH as f32).round() as usize;
    let meter = format!("{}{}", "█".repeat(filled), "░".repeat(WIDTH - filled));
    print!(
        "\r\x1b[2KInput [{meter}] {:>3.0}%  keys {total_keys:<6} clicks {total_clicks:<6}  #{:02x}{:02x}{:02x} {:>3}%  lights {light_count}",
        activity * 100.0,
        output.rgb[0],
        output.rgb[1],
        output.rgb[2],
        output.dimming,
    );
    let _ = io::stdout().flush();
}

#[cfg(target_os = "macos")]
pub struct HostInputCapture {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
impl HostInputCapture {
    pub fn ensure_access() -> Result<()> {
        if unsafe { CGPreflightListenEventAccess() } {
            return Ok(());
        }
        println!("Requesting macOS Input Monitoring permission...");
        if unsafe { CGRequestListenEventAccess() } {
            Ok(())
        } else {
            bail!(
                "macOS did not grant input monitoring access. Open System Settings > Privacy & Security > Input Monitoring, enable your terminal or Light Wizard, then restart it"
            )
        }
    }

    pub fn start(sender: Sender<InputEvent>, running: Arc<AtomicBool>) -> Result<HostInputCapture> {
        use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
        use core_graphics::event::{
            CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
            CallbackResult, EventField,
        };

        Self::ensure_access()?;
        let stop = Arc::new(AtomicBool::new(false));
        let capture_stop = Arc::clone(&stop);
        let (startup_sender, startup_receiver) = crossbeam_channel::bounded(1);
        let thread = thread::Builder::new()
            .name("light-wizard-input".into())
            .spawn(move || {
                let tap = CGEventTap::new(
                    CGEventTapLocation::HID,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![
                        CGEventType::KeyDown,
                        CGEventType::LeftMouseDown,
                        CGEventType::RightMouseDown,
                        CGEventType::OtherMouseDown,
                    ],
                    move |_proxy, event_type, event| {
                        let is_repeat = event_type_is_key_down(event_type)
                            && event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT)
                                != 0;
                        if let Some(input) = classify_event(event_type, is_repeat) {
                            let _ = sender.try_send(input);
                        }
                        CallbackResult::Keep
                    },
                );
                let Ok(tap) = tap else {
                    let _ = startup_sender.send(Err(
                        "macOS refused to create the listen-only input event tap".to_owned(),
                    ));
                    return;
                };
                let Ok(loop_source) = tap.mach_port().create_runloop_source(0) else {
                    let _ = startup_sender.send(Err(
                        "could not create the input event run-loop source".to_owned(),
                    ));
                    return;
                };
                let run_loop = CFRunLoop::get_current();
                let mode = unsafe { kCFRunLoopDefaultMode };
                run_loop.add_source(&loop_source, mode);
                tap.enable();
                if startup_sender.send(Ok(())).is_err() {
                    return;
                }
                while running.load(Ordering::Relaxed) && !capture_stop.load(Ordering::Relaxed) {
                    CFRunLoop::run_in_mode(mode, Duration::from_millis(50), true);
                }
                run_loop.remove_source(&loop_source, mode);
            })
            .context("failed to start the macOS input-monitoring thread")?;

        match startup_receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!(
                    "{message}. Open System Settings > Privacy & Security > Input Monitoring, enable your terminal or Light Wizard, then restart it"
                )
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                bail!("timed out while starting macOS input monitoring")
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for HostInputCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn event_type_is_key_down(event_type: core_graphics::event::CGEventType) -> bool {
    matches!(event_type, core_graphics::event::CGEventType::KeyDown)
}

#[cfg(target_os = "macos")]
fn classify_event(
    event_type: core_graphics::event::CGEventType,
    is_repeat: bool,
) -> Option<InputEvent> {
    use core_graphics::event::CGEventType;
    match event_type {
        CGEventType::KeyDown if !is_repeat => Some(InputEvent::KeyPress),
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            Some(InputEvent::MouseClick)
        }
        _ => None,
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[cfg(not(target_os = "macos"))]
pub struct HostInputCapture;

#[cfg(not(target_os = "macos"))]
impl HostInputCapture {
    pub fn ensure_access() -> Result<()> {
        bail!("host input monitoring is currently implemented only for macOS")
    }

    pub fn start(
        _sender: Sender<InputEvent>,
        _running: Arc<AtomicBool>,
    ) -> Result<HostInputCapture> {
        bail!("host input monitoring is currently implemented only for macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> InputReactiveConfig {
        InputReactiveConfig {
            palette: vec!["#ff0000".into(), "#00ff00".into(), "#0000ff".into()],
            color_speed: 0.0,
            activity_color_speed: 0.0,
            spatial_spread: 0.0,
            ..InputReactiveConfig::default()
        }
    }

    #[test]
    fn key_and_click_boosts_accumulate_and_clamp() {
        let config = test_config();
        let started = Instant::now();
        let mut mapper = InputMapper::new_at(&config, started);
        mapper.render_at(
            ActivityBatch {
                key_presses: 2,
                mouse_clicks: 1,
            },
            1,
            started,
        );
        assert!((mapper.activity() - 0.49).abs() < 0.0001);
        mapper.render_at(
            ActivityBatch {
                key_presses: 10,
                mouse_clicks: 10,
            },
            1,
            started,
        );
        assert_eq!(mapper.activity(), 1.0);
    }

    #[test]
    fn activity_decays_exponentially() {
        let mut config = test_config();
        config.release_ms = 1_000.0;
        let started = Instant::now();
        let mut mapper = InputMapper::new_at(&config, started);
        mapper.render_at(
            ActivityBatch {
                key_presses: 9,
                mouse_clicks: 0,
            },
            1,
            started,
        );
        assert_eq!(mapper.activity(), 1.0);
        mapper.render_at(
            ActivityBatch::default(),
            1,
            started + Duration::from_secs(1),
        );
        assert!((mapper.activity() - std::f32::consts::E.recip()).abs() < 0.0001);
    }

    #[test]
    fn activity_maps_to_the_configured_brightness_range() {
        let mut config = test_config();
        config.brightness_min = 10;
        config.brightness_max = 90;
        config.key_boost = 0.5;
        let started = Instant::now();
        let mut mapper = InputMapper::new_at(&config, started);
        let idle = mapper.render_at(ActivityBatch::default(), 1, started);
        assert_eq!(idle[0].dimming, 10);
        let active = mapper.render_at(
            ActivityBatch {
                key_presses: 1,
                mouse_clicks: 0,
            },
            1,
            started,
        );
        assert_eq!(active[0].dimming, 50);
    }

    #[test]
    fn activity_accelerates_palette_motion() {
        let mut config = test_config();
        config.color_speed = 0.1;
        config.activity_color_speed = 0.4;
        config.release_ms = 10_000.0;
        let started = Instant::now();
        let mut mapper = InputMapper::new_at(&config, started);
        mapper.activity = 1.0;
        mapper.render_at(
            ActivityBatch::default(),
            1,
            started + Duration::from_secs(1),
        );
        assert!((mapper.palette_phase - 0.461_935).abs() < 0.001);
    }

    #[test]
    fn adjacent_lights_use_spatial_palette_offsets() {
        let mut config = test_config();
        config.spatial_spread = 1.0 / 3.0;
        let started = Instant::now();
        let mut mapper = InputMapper::new_at(&config, started);
        let outputs = mapper.render_at(ActivityBatch::default(), 3, started);
        assert_eq!(outputs[0].rgb, [255, 0, 0]);
        assert_eq!(outputs[1].rgb, [0, 255, 0]);
        assert_eq!(outputs[2].rgb, [0, 0, 255]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn input_classifier_ignores_repeats_and_unselected_events() {
        use core_graphics::event::CGEventType;
        assert_eq!(
            classify_event(CGEventType::KeyDown, false),
            Some(InputEvent::KeyPress)
        );
        assert_eq!(classify_event(CGEventType::KeyDown, true), None);
        assert_eq!(
            classify_event(CGEventType::LeftMouseDown, false),
            Some(InputEvent::MouseClick)
        );
        assert_eq!(classify_event(CGEventType::KeyUp, false), None);
        assert_eq!(classify_event(CGEventType::ScrollWheel, false), None);
        assert_eq!(classify_event(CGEventType::MouseMoved, false), None);
    }
}
