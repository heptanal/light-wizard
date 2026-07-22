use std::{
    collections::VecDeque,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::Sender;
use rodio::{
    ChannelCount, Decoder, Player, SampleRate, Source,
    stream::{DeviceSinkBuilder, MixerDeviceSink},
};
use symphonia::{
    core::{
        audio::{AudioBufferRef, SampleBuffer, SignalSpec},
        codecs::{CODEC_TYPE_OPUS, CodecRegistry, Decoder as SymphoniaDecoder, DecoderOptions},
        errors::Error as SymphoniaError,
        formats::{FormatOptions, FormatReader},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
        probe::Hint,
        units,
    },
    default::get_probe,
};
use symphonia_adapter_libopus::OpusDecoder;

const ANALYSIS_CHUNK_FRAMES: usize = 512;

#[derive(Debug, Clone)]
pub struct AudioFileMetadata {
    pub path: PathBuf,
    pub format: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Option<Duration>,
}

pub struct PreparedAudioFile {
    decoder: AudioDecoder,
    output: MixerDeviceSink,
    metadata: AudioFileMetadata,
    delay: Duration,
}

impl PreparedAudioFile {
    pub fn open(path: &Path, playback_delay_ms: u64) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open audio file {}", path.display()))?;
        let file_metadata = file.metadata().with_context(|| {
            format!("failed to read metadata for audio file {}", path.display())
        })?;
        if !file_metadata.is_file() {
            bail!("audio path {} is not a regular file", path.display());
        }

        let extension = path.extension().and_then(|value| value.to_str());
        let decoder = open_decoder(path, file, file_metadata.len(), extension).with_context(|| {
            format!(
                "unsupported or invalid audio file {}; expected MP3, FLAC, WAV, Vorbis, Ogg Opus, or MP4/AAC",
                path.display()
            )
        })?;

        let sample_rate = decoder.sample_rate().get();
        validate_source_sample_rate(sample_rate)?;
        let channels = decoder.channels().get();
        let metadata = AudioFileMetadata {
            path: path.to_path_buf(),
            format: decoder.format_name(extension),
            sample_rate,
            channels,
            duration: decoder.total_duration(),
        };

        let mut output = DeviceSinkBuilder::from_default_device()
            .context("no default audio output device is available; select one and try again")?
            .open_stream()
            .context("failed to open the default audio output device")?;
        output.log_on_drop(false);

        Ok(Self {
            decoder,
            output,
            metadata,
            delay: Duration::from_millis(playback_delay_ms),
        })
    }

    pub fn metadata(&self) -> &AudioFileMetadata {
        &self.metadata
    }

    pub fn start(self, analysis: Sender<Vec<f32>>) -> FilePlayback {
        let completed = Arc::new(AtomicBool::new(false));
        let source =
            LookaheadSource::new(self.decoder, self.delay, analysis, Arc::clone(&completed));
        let player = Player::connect_new(self.output.mixer());
        player.append(source);
        FilePlayback {
            _output: self.output,
            player,
            completed,
        }
    }
}

enum AudioDecoder {
    Rodio(Decoder<BufReader<File>>),
    Opus(OpusSource),
}

impl AudioDecoder {
    fn format_name(&self, extension: Option<&str>) -> String {
        match self {
            Self::Opus(_) => "Ogg Opus".into(),
            Self::Rodio(_) => format_name(extension),
        }
    }
}

impl Iterator for AudioDecoder {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Rodio(decoder) => decoder.next(),
            Self::Opus(decoder) => decoder.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Rodio(decoder) => decoder.size_hint(),
            Self::Opus(decoder) => decoder.size_hint(),
        }
    }
}

impl Source for AudioDecoder {
    fn current_span_len(&self) -> Option<usize> {
        match self {
            Self::Rodio(decoder) => decoder.current_span_len(),
            Self::Opus(decoder) => decoder.current_span_len(),
        }
    }

    fn channels(&self) -> ChannelCount {
        match self {
            Self::Rodio(decoder) => decoder.channels(),
            Self::Opus(decoder) => decoder.channels(),
        }
    }

    fn sample_rate(&self) -> SampleRate {
        match self {
            Self::Rodio(decoder) => decoder.sample_rate(),
            Self::Opus(decoder) => decoder.sample_rate(),
        }
    }

    fn total_duration(&self) -> Option<Duration> {
        match self {
            Self::Rodio(decoder) => decoder.total_duration(),
            Self::Opus(decoder) => decoder.total_duration(),
        }
    }
}

fn open_decoder(
    path: &Path,
    file: File,
    byte_len: u64,
    extension: Option<&str>,
) -> Result<AudioDecoder> {
    if extension.is_some_and(|extension| extension.eq_ignore_ascii_case("opus")) {
        return OpusSource::new(file, extension).map(AudioDecoder::Opus);
    }

    match open_rodio_decoder(file, byte_len, extension) {
        Ok(decoder) => Ok(AudioDecoder::Rodio(decoder)),
        Err(rodio_error)
            if extension.is_some_and(|extension| {
                extension.eq_ignore_ascii_case("ogg") || extension.eq_ignore_ascii_case("oga")
            }) =>
        {
            let file = File::open(path)
                .with_context(|| format!("failed to reopen audio file {}", path.display()))?;
            OpusSource::new(file, extension)
                .map(AudioDecoder::Opus)
                .map_err(|_| rodio_error)
        }
        Err(error) => Err(error),
    }
}

fn open_rodio_decoder(
    file: File,
    byte_len: u64,
    extension: Option<&str>,
) -> Result<Decoder<BufReader<File>>> {
    let mut builder = Decoder::builder()
        .with_data(BufReader::new(file))
        .with_byte_len(byte_len)
        .with_seekable(true);
    if let Some(extension) = extension {
        builder = builder.with_hint(extension);
    }
    builder.build().map_err(Into::into)
}

struct OpusSource {
    decoder: Box<dyn SymphoniaDecoder>,
    format: Box<dyn FormatReader>,
    track_id: u32,
    total_duration: Option<Duration>,
    buffer: SampleBuffer<f32>,
    buffer_offset: usize,
    spec: SignalSpec,
}

impl OpusSource {
    fn new(file: File, extension: Option<&str>) -> Result<Self> {
        let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(extension) = extension {
            hint.with_extension(extension);
        }
        let mut probed = get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .context("failed to read the Opus container")?;

        let track = probed
            .format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec == CODEC_TYPE_OPUS)
            .context("the Ogg file does not contain an Opus audio stream")?;
        let track_id = track.id;
        let codec_params = track.codec_params.clone();
        let total_duration = codec_params
            .time_base
            .zip(codec_params.n_frames)
            .map(|(base, frames)| Duration::from(base.calc_time(frames)))
            .filter(|duration| !duration.is_zero());

        let mut codecs = CodecRegistry::new();
        codecs.register_all::<OpusDecoder>();
        let mut decoder = codecs
            .make(&codec_params, &DecoderOptions::default())
            .context("failed to initialize the Opus decoder")?;
        let (buffer, spec) = decode_next_opus_packet(&mut probed.format, &mut decoder, track_id)
            .context("the Opus stream contains no decodable audio packets")?;

        Ok(Self {
            decoder,
            format: probed.format,
            track_id,
            total_duration,
            buffer,
            buffer_offset: 0,
            spec,
        })
    }

    fn refill(&mut self) -> Option<()> {
        let (buffer, spec) =
            decode_next_opus_packet(&mut self.format, &mut self.decoder, self.track_id)?;
        self.spec = spec;
        self.buffer = buffer;
        self.buffer_offset = 0;
        Some(())
    }
}

fn decode_next_opus_packet(
    format: &mut Box<dyn FormatReader>,
    decoder: &mut Box<dyn SymphoniaDecoder>,
    track_id: u32,
) -> Option<(SampleBuffer<f32>, SignalSpec)> {
    loop {
        let packet = format.next_packet().ok()?;
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) if decoded.frames() != 0 => {
                let spec = *decoded.spec();
                return Some((copy_sample_buffer(decoded, &spec), spec));
            }
            Ok(_) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => return None,
        }
    }
}

fn copy_sample_buffer(decoded: AudioBufferRef<'_>, spec: &SignalSpec) -> SampleBuffer<f32> {
    let mut buffer = SampleBuffer::new(units::Duration::from(decoded.capacity() as u64), *spec);
    buffer.copy_interleaved_ref(decoded);
    buffer
}

impl Iterator for OpusSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buffer_offset >= self.buffer.len() {
            self.refill()?;
        }
        let sample = *self.buffer.samples().get(self.buffer_offset)?;
        self.buffer_offset += 1;
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.buffer.len().saturating_sub(self.buffer_offset), None)
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.buffer.len())
    }

    fn channels(&self) -> ChannelCount {
        ChannelCount::new(self.spec.channels.count() as u16)
            .expect("decoded Opus audio always has at least one channel")
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(self.spec.rate).expect("decoded Opus audio has a non-zero sample rate")
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }
}

pub struct FilePlayback {
    // The OS output stream must outlive the player and its source.
    _output: MixerDeviceSink,
    player: Player,
    completed: Arc<AtomicBool>,
}

impl FilePlayback {
    pub fn is_finished(&self) -> bool {
        self.completed.load(Ordering::Acquire) && self.player.empty()
    }

    pub fn stop(&self) {
        self.player.stop();
    }
}

struct LookaheadSource<S> {
    input: S,
    analysis: Option<Sender<Vec<f32>>>,
    analysis_chunk_frames: usize,
    mono_chunk: Vec<f32>,
    frame_sum: f32,
    frame_samples: usize,
    channels: ChannelCount,
    sample_rate: SampleRate,
    delay: Duration,
    delay_samples: usize,
    initial_silence_remaining: usize,
    delayed: VecDeque<f32>,
    input_finished: bool,
    completed: Arc<AtomicBool>,
    #[cfg(test)]
    maximum_buffered: usize,
}

impl<S> LookaheadSource<S>
where
    S: Source<Item = f32>,
{
    fn new(
        input: S,
        delay: Duration,
        analysis: Sender<Vec<f32>>,
        completed: Arc<AtomicBool>,
    ) -> Self {
        Self::with_chunk_frames(input, delay, analysis, completed, ANALYSIS_CHUNK_FRAMES)
    }

    fn with_chunk_frames(
        input: S,
        delay: Duration,
        analysis: Sender<Vec<f32>>,
        completed: Arc<AtomicBool>,
        analysis_chunk_frames: usize,
    ) -> Self {
        let channels = input.channels();
        let sample_rate = input.sample_rate();
        let delay_frames = duration_to_frames(delay, sample_rate.get());
        let delay_samples = delay_frames.saturating_mul(channels.get() as usize);
        Self {
            input,
            analysis: Some(analysis),
            analysis_chunk_frames,
            mono_chunk: Vec::with_capacity(analysis_chunk_frames),
            frame_sum: 0.0,
            frame_samples: 0,
            channels,
            sample_rate,
            delay,
            delay_samples,
            initial_silence_remaining: delay_samples,
            delayed: VecDeque::with_capacity(delay_samples),
            input_finished: false,
            completed,
            #[cfg(test)]
            maximum_buffered: 0,
        }
    }

    fn analyze_sample(&mut self, sample: f32) {
        self.frame_sum += sanitize(sample);
        self.frame_samples += 1;
        if self.frame_samples == self.channels.get() as usize {
            self.mono_chunk
                .push(self.frame_sum / self.frame_samples as f32);
            self.frame_sum = 0.0;
            self.frame_samples = 0;
            if self.mono_chunk.len() >= self.analysis_chunk_frames {
                self.flush_analysis();
            }
        }
    }

    fn finish_analysis(&mut self) {
        if self.frame_samples != 0 {
            self.mono_chunk
                .push(self.frame_sum / self.frame_samples as f32);
            self.frame_sum = 0.0;
            self.frame_samples = 0;
        }
        self.flush_analysis();
        self.analysis.take();
    }

    fn flush_analysis(&mut self) {
        if self.mono_chunk.is_empty() {
            return;
        }
        let chunk = std::mem::replace(
            &mut self.mono_chunk,
            Vec::with_capacity(self.analysis_chunk_frames),
        );
        if self
            .analysis
            .as_ref()
            .is_some_and(|analysis| analysis.send(chunk).is_err())
        {
            self.analysis.take();
        }
    }

    fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
    }
}

impl<S> Iterator for LookaheadSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.input_finished {
            if let Some(sample) = self.input.next() {
                self.analyze_sample(sample);
                if self.delay_samples == 0 {
                    return Some(sample);
                }
                let output = if self.initial_silence_remaining != 0 {
                    self.initial_silence_remaining -= 1;
                    self.delayed.push_back(sample);
                    0.0
                } else {
                    let output = self
                        .delayed
                        .pop_front()
                        .expect("delay buffer was just checked as non-empty");
                    self.delayed.push_back(sample);
                    output
                };
                #[cfg(test)]
                {
                    self.maximum_buffered = self.maximum_buffered.max(self.delayed.len());
                }
                return Some(output);
            }
            self.input_finished = true;
            self.finish_analysis();
        }

        if self.initial_silence_remaining != 0 {
            self.initial_silence_remaining -= 1;
            return Some(0.0);
        }

        let output = self.delayed.pop_front();
        if output.is_none() {
            self.mark_completed();
        }
        output
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.input_finished {
            let remaining = self
                .delayed
                .len()
                .saturating_add(self.initial_silence_remaining);
            return (remaining, Some(remaining));
        }
        let (minimum, maximum) = self.input.size_hint();
        (
            minimum.saturating_add(self.delay_samples),
            maximum.map(|value| value.saturating_add(self.delay_samples)),
        )
    }
}

impl<S> Source for LookaheadSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input
            .total_duration()
            .and_then(|duration| duration.checked_add(self.delay))
    }
}

fn duration_to_frames(duration: Duration, sample_rate: u32) -> usize {
    let frames = duration
        .as_nanos()
        .saturating_mul(u128::from(sample_rate))
        .saturating_add(500_000_000)
        / 1_000_000_000;
    usize::try_from(frames).unwrap_or(usize::MAX)
}

fn validate_source_sample_rate(sample_rate: u32) -> Result<()> {
    if !(8_000..=192_000).contains(&sample_rate) {
        bail!(
            "audio file sample rate {sample_rate} Hz is unsupported; expected 8000 through 192000 Hz"
        );
    }
    Ok(())
}

fn format_name(extension: Option<&str>) -> String {
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("mp3") => "MP3".into(),
        Some("flac") => "FLAC".into(),
        Some("wav") | Some("wave") => "WAV".into(),
        Some("ogg") | Some("oga") => "Ogg Vorbis".into(),
        Some("opus") => "Ogg Opus".into(),
        Some("mp4") | Some("m4a") | Some("aac") => "MP4/AAC".into(),
        Some(extension) => format!("detected audio ({extension})"),
        None => "detected audio".into(),
    }
}

fn sanitize(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, num::NonZero};

    use crossbeam_channel::{bounded, unbounded};

    use super::*;

    struct TestSource {
        samples: std::vec::IntoIter<f32>,
        channels: ChannelCount,
        sample_rate: SampleRate,
        duration: Option<Duration>,
    }

    impl TestSource {
        fn new(samples: Vec<f32>, channels: u16, sample_rate: u32) -> Self {
            let frame_count = samples.len() / channels as usize;
            Self {
                samples: samples.into_iter(),
                channels: NonZero::new(channels).unwrap(),
                sample_rate: NonZero::new(sample_rate).unwrap(),
                duration: Some(Duration::from_secs_f64(
                    frame_count as f64 / sample_rate as f64,
                )),
            }
        }
    }

    impl Iterator for TestSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            self.samples.next()
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            self.samples.size_hint()
        }
    }

    impl Source for TestSource {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.samples.len())
        }

        fn channels(&self) -> ChannelCount {
            self.channels
        }

        fn sample_rate(&self) -> SampleRate {
            self.sample_rate
        }

        fn total_duration(&self) -> Option<Duration> {
            self.duration
        }
    }

    fn source(
        samples: Vec<f32>,
        channels: u16,
        sample_rate: u32,
        delay: Duration,
        chunk_frames: usize,
    ) -> (
        LookaheadSource<TestSource>,
        crossbeam_channel::Receiver<Vec<f32>>,
    ) {
        let (sender, receiver) = unbounded();
        let source = LookaheadSource::with_chunk_frames(
            TestSource::new(samples, channels, sample_rate),
            delay,
            sender,
            Arc::new(AtomicBool::new(false)),
            chunk_frames,
        );
        (source, receiver)
    }

    #[test]
    fn zero_delay_is_a_passthrough() {
        let samples = vec![0.1, -0.2, 0.3];
        let (source, _) = source(samples.clone(), 1, 1_000, Duration::ZERO, 8);
        assert_eq!(source.collect::<Vec<_>>(), samples);
    }

    #[test]
    fn emits_initial_silence_and_exact_sample_delay() {
        let samples = vec![1.0, 2.0, 3.0, 4.0];
        let (source, _) = source(samples, 1, 1_000, Duration::from_millis(3), 8);
        assert_eq!(
            source.collect::<Vec<_>>(),
            vec![0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0]
        );
    }

    #[test]
    fn delay_is_frame_aligned_for_stereo() {
        let samples = vec![0.1, 0.2, 0.3, 0.4];
        let (source, _) = source(samples.clone(), 2, 1_000, Duration::from_millis(2), 8);
        let mut expected = vec![0.0; 4];
        expected.extend(samples);
        assert_eq!(source.collect::<Vec<_>>(), expected);
    }

    #[test]
    fn drains_every_delayed_sample_after_eof() {
        let samples = vec![0.25, 0.5];
        let (source, _) = source(samples, 1, 1_000, Duration::from_millis(5), 8);
        assert_eq!(
            source.collect::<Vec<_>>(),
            vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.25, 0.5]
        );
    }

    #[test]
    fn duration_uses_native_sample_rate_and_includes_delay() {
        let (source, _) = source(
            vec![0.0; 44_100],
            1,
            44_100,
            Duration::from_millis(750),
            512,
        );
        assert_eq!(source.total_duration(), Some(Duration::from_millis(1_750)));
    }

    #[test]
    fn stereo_analysis_is_downmixed_by_frame() {
        let (source, receiver) = source(vec![1.0, -1.0, 0.5, 0.25], 2, 48_000, Duration::ZERO, 8);
        let _ = source.collect::<Vec<_>>();
        assert_eq!(receiver.recv().unwrap(), vec![0.0, 0.375]);
    }

    #[test]
    fn flushes_a_partial_analysis_chunk_at_eof() {
        let (source, receiver) =
            source(vec![0.1, 0.2, 0.3, 0.4, 0.5], 1, 48_000, Duration::ZERO, 4);
        let _ = source.collect::<Vec<_>>();
        assert_eq!(receiver.recv().unwrap(), vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(receiver.recv().unwrap(), vec![0.5]);
    }

    #[test]
    fn delay_buffer_memory_is_bounded_for_long_sources() {
        let delay = Duration::from_millis(100);
        let expected_samples = 4_800 * 2;
        let (mut source, _) = source(vec![0.0; 480_000], 2, 48_000, delay, 512);
        while source.next().is_some() {}
        assert_eq!(source.delay_samples, expected_samples);
        assert!(source.maximum_buffered <= expected_samples);
    }

    #[test]
    fn completion_is_signaled_only_after_delayed_output_is_drained() {
        let completed = Arc::new(AtomicBool::new(false));
        let (sender, _receiver) = bounded(4);
        let mut source = LookaheadSource::with_chunk_frames(
            TestSource::new(vec![1.0], 1, 1_000),
            Duration::from_millis(2),
            sender,
            Arc::clone(&completed),
            8,
        );
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(1.0));
        assert!(!completed.load(Ordering::Acquire));
        assert_eq!(source.next(), None);
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn validates_source_sample_rate_bounds() {
        assert!(validate_source_sample_rate(8_000).is_ok());
        assert!(validate_source_sample_rate(192_000).is_ok());
        assert!(validate_source_sample_rate(7_999).is_err());
        assert!(validate_source_sample_rate(192_001).is_err());
    }

    #[test]
    fn decodes_an_ogg_opus_stream_incrementally() {
        // 100 ms of a mono 440 Hz tone generated with FFmpeg/libopus.
        const OPUS_HEX: &str = concat!(
            "4f67675300020000000000000000e4dd2bae00000000f35b38fa01134f70",
            "7573486561640101380180bb00000000004f676753000000000000000000",
            "00e4dd2bae01000000bc4817f3013e4f707573546167730d0000004c6176",
            "6636322e31322e313032010000001d000000656e636f6465723d4c617663",
            "36322e32382e313032206c69626f7075734f6767530004f8130000000000",
            "00e4dd2bae02000000e49f6ec4065f423b383b2f7881a75d6c9e99ac0000",
            "080ae05ad5119c443115057b3de67e1cb8f90d59595f21d0ba3aa58e7538",
            "3c1a181fb9faf16ad9f5d570914f0b381afe9843b0b543aaf0596be2f435",
            "0e0d0065c40d12e1e1726c418d329138b2c8489480e5a5ea04789f6701e7",
            "fc954d4eaa18a719dfe55c59dffb6ea13365f5a8a5490de45777878f0a42",
            "d0924691fd1fd43b703ff7d597a83856825225700bba963c1c0f4f41f789",
            "bd789ab2df759cfc4b3a457e285a7287c683883b1c472a162818134401ff",
            "ba0f93c2b1e175d8a3fc94d16c4e8171b6f32023a79ff5d6e7f72cdd7c05",
            "789ab2df759cfc45edd7339f1363df1013d90fefc0b7a178bc2768b989bf",
            "68afc88af0d90732a3c8a1e9d51e26c7dfc50ead776f594b134c789ab2df",
            "759cfc4933bf4af7252368a68d200175769bed3be7a525aa12043feeebef",
            "464c96892058587f67f75fbf38defc5697f829a76bf649e5047805a415f0",
            "11e78862b1a6b060cbe0d49a234e82d9771ca4015f3bca47ff16056ccb1f",
            "2626097f99bf61538e260e5c",
        );
        let bytes = OPUS_HEX
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect::<Vec<_>>();
        let path = std::env::temp_dir().join(format!(
            "light-wizard-opus-decoder-{}.ogg",
            std::process::id()
        ));
        fs::write(&path, bytes).unwrap();

        let result = (|| {
            let file = File::open(&path)?;
            let byte_len = file.metadata()?.len();
            let mut source = open_decoder(&path, file, byte_len, Some("ogg"))?;
            assert_eq!(source.format_name(Some("ogg")), "Ogg Opus");
            assert_eq!(source.sample_rate().get(), 48_000);
            assert_eq!(source.channels().get(), 1);
            assert!(source.total_duration().is_some());
            let samples = source.by_ref().collect::<Vec<_>>();
            assert!(
                (5_000..=5_600).contains(&samples.len()),
                "decoded {} samples",
                samples.len()
            );
            assert!(samples.iter().any(|sample| sample.abs() > 0.01));
            Ok::<_, anyhow::Error>(())
        })();
        let _ = fs::remove_file(path);
        result.unwrap();
    }
}
