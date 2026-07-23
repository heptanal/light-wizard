# Light Wizard

Light Wizard is a local Rust controller for WiZ lights with explicit light
modes. It can turn system audio or a local audio file into a synchronized light
show, react to this Mac's key presses and mouse clicks, or cycle rapidly through
configurable color spectra in ways that are not available in the WiZ app.
Control stays on the LAN, and captured audio and input activity never leave the
computer.

Current modes and shared capabilities include:

- synchronized, alternate, and chase color cycles from 0.1 through an
  experimental 30 hard color changes per second;
- input-reactive brightness and palette motion driven by anonymous key presses
  and mouse clicks;
- macOS system-output capture through ScreenCaptureKit (no loopback driver);
- a built-in single-file player for MP3, FLAC, WAV, Ogg Vorbis, Ogg Opus, and
  MP4/AAC;
- LAN discovery and explicit-IP fallback for WiZ lights, with best-effort
  firmware module/version reporting;
- RMS volume, FFT bass/mid/treble analysis, 12-note chroma analysis, and
  adaptive beat detection;
- continuously tracked pitch-class colors, chord tones distributed across
  multiple lights, perceptual chord blending for one light, and adaptive
  confidence that keeps dense music moving without letting silence scramble it;
- spectral-onset response plus beat-driven chord colors that move between
  bulbs without introducing unrelated hues;
- a legacy drifting-palette mode plus configurable attack/release, brightness,
  sensitivity, and update rate;
- rate-limited, fire-and-forget WiZ UDP output suitable for real-time use;
- best-effort restoration of each light's previous state on Ctrl+C or when a
  file finishes.

## Requirements

- macOS 13 or newer;
- a Rust toolchain (`rustup` is recommended);
- WiZ lights and the Mac on the same IPv4 LAN;
- Screen & System Audio Recording permission for system-audio mode. File mode
  does not use ScreenCaptureKit and does not require that permission;
- Input Monitoring permission for `input-reactive` mode.

## Run it

Running `light-wizard` without a mode shows the command help. First check LAN
discovery without requesting audio permission:

```sh
light-wizard discover
```

Then start the visualizer:

```sh
light-wizard visualizer
```

Or play and visualize one local file through the default system output device:

```sh
light-wizard visualizer --audio-file song.mp3
```

Or cycle through a custom spectrum at 20 color changes per second:

```sh
light-wizard color-cycle --frequency-hz 20 \
  --palette '#ff0044,#00ff88,#2200ff' --brightness 100
```

Or turn typing and clicking activity into ambient light:

```sh
light-wizard input-reactive
```

Input-reactive mode keeps the lights on at a configurable ambient brightness.
Each non-repeating key press or mouse-button click adds activity energy; that
energy decays smoothly, controls brightness, and accelerates motion through the
palette. Key releases, key auto-repeat, typed characters, scrolling, cursor
movement, and the active application are never collected. The terminal only
shows aggregate event counts.

Color-cycle mode runs until Ctrl+C and never deliberately powers a light off.
Each frequency tick jumps to the next palette color without interpolation.
Its `sync` pattern gives every light the same color, `alternate` offsets odd and
even lights by half the palette, and `chase` distributes starting colors across
the palette. Frequency is always measured per light. The upper end of the 30 Hz
range is best-effort because visible timing depends on bulb firmware and Wi-Fi
conditions.

> **Photosensitivity warning:** Rapid light changes, especially in the 3–30 Hz
> range, may trigger seizures. Light Wizard prints a warning before running
> frequencies in that range.

File mode supports MP3, FLAC, WAV, Ogg Vorbis, mono/stereo Ogg Opus (`.opus`,
plus Opus streams in `.ogg`), and AAC audio in MP4/M4A containers. It preserves
the file's native channel layout for playback,
downmixes each decoded frame to mono for analysis, and analyzes at the file's
native sample rate. The system-selected output device and volume are used.
Playback exits automatically after the delayed audio finishes; Ctrl+C cancels
it. V1 intentionally has no playlist, pause, seek, loop, or application volume.
Opus is decoded through a bundled libopus adapter, so no system codec or
separate libopus installation is required.

For a guided setup that explains every field, shows both the current and
built-in default values, validates each answer, and previews the TOML before
saving it, run:

```sh
light-wizard configure
```

The wizard writes `light-wizard.toml`. To create or edit another file, combine
it with `--config`, for example:

```sh
light-wizard --config studio.toml configure
```

macOS should request Screen & System Audio Recording permission on the first
system-audio run. If capture does not begin after granting it, restart the
terminal and run the command again. Play some audio and press Ctrl+C to stop;
by default, Light Wizard restores the state it read from each light before the
show. The built-in file player never requests this permission.

The first `input-reactive` run requests macOS Input Monitoring permission. If
macOS does not immediately activate the event tap, enable the terminal or Light
Wizard under System Settings > Privacy & Security > Input Monitoring, restart
it, and run the mode again.

If broadcast discovery is blocked by the router or a VPN, provide the light
addresses directly:

```sh
light-wizard visualizer \
  --light 192.168.1.41 --light 192.168.1.42
```

`discover` follows its registration broadcast with best-effort
`getSystemConfig` queries. When the firmware supplies them, output includes the
MAC address, internal module identifier (for example `ESP03_SHRGB1W_01`), and
firmware version. The module identifier is not guaranteed to match the retail
name printed on the box. None of these details are required for control. A
directed broadcast can also be added to a mode or `discover` with
`--broadcast 192.168.1.255`.

Useful experiments:

```sh
# Verify capture and analysis without touching any lights
light-wizard visualizer --dry-run

# Play a file and print its analysis without discovering or controlling lights
light-wizard visualizer --audio-file song.flac --dry-run

# Preview color-cycle timing without discovering or controlling lights
light-wizard color-cycle --frequency-hz 15 --pattern alternate --dry-run

# Preview host-input activity without discovering or controlling lights
light-wizard input-reactive --dry-run

# More sensitive, lower-traffic, two-color pitch wheel
light-wizard visualizer --sensitivity 1.8 --fps 20 \
  --palette '#ff0055,#0055ff'

# Print every supported setting
light-wizard default-config
```

Run `light-wizard --help` for the mode list or
`light-wizard <mode> --help` for its overrides.

## Configuration

Copy [`light-wizard.example.toml`](light-wizard.example.toml) to
`light-wizard.toml`. That filename is loaded automatically from the current
directory, or another file can be selected with `--config path/to/file.toml`.
Command-line values override the file. Existing configurations without
`[input_reactive]` or `[color_cycle]` sections receive the corresponding
built-in defaults.

### Input-reactive configuration

The `[input_reactive]` section controls:

- `key_boost` and `click_boost`: normalized activity contributed by each event;
- `release_ms`: exponential activity decay time;
- `brightness_min` and `brightness_max`: dim ambient and full-activity output;
- `color_speed`: idle palette motion;
- `activity_color_speed`: additional palette speed at full activity;
- `palette` and `spatial_spread`: interpolated colors and the offset between
  adjacent lights;
- `fps` and `change_threshold`: network update ceiling and change suppression.

The event callback never blocks, events are accumulated between frames, and the
mode never sends WiZ's firmware-native `pulse` command. State restoration,
explicit light selection, dry-run behavior, and the two-second keepalive match
the other real-time modes.

### Color-cycle configuration

The `[color_cycle]` section controls:

- `frequency_hz`: hard color changes per second for every light, from 0.1 to 30;
- `palette`: two or more RGB colors visited in order and wrapped;
- `brightness`: constant brightness while every light remains powered on;
- `pattern`: `sync`, `alternate`, or `chase`.

Every pattern preserves the selected frequency for each bulb. The latter two
patterns only change palette offsets. Lights use stable IP ordering to determine
their alternate/chase positions. Previous power, color/temperature or scene,
and brightness are restored after Ctrl+C by default; `--no-restore` leaves the
last color active.

### Visualizer configuration

The main tuning controls are:

- `sensitivity`, `floor_db`, and `ceiling_db`: map quiet/loud audio to a 0–1
  intensity;
- `attack_ms` and `release_ms`: control how quickly light brightness rises and
  falls;
- `color_mode`: `pitch` (the default) maps detected notes to stable colors;
  `drift` restores the original time-driven effect;
- `palette`: forms a circular color wheel sampled at 12 positions from C
  through B. Chords spread their strongest tones across multiple lights; one
  light shows a weighted blend;
- `pitch_smoothing_ms` controls harmonic stability. `pitch_min_confidence` is
  the point where tracking reaches full speed; uncertain harmony continues to
  influence color gently, while silence holds the last harmonic color;
- `rotate_colors_on_beat`: moves the detected chord tones to the next bulbs on
  each beat. Single-light setups retain their blended chord color;
- `color_speed`, `color_influence`, and `spatial_spread`: control only the
  legacy `drift` mode;
- `beat_threshold` tunes adaptive detection; `beat_boost` and
  `beat_duration_ms` tune the custom brightness accent;
- `fps`: caps WiZ network updates. The default is the supported maximum of 30
  FPS for responsive harmonic and beat motion, while change suppression avoids
  resending imperceptibly small changes. Lower values reduce Wi-Fi traffic;
  visible improvement above 15–20 FPS depends on the bulb and network.

### File playback synchronization

`player.playback_delay_ms` defaults to 150 ms and may be overridden for one run
with `--playback-delay-ms MS` (which requires `--audio-file`). File mode uses a
single streaming decoder: each frame is sent to analysis immediately, while
the original interleaved samples wait in a sample-rate-aware bounded buffer.
The player first emits silence, then plays those exact samples after the chosen
delay, including draining the delayed tail at end of file. The whole song is
never loaded into memory, so long tracks do not accumulate clock drift.

Tune the delay by ear:

- increase it if audible events occur before the lights react;
- decrease it if the lights visibly lead the audio.

WiZ animation commands are fire-and-forget UDP. A network round-trip time does
not reveal when a bulb has visibly settled, so Light Wizard does not pretend it
can auto-calibrate this value. Wi-Fi conditions, bulb processing, Rodio's
output buffer (roughly 100 ms by default), and the selected audio device all
contribute latency.

Detected beats bypass the ordinary frame deadline and start Light Wizard's own
brightness envelope. `beat_boost` sets its normalized brightness jump and
`beat_duration_ms` controls how long it remains active; ordinary attack/release
smoothing continues underneath it. `pulse_on_beat` is on by default and also
emits WiZ's firmware-native `pulse` command. The two effects intentionally
stack for sharper accents, though native behavior can vary by light model.

The live terminal meter reports detected note names after `N`, followed by
tonal confidence, output RGB, and brightness. A `~` marks an uncertain but
still active estimate; `ONSET` and `BEAT` show transient events. Use `--dry-run`
to tune pitch settings without sending anything to the lights. In file mode,
`--dry-run` still plays the audio but skips light discovery and control.

A manual macOS playback test is:

```sh
light-wizard visualizer --audio-file song.mp3
```

## Verification

```sh
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```

The application has unit coverage for incremental Ogg Opus decoding, exact
file delay and EOF draining, native-rate duration, stereo downmix, bounded
streaming memory, configuration and CLI compatibility, all twelve pitch
classes, chord color behavior, FFT analysis, discovery broadcast math and
identity parsing, audio sample conversion, input-event classification and
energy mapping, color-cycle palette scheduling and WiZ payloads, custom beat
envelopes, and safe restoration of RGB/temperature/scene states.
