use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use memmap2::Mmap;

use crate::protocol::{
    CHUNK_HEADER_SIZE, ChunkHeader, EditorMetadata, HEADER_SIZE, Header, ReplayInfo, SeekEntry,
    SeekResult, TimedEvent, invalid_data, unpack,
};
use crate::recorder::Shadow;

pub(crate) struct Player {
    file: Mmap,
    footer_offset: usize,
    index: Vec<SeekEntry>,
    header: Header,
    metadata: EditorMetadata,
}

impl Player {
    pub fn open(path: &Path) -> io::Result<Self> {
        if !fs::metadata(path)?.is_file() {
            return Err(invalid_data("replay path must be a regular file"));
        }
        let input = File::open(path)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() || metadata.len() < HEADER_SIZE as u64 {
            return Err(invalid_data("invalid replay file"));
        }
        // SAFETY: Only this process owns the anonymous backing file. It is never
        // modified after mapping, even if the original replay is replaced or truncated.
        let mut backing = tempfile::tempfile()?;
        if io::copy(&mut input.take(metadata.len()), &mut backing)? != metadata.len() {
            return Err(invalid_data("replay was truncated while opening"));
        }
        let file = unsafe { Mmap::map(&backing)? };

        let header = Header::decode(&file)?;
        let metadata_offset = file_offset(header.metadata_offset, file.len())?;
        let metadata_len = file_offset(header.metadata_len, file.len())?;
        let footer_offset = file_offset(header.footer_offset, file.len())?;
        let first_chunk = metadata_offset
            .checked_add(metadata_len)
            .filter(|&end| metadata_offset >= HEADER_SIZE && end < footer_offset)
            .ok_or_else(|| invalid_data("invalid replay metadata range"))?;
        let metadata = unpack(&file[metadata_offset..first_chunk])?;
        let index: Vec<SeekEntry> = unpack(&file[footer_offset..])?;
        if index.first() != Some(&SeekEntry(0, first_chunk as u64)) {
            return Err(invalid_data("replay is missing its initial keyframe"));
        }
        if index
            .windows(2)
            .any(|pair| pair[0].0 > pair[1].0 || pair[0].1 >= pair[1].1)
        {
            return Err(invalid_data("unordered replay keyframe index"));
        }
        for (position, &SeekEntry(timestamp_ns, offset)) in index.iter().enumerate() {
            let start = file_offset(offset, footer_offset)?;
            let end = index
                .get(position + 1)
                .map_or(Ok(footer_offset), |next| file_offset(next.1, footer_offset))?;
            let mut chunks = &file[start..end];
            let (chunk, _) = take_chunk(&mut chunks)?;
            if !chunk.is_keyframe
                || chunk.timestamp_ns != timestamp_ns
                || timestamp_ns > header.duration_ns
            {
                return Err(invalid_data("invalid replay keyframe entry"));
            }
        }
        Ok(Self {
            file,
            footer_offset,
            index,
            header,
            metadata,
        })
    }

    pub fn info(&self) -> ReplayInfo {
        ReplayInfo {
            started_at_unix_ns: self.header.started_at_unix_ns,
            duration_ns: self.header.duration_ns,
            metadata: self.metadata.clone(),
            keyframes: self.index.iter().map(|entry| entry.0).collect(),
        }
    }

    pub fn seek(&self, timestamp_ns: u64) -> io::Result<SeekResult> {
        let timestamp_ns = timestamp_ns.min(self.header.duration_ns);
        let next = self.index.partition_point(|entry| entry.0 <= timestamp_ns);
        let keyframe = self.index[next - 1];
        let end = self
            .index
            .get(next)
            .map_or(self.footer_offset, |entry| entry.1 as usize);
        let mut chunks = &self.file[keyframe.1 as usize..end];
        let (header, frame) = take_chunk(&mut chunks)?;
        let mut shadow = Shadow::from_snapshot(unpack(&decompress(&header, frame)?)?)?;
        let mut previous_ns = keyframe.0;
        'chunks: while !chunks.is_empty() {
            let (header, frame) = take_chunk(&mut chunks)?;
            if header.is_keyframe
                || header.timestamp_ns < previous_ns
                || header.timestamp_ns > self.header.duration_ns
            {
                return Err(invalid_data("unexpected replay delta chunk"));
            }
            if header.timestamp_ns > timestamp_ns {
                break;
            }
            let events: Vec<TimedEvent> = unpack(&decompress(&header, frame)?)?;
            if events.first().map(|event| event.timestamp_ns) != Some(header.timestamp_ns) {
                return Err(invalid_data(
                    "replay delta start does not match chunk timestamp",
                ));
            }
            for event in events {
                if event.timestamp_ns < previous_ns || event.timestamp_ns > self.header.duration_ns
                {
                    return Err(invalid_data("unordered replay events"));
                }
                if event.timestamp_ns > timestamp_ns {
                    break 'chunks;
                }
                previous_ns = event.timestamp_ns;
                shadow.apply(event.event)?;
            }
        }
        Ok(SeekResult {
            timestamp_ns,
            snapshot: shadow.snapshot(),
        })
    }
}

fn file_offset(offset: u64, limit: usize) -> io::Result<usize> {
    usize::try_from(offset)
        .ok()
        .filter(|&offset| offset < limit)
        .ok_or_else(|| invalid_data("replay file offset outside its section"))
}

fn take_chunk<'a>(chunks: &mut &'a [u8]) -> io::Result<(ChunkHeader, &'a [u8])> {
    let (header, rest) = chunks
        .split_at_checked(CHUNK_HEADER_SIZE)
        .ok_or_else(|| invalid_data("truncated replay chunk header"))?;
    let header = ChunkHeader::decode(header)?;
    let (frame, rest) = rest
        .split_at_checked(header.compressed_size as usize)
        .ok_or_else(|| invalid_data("truncated compressed replay chunk"))?;
    *chunks = rest;
    Ok((header, frame))
}

fn decompress(header: &ChunkHeader, frame: &[u8]) -> io::Result<Vec<u8>> {
    let expected = header.uncompressed_size as usize;
    let bytes = zstd::bulk::decompress(frame, expected)?;
    if bytes.len() != expected {
        return Err(invalid_data(
            "decoded replay chunk size does not match header",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::protocol::{Buffer, CursorPosition, Event, PushEvent, Snapshot};
    use crate::recorder::Recording;

    #[tokio::test]
    async fn seeks_forward_and_backward_across_keyframes_and_equal_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.nvimreplay");
        let metadata = EditorMetadata {
            colorscheme: Some("habamax".into()),
            ..Default::default()
        };
        let snapshot = Snapshot {
            buffers: BTreeMap::from([(
                7,
                Buffer {
                    path: Some("edit.rs".into()),
                    filetype: Some("rust".into()),
                    lines: vec!["é\r".into(), "".into()],
                },
            )]),
            active_buffer: Some(7),
            active_window: Some(9),
            ..Default::default()
        };
        let recording = Recording::start(path.clone(), metadata.clone(), snapshot)
            .await
            .unwrap();
        let edit = |firstline, lastline, line: &str| Event::BufLines {
            buffer_id: 7,
            firstline,
            lastline,
            new_lines: vec![line.into()],
        };
        let keyframe = 10_000_000_000;
        for (timestamp_ns, event) in [
            (1, edit(1, 2, "β\0")),
            (keyframe, edit(0, 1, "new")),
            (keyframe, edit(2, 2, "tail")),
            (
                keyframe + 5,
                Event::CursorMoved {
                    window_id: 9,
                    buffer_id: Some(7),
                    row: 3,
                    col: 2,
                },
            ),
        ] {
            recording
                .push(
                    PushEvent {
                        timestamp_ns: Some(timestamp_ns),
                        event,
                    },
                    false,
                )
                .await
                .unwrap();
        }
        let info = recording.stop().await.unwrap();
        assert_eq!(info.keyframes, [0, keyframe]);
        assert_eq!(info.metadata, metadata);

        let player = Player::open(&path).unwrap();
        assert_eq!(player.info(), info);
        assert_eq!(
            player.seek(keyframe - 1).unwrap().snapshot.buffers[&7].lines,
            ["é\r", "β\0"]
        );
        assert_eq!(
            player.seek(keyframe).unwrap().snapshot.buffers[&7].lines,
            ["new", "β\0", "tail"]
        );
        assert_eq!(
            player.seek(0).unwrap().snapshot.buffers[&7].lines,
            ["é\r", ""]
        );
        let end = player.seek(u64::MAX).unwrap();
        assert_eq!(end.timestamp_ns, keyframe + 5);
        assert_eq!(end.snapshot.cursors[&9], CursorPosition { row: 3, col: 2 });

        // Changing the source cannot invalidate a player that already opened it.
        let length = fs::metadata(&path).unwrap().len();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(length - 1)
            .unwrap();
        assert!(Player::open(&path).is_err());
        assert_eq!(player.seek(u64::MAX).unwrap(), end);
    }

    #[tokio::test]
    async fn count_based_chunks_index_keyframes_even_at_one_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.nvimreplay");
        let recording =
            Recording::start(path.clone(), EditorMetadata::default(), Snapshot::default())
                .await
                .unwrap();
        for i in 0..(5 * 512 + 1) {
            recording
                .push(
                    PushEvent {
                        timestamp_ns: Some(1),
                        event: Event::CursorMoved {
                            window_id: 1,
                            buffer_id: None,
                            row: i + 1,
                            col: 0,
                        },
                    },
                    false,
                )
                .await
                .unwrap();
        }
        assert_eq!(recording.stop().await.unwrap().keyframes, [0, 1]);
        let player = Player::open(&path).unwrap();
        assert!(player.seek(0).unwrap().snapshot.cursors.is_empty());
        assert_eq!(
            player.seek(1).unwrap().snapshot.cursors[&1].row,
            5 * 512 + 1
        );
    }
}
