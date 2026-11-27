use std::collections::BTreeMap;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rmpv::Value;
use ropey::Rope;
use tempfile::NamedTempFile;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{self, JoinHandle};

use crate::protocol::{
    Buffer, ChunkHeader, CursorPosition, EditorMetadata, Event, HEADER_SIZE, Header,
    MAX_CHUNK_BYTES, PushEvent, ReplayInfo, SeekEntry, Snapshot, TimedEvent, invalid_data, pack,
};

const QUEUE_CAPACITY: usize = 64;
const CHUNK_EVENTS: u32 = 512;
const CHUNK_NS: u64 = 2_000_000_000;
const KEYFRAME_CHUNKS: usize = 5;
const KEYFRAME_NS: u64 = CHUNK_NS * KEYFRAME_CHUNKS as u64;

pub(crate) struct Recording {
    events: mpsc::Sender<QueuedEvent>,
    writer: JoinHandle<io::Result<ReplayInfo>>,
    started: Instant,
}

struct QueuedEvent {
    timestamp_ns: u64,
    timestamp_explicit: bool,
    event: Event,
    reply: Option<oneshot::Sender<io::Result<()>>>,
}

impl Recording {
    pub async fn start(
        path: PathBuf,
        metadata: EditorMetadata,
        snapshot: Snapshot,
    ) -> io::Result<Self> {
        let (events, mut input) = mpsc::channel::<QueuedEvent>(QUEUE_CAPACITY);
        let (ready, initialized) = oneshot::channel();
        let started = Instant::now();
        let writer = task::spawn_blocking(move || {
            let (file, mut writer) = match create_writer(&path, metadata, snapshot) {
                Ok(opened) => opened,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return Err(io::Error::other("could not start recording"));
                }
            };
            if ready.send(Ok(())).is_err() {
                return Err(io::Error::other(
                    "client disconnected before recording started",
                ));
            }
            while let Some(queued) = input.blocking_recv() {
                match writer.prepare(queued.timestamp_ns, queued.timestamp_explicit, queued.event) {
                    Ok((timestamp_ns, bytes)) => {
                        writer.append(timestamp_ns, &bytes)?;
                        if let Some(reply) = queued.reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    Err(error) => match queued.reply {
                        Some(reply) => {
                            let _ = reply.send(Err(error));
                        }
                        None => eprintln!("push_event: {error}"),
                    },
                }
            }
            let info = writer.finish()?;
            file.as_file().sync_all()?;
            if let Err(error) = file.persist_noclobber(path) {
                let cause = error.error;
                let location = match error.file.keep() {
                    Ok((_, path)) => format!("Recording saved at {}", path.display()),
                    Err(error) => format!(
                        "Could not keep the completed temporary recording ({})",
                        error.error
                    ),
                };
                return Err(io::Error::new(cause.kind(), format!("{cause}. {location}")));
            }
            Ok(info)
        });
        initialized.await.map_err(io::Error::other)??;
        Ok(Self {
            events,
            writer,
            started,
        })
    }

    pub async fn push(&self, event: PushEvent, request: bool) -> io::Result<()> {
        let (reply, ack) = if request {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let timestamp_ns = event
            .timestamp_ns
            .unwrap_or_else(|| self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        self.events
            .send(QueuedEvent {
                timestamp_ns,
                timestamp_explicit: event.timestamp_ns.is_some(),
                event: event.event,
                reply,
            })
            .await
            .map_err(|_| io::Error::other("recording writer stopped"))?;
        if let Some(ack) = ack {
            return ack
                .await
                .map_err(|_| io::Error::other("recording writer stopped"))?;
        }
        Ok(())
    }

    pub async fn stop(self) -> io::Result<ReplayInfo> {
        drop(self.events);
        self.writer.await.map_err(io::Error::other)?
    }
}

fn create_writer(
    path: &Path,
    metadata: EditorMetadata,
    snapshot: Snapshot,
) -> io::Result<(NamedTempFile, Writer<BufWriter<std::fs::File>>)> {
    if path.file_name().is_none() {
        return Err(invalid_data("recording path must include a file name"));
    }
    match path.symlink_metadata() {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recording path already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file = NamedTempFile::new_in(parent)?;
    let writer = Writer::new(BufWriter::new(file.reopen()?), metadata, snapshot)?;
    Ok((file, writer))
}

struct Writer<W> {
    output: W,
    offset: u64,
    header: Header,
    metadata: EditorMetadata,
    shadow: Shadow,
    index: Vec<SeekEntry>,
    deltas_since_keyframe: usize,
    first_event_ns: u64,
    event_count: u32,
    events: Vec<u8>,
}

impl<W: Write + Seek> Writer<W> {
    fn new(mut output: W, metadata: EditorMetadata, snapshot: Snapshot) -> io::Result<Self> {
        let shadow = Shadow::from_snapshot(snapshot)?;
        let metadata_bytes = pack(&metadata)?;
        let started_at_unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos()
            .try_into()
            .map_err(io::Error::other)?;
        let header = Header {
            started_at_unix_ns,
            metadata_offset: HEADER_SIZE as u64,
            metadata_len: metadata_bytes.len() as u64,
            footer_offset: 0,
            duration_ns: 0,
        };
        output.write_all(&header.encode())?;
        output.write_all(&metadata_bytes)?;
        let mut writer = Self {
            output,
            offset: header.metadata_offset + header.metadata_len,
            header,
            metadata,
            shadow,
            index: Vec::new(),
            deltas_since_keyframe: 0,
            first_event_ns: 0,
            event_count: 0,
            events: vec![0; 5],
        };
        writer.keyframe()?;
        Ok(writer)
    }

    fn prepare(
        &mut self,
        timestamp_ns: u64,
        timestamp_explicit: bool,
        event: Event,
    ) -> io::Result<(u64, Vec<u8>)> {
        let previous_ns = self.header.duration_ns;
        if timestamp_explicit && timestamp_ns < previous_ns {
            return Err(invalid_data("event timestamps must be nondecreasing"));
        }
        let timestamp_ns = timestamp_ns.max(previous_ns);
        let timed = TimedEvent {
            timestamp_ns,
            event,
        };
        let bytes = pack(&timed)?;
        if bytes.len() > MAX_CHUNK_BYTES - 5 {
            return Err(invalid_data("event exceeds replay chunk size limit"));
        }
        self.shadow.apply(timed.event)?;
        Ok((timestamp_ns, bytes))
    }

    fn append(&mut self, timestamp_ns: u64, bytes: &[u8]) -> io::Result<()> {
        if self.event_count != 0
            && (timestamp_ns - self.first_event_ns >= CHUNK_NS
                || self.events.len() + bytes.len() > MAX_CHUNK_BYTES)
        {
            self.flush_deltas()?;
        }
        if self.event_count == 0 {
            self.first_event_ns = timestamp_ns;
        }
        self.events.extend_from_slice(bytes);
        self.event_count += 1;
        self.header.duration_ns = timestamp_ns;
        if self.event_count == CHUNK_EVENTS {
            self.flush_deltas()?;
        }
        if self.deltas_since_keyframe >= KEYFRAME_CHUNKS
            || timestamp_ns - self.index.last().map_or(0, |entry| entry.0) >= KEYFRAME_NS
        {
            self.flush_deltas()?;
            self.keyframe()?;
        }
        Ok(())
    }

    fn flush_deltas(&mut self) -> io::Result<()> {
        if self.event_count == 0 {
            return Ok(());
        }
        // A fixed array32 prefix lets the writer pack each event only once.
        self.events[0] = 0xdd;
        self.events[1..5].copy_from_slice(&self.event_count.to_be_bytes());
        self.offset += write_chunk(&mut self.output, self.first_event_ns, false, &self.events)?;
        self.events.truncate(5);
        self.event_count = 0;
        self.deltas_since_keyframe += 1;
        Ok(())
    }

    fn keyframe(&mut self) -> io::Result<()> {
        let timestamp_ns = self.header.duration_ns;
        let bytes = pack(&self.shadow.snapshot())?;
        self.index.push(SeekEntry(timestamp_ns, self.offset));
        self.offset += write_chunk(&mut self.output, timestamp_ns, true, &bytes)?;
        self.deltas_since_keyframe = 0;
        Ok(())
    }

    fn finish(mut self) -> io::Result<ReplayInfo> {
        self.flush_deltas()?;
        self.header.footer_offset = self.offset;
        self.output.write_all(&pack(&self.index)?)?;
        self.output.seek(SeekFrom::Start(0))?;
        self.output.write_all(&self.header.encode())?;
        self.output.flush()?;
        Ok(ReplayInfo {
            started_at_unix_ns: self.header.started_at_unix_ns,
            duration_ns: self.header.duration_ns,
            metadata: self.metadata,
            keyframes: self.index.into_iter().map(|entry| entry.0).collect(),
        })
    }
}

fn write_chunk(
    output: &mut impl Write,
    timestamp_ns: u64,
    is_keyframe: bool,
    bytes: &[u8],
) -> io::Result<u64> {
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(invalid_data("snapshot exceeds replay chunk size limit"));
    }
    let compressed = zstd::bulk::compress(bytes, 3)?;
    let header = ChunkHeader {
        timestamp_ns,
        compressed_size: compressed.len() as u32,
        uncompressed_size: bytes.len() as u32,
        is_keyframe,
    };
    let encoded = header.encode();
    output.write_all(&encoded)?;
    output.write_all(&compressed)?;
    Ok((encoded.len() + compressed.len()) as u64)
}

pub(crate) struct Shadow {
    buffers: BTreeMap<u64, ShadowBuffer>,
    cursors: BTreeMap<u64, CursorPosition>,
    active_buffer: Option<u64>,
    active_window: Option<u64>,
    mode: Option<String>,
    layout: Option<Value>,
}

#[derive(Default)]
struct ShadowBuffer {
    path: Option<String>,
    filetype: Option<String>,
    text: Rope,
}

impl Shadow {
    pub fn from_snapshot(snapshot: Snapshot) -> io::Result<Self> {
        let mut buffers = BTreeMap::new();
        for (id, buffer) in snapshot.buffers {
            buffers.insert(
                id,
                ShadowBuffer {
                    path: buffer.path,
                    filetype: buffer.filetype,
                    text: Rope::from_str(&line_text(&buffer.lines)?),
                },
            );
        }
        if snapshot
            .active_buffer
            .is_some_and(|id| !buffers.contains_key(&id))
        {
            return Err(invalid_data("snapshot active buffer is missing"));
        }
        Ok(Self {
            buffers,
            cursors: snapshot.cursors,
            active_buffer: snapshot.active_buffer,
            active_window: snapshot.active_window,
            mode: snapshot.mode,
            layout: snapshot.layout,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            buffers: self
                .buffers
                .iter()
                .map(|(&id, buffer)| {
                    (
                        id,
                        Buffer {
                            path: buffer.path.clone(),
                            filetype: buffer.filetype.clone(),
                            lines: rope_lines(&buffer.text),
                        },
                    )
                })
                .collect(),
            cursors: self.cursors.clone(),
            active_buffer: self.active_buffer,
            active_window: self.active_window,
            mode: self.mode.clone(),
            layout: self.layout.clone(),
        }
    }

    pub fn apply(&mut self, event: Event) -> io::Result<()> {
        match event {
            Event::BufSnapshot {
                buffer_id,
                path,
                filetype,
                lines,
            } => {
                self.buffers.insert(
                    buffer_id,
                    ShadowBuffer {
                        path,
                        filetype,
                        text: Rope::from_str(&line_text(&lines)?),
                    },
                );
            }
            Event::BufEnter {
                buffer_id,
                window_id,
                path,
                filetype,
                lines,
            } => {
                let text = lines.as_deref().map(line_text).transpose()?;
                let buffer = self
                    .buffers
                    .entry(buffer_id)
                    .or_insert_with(|| ShadowBuffer {
                        text: Rope::from_str("\n"),
                        ..ShadowBuffer::default()
                    });
                if let Some(text) = text {
                    buffer.text = Rope::from_str(&text);
                }
                if path.is_some() {
                    buffer.path = path;
                }
                if filetype.is_some() {
                    buffer.filetype = filetype;
                }
                self.active_buffer = Some(buffer_id);
                self.active_window = window_id;
            }
            Event::BufLeave { buffer_id } => {
                if self.active_buffer == Some(buffer_id) {
                    self.active_buffer = None;
                    self.active_window = None;
                }
            }
            Event::BufClose { buffer_id } => {
                self.buffers.remove(&buffer_id);
                if self.active_buffer == Some(buffer_id) {
                    self.active_buffer = None;
                    self.active_window = None;
                }
            }
            Event::BufLines {
                buffer_id,
                firstline,
                lastline,
                new_lines,
            } => {
                let buffer = self
                    .buffers
                    .get_mut(&buffer_id)
                    .ok_or_else(|| invalid_data("buffer is missing from replay state"))?;
                if firstline > lastline || lastline >= buffer.text.len_lines() {
                    return Err(invalid_data("buffer line range is out of bounds"));
                }
                let replacement = line_text(&new_lines)?;
                let start = buffer.text.line_to_char(firstline);
                let end = buffer.text.line_to_char(lastline);
                buffer.text.remove(start..end);
                buffer.text.insert(start, &replacement);
            }
            Event::CursorMoved {
                window_id,
                buffer_id,
                row,
                col,
            } => {
                if buffer_id.is_some() && buffer_id == self.active_buffer {
                    self.active_window = Some(window_id);
                }
                self.cursors.insert(window_id, CursorPosition { row, col });
            }
            Event::ModeChanged { new_mode, .. } => self.mode = Some(new_mode),
            Event::WinLayout { layout } => self.layout = layout,
            Event::KeyPress { .. } | Event::UiRedraw { .. } | Event::RecordingEnd => {}
        }
        Ok(())
    }
}

fn line_text(lines: &[String]) -> io::Result<String> {
    let mut text = String::new();
    for line in lines {
        if line.contains('\n') {
            return Err(invalid_data("buffer lines must not contain line feeds"));
        }
        text.push_str(line);
        text.push('\n');
    }
    Ok(text)
}

fn rope_lines(text: &Rope) -> Vec<String> {
    // Each editor line has a trailing LF, so Ropey's final empty line is a sentinel.
    text.lines()
        .take(text.len_lines() - 1)
        .map(|line| {
            let mut line = line.to_string();
            line.pop();
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writer_reports_invalid_edits_without_mutating_recording() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.nvimreplay");
        let recording =
            Recording::start(path.clone(), EditorMetadata::default(), Snapshot::default())
                .await
                .unwrap();
        let event = |timestamp_ns, event| PushEvent {
            timestamp_ns: Some(timestamp_ns),
            event,
        };
        recording
            .push(
                event(
                    5,
                    Event::BufEnter {
                        buffer_id: 7,
                        window_id: None,
                        path: None,
                        filetype: None,
                        lines: Some(vec!["original".into()]),
                    },
                ),
                true,
            )
            .await
            .unwrap();
        let stale = recording
            .push(event(4, Event::BufClose { buffer_id: 7 }), true)
            .await;
        assert_eq!(stale.unwrap_err().kind(), io::ErrorKind::InvalidData);
        let invalid_edit = recording
            .push(
                event(
                    6,
                    Event::BufLines {
                        buffer_id: 7,
                        firstline: 0,
                        lastline: 2,
                        new_lines: vec![],
                    },
                ),
                true,
            )
            .await;
        assert_eq!(invalid_edit.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(recording.stop().await.unwrap().duration_ns, 5);
        let player = crate::player::Player::open(&path).unwrap();
        assert_eq!(
            player.seek(5).unwrap().snapshot.buffers[&7].lines,
            ["original"]
        );
    }

    #[tokio::test]
    async fn publishing_refuses_to_overwrite_a_file_created_during_recording() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.nvimreplay");
        let recording =
            Recording::start(path.clone(), EditorMetadata::default(), Snapshot::default())
                .await
                .unwrap();
        std::fs::write(&path, b"keep this").unwrap();
        assert_eq!(
            recording.stop().await.unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"keep this");
        let saved = temp
            .path()
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| *entry != path)
            .unwrap();
        assert_eq!(
            crate::player::Player::open(&saved)
                .unwrap()
                .info()
                .duration_ns,
            0
        );
    }
}
