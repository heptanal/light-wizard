# Light Wizard

Light Wizard is a Rust controller for WiZ lights. It turns either all system
audio playing on a Mac or one local audio file into a synchronized light show
over the local network. Audio never leaves the computer.

The current visualizer includes:

- macOS system-output capture through ScreenCaptureKit (no loopback driver);
- a built-in single-file player for MP3, FLAC, WAV, Ogg Vorbis, Ogg Opus, and
  MP4/AAC;
- LAN discovery and explicit-IP fallback for WiZ lights;
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
  does not use ScreenCaptureKit and does not require that permission.

## Run it

First check LAN discovery without requesting audio permission:

```sh
light-wizard --discover-only
```

Then start the visualizer:

```sh
light-wizard
```

Or play and visualize one local file through the default system output device:

```sh
light-wizard --audio-file song.mp3
```

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
light-wizard --config-wizard
```

The wizard writes `light-wizard.toml`. To create or edit another file, combine
it with `--config`, for example:

```sh
light-wizard --config studio.toml --config-wizard
```

macOS should request Screen & System Audio Recording permission on the first
system-audio run. If capture does not begin after granting it, restart the
terminal and run the command again. Play some audio and press Ctrl+C to stop;
by default, Light Wizard restores the state it read from each light before the
show. The built-in file player never requests this permission.

If broadcast discovery is blocked by the router or a VPN, provide the light
addresses directly:

```sh
light-wizard --light 192.168.1.41 --light 192.168.1.42
```

MAC addresses are learned from `getPilot` replies and are not required. A
directed broadcast can also be added with `--broadcast 192.168.1.255`.

Useful experiments:

```sh
# Verify capture and analysis without touching any lights
light-wizard --dry-run

# Play a file and print its analysis without discovering or controlling lights
light-wizard --audio-file song.flac --dry-run

# More sensitive, lower-traffic, two-color pitch wheel
light-wizard --sensitivity 1.8 --fps 20 \
  --palette '#ff0055,#0055ff'

# Print every supported setting
light-wizard --print-default-config
```

Run `light-wizard --help` for all command-line overrides.

## Configuration

Copy [`light-wizard.example.toml`](light-wizard.example.toml) to
`light-wizard.toml`. That filename is loaded automatically from the current
directory, or another file can be selected with `--config path/to/file.toml`.
Command-line values override the file.

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
- `beat_threshold` and `beat_boost`: tune adaptive transient detection;
- `fps`: caps WiZ network updates. The default is the supported maximum of 30
  FPS for responsive harmonic and beat motion, while change suppression avoids
  resending imperceptibly small changes. Lower values reduce Wi-Fi traffic;
  visible improvement above 15–20 FPS depends on the bulb and network.

### File playback synchronization

`player.playback_delay_ms` defaults to 50 ms and may be overridden for one run
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

`pulse_on_beat` is off by default. When enabled, detected beats also emit WiZ's
native `pulse` command. Ordinary color/brightness animation uses `setPilot`, so
the visualizer works without `pulse` support.

The live terminal meter reports detected note names after `N`, followed by
tonal confidence, output RGB, and brightness. A `~` marks an uncertain but
still active estimate; `ONSET` and `BEAT` show transient events. Use `--dry-run`
to tune pitch settings without sending anything to the lights. In file mode,
`--dry-run` still plays the audio but skips light discovery and control.

A manual macOS playback test is:

```sh
light-wizard --audio-file song.mp3
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
classes, chord color behavior, FFT analysis, discovery broadcast math, audio
sample conversion, and safe restoration of RGB/temperature/scene states.
