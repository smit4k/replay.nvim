//! MessagePack-RPC requests use [0, id, method, params], responses use
//! [1, id, error, result], and notifications use [2, method, params].
//!
//! Methods and positional parameters:
//! - start_recording(path, metadata?, snapshot?) -> nil
//! - stop_recording() -> ReplayInfo
//! - push_event(event) -> nil; also accepted as a notification
//! - open_replay(path) -> ReplayInfo
//! - seek_to(timestamp_ns) -> SeekResult
//! - replay_render(input_path, output_path, render_options?) -> nil
//!   With `progress = true` in render_options, the server sends
//!   `[2, "replay_render_progress", [request_id, percent]]` before the reply.
//!   100 means all frames were sent to the encoder. The reply confirms the save.
//!
//! The Lua client also uses record_start, record_stop, and record_event.
//! Its events use "kind", "buf", and "win" as aliases for "type", "buffer_id",
//! and "window_id". The replay_open and replay_seek aliases are also available.
//! replay_seek returns the active buffer's lines, cursor position, and topline.
//!
//! Structs are string-keyed maps, including decimal buffer and window IDs
//! in snapshots. Event maps use the "type" field with the
//! snake_case variant name, plus the variant's fields and an optional
//! timestamp_ns. Relative timestamps are nanoseconds from start_recording.
//! Omitted event timestamps use the daemon's monotonic clock. Buffer line
//! ranges are zero-based and half-open. Cursor rows are one-based and columns
//! are zero-based byte offsets. WinLayout can carry the editor's split tree
//! and per-window dimensions and viewport data as a MessagePack value.
//!
//! Format v1 uses little-endian integers in the fixed headers. The 64-byte
//! file header stores magic, u16 version, u64 Unix time (ns), u64 metadata
//! offset, u64 metadata length, u64 footer offset, u64 duration (ns), and
//! 14 reserved zero bytes. Metadata is a MessagePack map. Each zstd frame
//! follows a 17-byte chunk header: u64 relative time, u32 compressed size,
//! u32 decoded size, u8 snapshot flag. Frames contain either a MessagePack
//! snapshot or an array of TimedEvents. The footer is a MessagePack array
//! of [relative timestamp_ns, file offset] pairs in file order.

use std::collections::BTreeMap;
use std::io::{self, Cursor, Read};

use rmpv::Value;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub(crate) const HEADER_SIZE: usize = 64;
pub(crate) const CHUNK_HEADER_SIZE: usize = 17;
pub(crate) const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
const MAGIC: [u8; 8] = *b"NVREP\0\0\x01";
const VERSION: u16 = 1;

pub(crate) fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find_map(|(key, value)| (key.as_str() == Some(name)).then_some(value))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct EditorMetadata {
    pub font: Option<String>,
    pub colorscheme: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct Snapshot {
    #[serde(default, with = "editor_ids")]
    pub buffers: BTreeMap<u64, Buffer>,
    #[serde(default, with = "editor_ids")]
    pub cursors: BTreeMap<u64, CursorPosition>,
    pub active_buffer: Option<u64>,
    pub active_window: Option<u64>,
    pub mode: Option<String>,
    pub layout: Option<Value>,
}

mod editor_ids {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error, ser::SerializeMap};

    pub(super) fn serialize<S: Serializer, T: Serialize>(
        values: &BTreeMap<u64, T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(values.len()))?;
        for (id, value) in values {
            map.serialize_entry(&id.to_string(), value)?;
        }
        map.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<u64, T>, D::Error> {
        BTreeMap::<String, T>::deserialize(deserializer)?
            .into_iter()
            .map(|(key, value)| {
                let id = key.parse::<u64>().map_err(D::Error::custom)?;
                if key != id.to_string() {
                    return Err(D::Error::custom("expected a decimal editor ID"));
                }
                Ok((id, value))
            })
            .collect()
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Buffer {
    pub path: Option<String>,
    pub filetype: Option<String>,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct CursorPosition {
    pub row: u64,
    pub col: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushEvent {
    pub timestamp_ns: Option<u64>,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TimedEvent {
    pub timestamp_ns: u64,
    pub event: Event,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Event {
    BufSnapshot {
        #[serde(alias = "buf")]
        buffer_id: u64,
        path: Option<String>,
        filetype: Option<String>,
        lines: Vec<String>,
    },
    BufEnter {
        #[serde(alias = "buf")]
        buffer_id: u64,
        #[serde(alias = "win")]
        window_id: Option<u64>,
        path: Option<String>,
        filetype: Option<String>,
        lines: Option<Vec<String>>,
    },
    BufLeave {
        #[serde(alias = "buf")]
        buffer_id: u64,
    },
    BufClose {
        #[serde(alias = "buf")]
        buffer_id: u64,
    },
    BufLines {
        #[serde(alias = "buf")]
        buffer_id: u64,
        firstline: usize,
        lastline: usize,
        new_lines: Vec<String>,
    },
    CursorMoved {
        #[serde(alias = "win")]
        window_id: u64,
        #[serde(alias = "buf")]
        buffer_id: Option<u64>,
        row: u64,
        col: u64,
    },
    ModeChanged {
        old_mode: String,
        new_mode: String,
    },
    WinLayout {
        layout: Option<Value>,
    },
    KeyPress {
        key: String,
    },
    UiRedraw {
        events: Value,
    },
    RecordingEnd,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ReplayInfo {
    pub started_at_unix_ns: u64,
    pub duration_ns: u64,
    pub metadata: EditorMetadata,
    pub keyframes: Vec<u64>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct SeekResult {
    pub timestamp_ns: u64,
    pub snapshot: Snapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct SeekEntry(pub u64, pub u64);

#[derive(Debug, PartialEq)]
pub(crate) struct Header {
    pub started_at_unix_ns: u64,
    pub metadata_offset: u64,
    pub metadata_len: u64,
    pub footer_offset: u64,
    pub duration_ns: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut bytes = [0; HEADER_SIZE];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&VERSION.to_le_bytes());
        bytes[10..18].copy_from_slice(&self.started_at_unix_ns.to_le_bytes());
        bytes[18..26].copy_from_slice(&self.metadata_offset.to_le_bytes());
        bytes[26..34].copy_from_slice(&self.metadata_len.to_le_bytes());
        bytes[34..42].copy_from_slice(&self.footer_offset.to_le_bytes());
        bytes[42..50].copy_from_slice(&self.duration_ns.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut bytes = bytes
            .get(..HEADER_SIZE)
            .ok_or_else(|| invalid_data("truncated replay header"))?;
        if read_array::<8>(&mut bytes)? != MAGIC {
            return Err(invalid_data("invalid replay magic"));
        }
        if u16::from_le_bytes(read_array(&mut bytes)?) != VERSION {
            return Err(invalid_data("unsupported replay version"));
        }
        Ok(Self {
            started_at_unix_ns: u64::from_le_bytes(read_array(&mut bytes)?),
            metadata_offset: u64::from_le_bytes(read_array(&mut bytes)?),
            metadata_len: u64::from_le_bytes(read_array(&mut bytes)?),
            footer_offset: u64::from_le_bytes(read_array(&mut bytes)?),
            duration_ns: u64::from_le_bytes(read_array(&mut bytes)?),
        })
    }
}

pub(crate) struct ChunkHeader {
    pub timestamp_ns: u64,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub is_keyframe: bool,
}

impl ChunkHeader {
    pub fn encode(&self) -> [u8; CHUNK_HEADER_SIZE] {
        let mut bytes = [0; CHUNK_HEADER_SIZE];
        bytes[..8].copy_from_slice(&self.timestamp_ns.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.compressed_size.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.uncompressed_size.to_le_bytes());
        bytes[16] = u8::from(self.is_keyframe);
        bytes
    }

    pub fn decode(mut bytes: &[u8]) -> io::Result<Self> {
        let timestamp_ns = u64::from_le_bytes(read_array(&mut bytes)?);
        let compressed_size = u32::from_le_bytes(read_array(&mut bytes)?);
        let uncompressed_size = u32::from_le_bytes(read_array(&mut bytes)?);
        let [kind] = read_array(&mut bytes)?;
        if kind > 1
            || compressed_size == 0
            || uncompressed_size == 0
            || uncompressed_size as usize > MAX_CHUNK_BYTES
        {
            return Err(invalid_data("invalid replay chunk header"));
        }
        Ok(Self {
            timestamp_ns,
            compressed_size,
            uncompressed_size,
            is_keyframe: kind == 1,
        })
    }
}

fn read_array<const N: usize>(reader: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

pub(crate) fn pack(value: &impl Serialize) -> io::Result<Vec<u8>> {
    rmp_serde::to_vec_named(value).map_err(invalid_data)
}

pub(crate) fn unpack<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let mut input = Cursor::new(bytes);
    let mut decoder = rmp_serde::Deserializer::new(&mut input);
    decoder.set_max_depth(64);
    let value = T::deserialize(&mut decoder).map_err(invalid_data)?;
    if input.position() != bytes.len() as u64 {
        return Err(invalid_data("trailing bytes after MessagePack value"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_has_stable_little_endian_layout() {
        let header = Header {
            started_at_unix_ns: 0x0706_0504_0302_0100,
            metadata_offset: HEADER_SIZE as u64,
            metadata_len: 4,
            footer_offset: 68,
            duration_ns: 42,
        };
        let bytes = header.encode();
        assert_eq!(
            &bytes[..18],
            b"NVREP\0\0\x01\x01\0\0\x01\x02\x03\x04\x05\x06\x07"
        );
        assert_eq!(
            &bytes[18..50],
            &[64, 4, 68, 42].map(u64::to_le_bytes).concat()
        );
        assert_eq!(&bytes[50..], &[0; 14]);
        assert_eq!(Header::decode(&bytes).unwrap(), header);
        assert!(Header::decode(&bytes[..63]).is_err());
    }

    #[test]
    fn unpack_rejects_trailing_messagepack_values() {
        assert!(unpack::<u8>(&[0x01, 0x02]).is_err());
    }
}
