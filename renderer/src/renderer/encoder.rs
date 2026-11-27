use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

use super::rasterizer::RgbaFrame;
use super::rgba_pipe;

/// Streams timestamped RGBA frames to FFmpeg for constant-rate output.
pub struct VideoEncoder {
    child: Child,
    destination: PathBuf,
    scratch: TempDir,
    width: u32,
    height: u32,
    frame_bytes: usize,
    fps: u32,
    next_frame: u64,
}

impl VideoEncoder {
    /// Create an H.264 MP4 or a lossless FFV1 Matroska file. Existing files are protected.
    pub fn spawn(path: &Path, width: u32, height: u32, fps: u32) -> io::Result<Self> {
        let frame_bytes = usize::try_from(u64::from(width) * u64::from(height))
            .ok()
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&bytes| (1..=i32::MAX as usize - 4).contains(&bytes) && fps != 0)
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
            "matroska",
            "-i",
            "pipe:0",
        ]);
        let pixel_format = match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("mp4") => {
                let pixel_format = if width.is_multiple_of(2) && height.is_multiple_of(2) {
                    "yuv420p"
                } else {
                    "yuv444p"
                };
                ffmpeg.args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-pix_fmt",
                    pixel_format,
                    "-crf",
                    "20",
                    "-movflags",
                    "+faststart",
                    "-f",
                    "mp4",
                ]);
                pixel_format
            }
            Some(ext) if ext.eq_ignore_ascii_case("mkv") => {
                ffmpeg.args(["-c:v", "ffv1", "-pix_fmt", "bgra", "-f", "matroska"]);
                "bgra"
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "video output must end in .mp4 or .mkv",
                ));
            }
        };
        ffmpeg.args(["-vf", &format!("format={pixel_format},fps={fps}")]);
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
        let mut encoder = Self {
            child,
            destination,
            scratch,
            width,
            height,
            frame_bytes,
            fps,
            next_frame: 0,
        };
        let stdin =
            encoder.child.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "FFmpeg stdin unavailable")
            })?;
        rgba_pipe::write_header(stdin, width, height, fps)?;
        Ok(encoder)
    }

    pub fn write_frame(&mut self, frame: RgbaFrame<'_>, last: bool) -> io::Result<()> {
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
        // The last packet tells FFmpeg how long to hold a still frame at the end.
        if (!frame.reused || self.next_frame == 0 || last)
            && let Err(error) =
                rgba_pipe::write_frame(stdin, frame.pixels, self.next_frame, self.fps)
        {
            self.child.stdin.take();
            return Err(error);
        }
        self.next_frame += 1;
        Ok(())
    }

    /// Close the input stream and wait for FFmpeg to finalize the container.
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
        if self.next_frame == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires FFmpeg on PATH"]
    fn sparse_frames_preserve_every_output_frame() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("replay.mkv");
        let first = [0x12, 0x34, 0xff, 0xe0].repeat(4);
        let second = [0x80, 0xff, 0x34, 0xff].repeat(4);
        let mut encoder = VideoEncoder::spawn(&path, 2, 2, 60)?;
        for index in 0..93 {
            encoder.write_frame(
                RgbaFrame {
                    width: 2,
                    height: 2,
                    pixels: if index < 61 { &first } else { &second },
                    reused: index != 0 && index != 61,
                },
                index == 92,
            )?;
        }
        encoder.finish()?;

        let decoded = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args([
                "-pix_fmt",
                "rgba",
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()?;
        assert!(decoded.status.success());
        let mut expected = first.repeat(61);
        expected.extend(second.repeat(32));
        assert_eq!(decoded.stdout, expected);
        Ok(())
    }
}
