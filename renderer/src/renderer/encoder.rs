use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

use super::rasterizer::RgbaFrame;

/// Streams fixed-size RGBA frames to FFmpeg at a constant frame rate.
pub struct VideoEncoder {
    child: Child,
    destination: PathBuf,
    scratch: TempDir,
    width: u32,
    height: u32,
    frame_bytes: usize,
    wrote_frame: bool,
}

impl VideoEncoder {
    /// Create an H.264 MP4 or a lossless FFV1 Matroska file. Existing files are protected.
    pub fn spawn(path: &Path, width: u32, height: u32, fps: u32) -> io::Result<Self> {
        let frame_bytes = usize::try_from(u64::from(width) * u64::from(height))
            .ok()
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&bytes| bytes != 0 && fps != 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid frame size or rate")
            })?;
        let mut ffmpeg = Command::new("ffmpeg");
        ffmpeg.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-n",
            "-xerror",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &format!("{width}x{height}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
        ]);
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("mp4") => {
                let pixel_format = if width.is_multiple_of(2) && height.is_multiple_of(2) {
                    "yuv420p"
                } else {
                    "yuv444p"
                };
                ffmpeg.args([
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    pixel_format,
                    "-crf",
                    "12",
                    "-movflags",
                    "+faststart",
                    "-f",
                    "mp4",
                ]);
            }
            Some(ext) if ext.eq_ignore_ascii_case("mkv") => {
                ffmpeg.args(["-c:v", "ffv1", "-pix_fmt", "bgra", "-f", "matroska"]);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "video output must end in .mp4 or .mkv",
                ));
            }
        }
        let destination = std::path::absolute(path)?;
        if destination.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "video output already exists",
            ));
        }
        let parent = destination.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "video output has no parent directory",
            )
        })?;
        let scratch = tempfile::Builder::new()
            .prefix(".replay-render-")
            .tempdir_in(parent)?;
        let child = ffmpeg
            .arg(scratch.path().join("video"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        Ok(Self {
            child,
            destination,
            scratch,
            width,
            height,
            frame_bytes,
            wrote_frame: false,
        })
    }

    pub fn write_frame(&mut self, frame: RgbaFrame<'_>) -> io::Result<()> {
        if (frame.width, frame.height) != (self.width, self.height)
            || frame.pixels.len() != self.frame_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RGBA frame size mismatch",
            ));
        }
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "FFmpeg stdin closed"))?;
        if let Err(error) = stdin.write_all(frame.pixels) {
            self.child.stdin.take();
            return Err(error);
        }
        self.wrote_frame = true;
        Ok(())
    }

    /// Close the raw video stream and wait for FFmpeg to finalize the container.
    pub fn finish(mut self) -> io::Result<()> {
        let stream_closed_early = self.child.stdin.take().is_none();
        let status = self.child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!("FFmpeg exited with {status}")));
        }
        if stream_closed_early {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "FFmpeg input stream failed",
            ));
        }
        if !self.wrote_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no video frames written",
            ));
        }
        // Publish only a finalized video, without replacing an export from another process.
        fs::hard_link(self.scratch.path().join("video"), &self.destination)
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        super::reap_child(&mut self.child);
    }
}
