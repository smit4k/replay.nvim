use std::io::{self, Write};

const TIMESTAMP_SCALE_NS: u64 = 1_000;
const NS_PER_SECOND: u64 = 1_000_000_000;

// A streaming Matroska segment with one uncompressed RGBA video track.
pub(super) fn write_header(
    input: &mut impl Write,
    width: u32,
    height: u32,
    fps: u32,
) -> io::Result<()> {
    let mut ebml = Vec::new();
    uint(&mut ebml, 0x4286, 1); // EBMLVersion
    uint(&mut ebml, 0x42f7, 1); // EBMLReadVersion
    uint(&mut ebml, 0x42f2, 4); // EBMLMaxIDLength
    uint(&mut ebml, 0x42f3, 8); // EBMLMaxSizeLength
    element(&mut ebml, 0x4282, b"matroska"); // DocType
    uint(&mut ebml, 0x4287, 2); // DocTypeVersion
    uint(&mut ebml, 0x4285, 2); // DocTypeReadVersion

    let mut info = Vec::new();
    uint(&mut info, 0x2a_d7b1, TIMESTAMP_SCALE_NS); // TimestampScale
    element(&mut info, 0x4d80, b"replay.nvim"); // MuxingApp
    element(&mut info, 0x5741, b"replay.nvim"); // WritingApp

    let mut video = Vec::new();
    uint(&mut video, 0xb0, width.into()); // PixelWidth
    uint(&mut video, 0xba, height.into()); // PixelHeight
    element(&mut video, 0x2e_b524, b"RGBA"); // UncompressedFourCC

    let mut track = Vec::new();
    uint(&mut track, 0xd7, 1); // TrackNumber
    uint(&mut track, 0x73c5, 1); // TrackUID
    uint(&mut track, 0x83, 1); // TrackType (video)
    uint(&mut track, 0x9c, 0); // FlagLacing
    element(&mut track, 0x22_b59c, b"und"); // Language
    uint(&mut track, 0x23_e383, NS_PER_SECOND / u64::from(fps)); // DefaultDuration
    element(&mut track, 0x86, b"V_UNCOMPRESSED"); // CodecID
    element(&mut track, 0xe0, &video); // Video
    let mut tracks = Vec::new();
    element(&mut tracks, 0xae, &track); // TrackEntry

    let mut header = Vec::new();
    element(&mut header, 0x1a45_dfa3, &ebml); // EBML
    // The segment size is unknown because stdout and stdin pipes cannot seek.
    header.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xff]);
    element(&mut header, 0x1549_a966, &info); // Info
    element(&mut header, 0x1654_ae6b, &tracks); // Tracks
    input.write_all(&header)
}

pub(super) fn write_frame(
    input: &mut impl Write,
    pixels: &[u8],
    index: u64,
    fps: u32,
) -> io::Result<()> {
    // Start a cluster per frame so even long idle gaps fit the block timestamp range.
    let ticks_per_second = NS_PER_SECOND / TIMESTAMP_SCALE_NS;
    let timestamp = (index * ticks_per_second / u64::from(fps)).to_be_bytes();
    // Track 1, zero relative timestamp, keyframe.
    let block = [0x81, 0, 0, 0x80];
    let block_size = block.len() + pixels.len();
    let timestamp_element_size = 2 + timestamp.len();
    let block_element_size = 1 + size_width(block_size) + block_size;
    let cluster_size = timestamp_element_size + block_element_size;

    let mut header = Vec::with_capacity(32);
    element_header(&mut header, 0x1f43_b675, cluster_size); // Cluster
    element(&mut header, 0xe7, &timestamp); // Timestamp
    element_header(&mut header, 0xa3, block_size); // SimpleBlock
    header.extend_from_slice(&block);
    input.write_all(&header)?;
    input.write_all(pixels)
}

fn uint(output: &mut Vec<u8>, id: u32, value: u64) {
    let bytes = value.to_be_bytes();
    let leading_bytes = (value.leading_zeros() as usize / 8).min(bytes.len() - 1);
    element(output, id, &bytes[leading_bytes..]);
}

fn element(output: &mut Vec<u8>, id: u32, bytes: &[u8]) {
    element_header(output, id, bytes.len());
    output.extend_from_slice(bytes);
}

fn element_header(output: &mut Vec<u8>, id: u32, size: usize) {
    let width = size_width(size);
    let encoded_size = (size as u64 | (1 << (width * 7))).to_be_bytes();
    output.extend_from_slice(&id.to_be_bytes()[id.leading_zeros() as usize / 8..]);
    output.extend_from_slice(&encoded_size[encoded_size.len() - width..]);
}

fn size_width(size: usize) -> usize {
    // The all-ones value is reserved for an unknown size.
    ((usize::BITS - (size + 1).leading_zeros()) as usize).div_ceil(7)
}
