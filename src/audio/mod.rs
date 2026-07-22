#[cfg(target_os = "macos")]
mod macos;
mod player;

#[cfg(target_os = "macos")]
pub use macos::SystemAudioCapture;
pub use player::{AudioFileMetadata, FilePlayback, PreparedAudioFile};

#[cfg(not(target_os = "macos"))]
pub struct SystemAudioCapture;

#[cfg(not(target_os = "macos"))]
impl SystemAudioCapture {
    pub fn start(
        _sender: crossbeam_channel::Sender<Vec<f32>>,
        _sample_rate: u32,
        _channels: u32,
    ) -> anyhow::Result<Self> {
        anyhow::bail!("system-wide audio capture is currently implemented only for macOS")
    }
}
