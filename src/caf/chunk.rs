//! The bytes of a Core Audio Format file: the file header, the chunk framing,
//! and the variable-length integers the packet table is made of.
//!
//! Everything in CAF is big-endian. A file is an 8-byte header followed by
//! chunks, each a 4-byte type, a signed 64-bit body size and the body. The
//! chunks this module reads and writes are `desc` (the stream's format),
//! `pakt` (the packet table) and `data` (the packets); the rest are skipped on
//! the way in and, apart from `chan` and `info`, not written on the way out.

use crate::{Error, Result};

/// The file header's magic, `caff`.
pub(super) const MAGIC: &[u8; 4] = b"caff";
/// The only file version there is.
pub(super) const VERSION: u16 = 1;
/// Length of the file header: magic, version and flags.
pub(super) const FILE_HEADER_LEN: usize = 8;
/// Length of a chunk header: type and body size.
pub(super) const CHUNK_HEADER_LEN: usize = 12;

/// The `desc` chunk's format ID for Opus.
pub(super) const FORMAT_OPUS: &[u8; 4] = b"opus";
/// The `desc` body is a fixed 32 bytes.
pub(super) const DESC_LEN: usize = 32;
/// Byte offset of `mFramesPerPacket` within a `desc` body.
pub(super) const DESC_FRAMES_PER_PACKET_OFFSET: usize = 20;
/// The fixed part of a `pakt` body, ahead of the table.
pub(super) const PAKT_HEADER_LEN: usize = 24;
/// A `data` body opens with a 32-bit edit count before the first packet.
pub(super) const DATA_EDIT_COUNT_LEN: usize = 4;

/// A chunk's body size, when the chunk runs to the end of the file. Only
/// `data` may carry it, and only as the last chunk.
pub(super) const SIZE_TO_END: i64 = -1;

/// Chunk types, as the file spells them.
pub(super) mod chunk_type {
    pub const DESC: &[u8; 4] = b"desc";
    pub const CHAN: &[u8; 4] = b"chan";
    pub const INFO: &[u8; 4] = b"info";
    pub const PAKT: &[u8; 4] = b"pakt";
    pub const DATA: &[u8; 4] = b"data";
}

/// The `chan` chunk's layout tags for the two layouts this module writes:
/// `kAudioChannelLayoutTag_Mono` and `kAudioChannelLayoutTag_Stereo`, each
/// `(id << 16) | channels`.
pub(super) const LAYOUT_TAG_MONO: u32 = (100 << 16) | 1;
pub(super) const LAYOUT_TAG_STEREO: u32 = (101 << 16) | 2;

/// The `desc` chunk, as far as an Opus stream uses it.
///
/// `mBytesPerPacket` and `mFramesPerPacket` decide the shape of the packet
/// table: a non-zero value is a constant every packet shares, and a zero means
/// the table carries the value per packet. Opus packets vary in size, so bytes
/// per packet is zero in every file this module has seen; frames per packet is
/// 960 in the files Apple and FFmpeg write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Desc {
    pub sample_rate: f64,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub channels: u32,
}

impl Desc {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() != DESC_LEN {
            return Err(Error::InvalidStream("caf desc chunk is not 32 bytes"));
        }
        if &body[8..12] != FORMAT_OPUS {
            return Err(Error::InvalidStream("caf file does not contain Opus"));
        }
        let sample_rate = f64::from_be_bytes(body[0..8].try_into().expect("8 bytes"));
        // Format flags, bits per channel: both zero for a compressed format and
        // neither read here.
        Ok(Desc {
            sample_rate,
            bytes_per_packet: be_u32(&body[16..20]),
            frames_per_packet: be_u32(&body[DESC_FRAMES_PER_PACKET_OFFSET..24]),
            channels: be_u32(&body[24..28]),
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.sample_rate.to_be_bytes());
        out.extend_from_slice(FORMAT_OPUS);
        out.extend_from_slice(&0u32.to_be_bytes()); // format flags
        out.extend_from_slice(&self.bytes_per_packet.to_be_bytes());
        out.extend_from_slice(&self.frames_per_packet.to_be_bytes());
        out.extend_from_slice(&self.channels.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // bits per channel
    }
}

/// The fixed head of the `pakt` chunk.
///
/// The three frame counts are what make a CAF gapless, and they add up:
/// `priming + valid + remainder` is the number of frames every packet in the
/// file decodes to. Priming is the encoder delay — RFC 7845's pre-skip under
/// another name — and remainder is the padding the encoder added to fill its
/// last frame, so `valid` is the audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaktHeader {
    pub packets: i64,
    pub valid_frames: i64,
    pub priming_frames: i32,
    pub remainder_frames: i32,
}

impl PaktHeader {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < PAKT_HEADER_LEN {
            return Err(Error::InvalidStream("caf pakt chunk is truncated"));
        }
        Ok(PaktHeader {
            packets: be_i64(&body[0..8]),
            valid_frames: be_i64(&body[8..16]),
            priming_frames: be_u32(&body[16..20]) as i32,
            remainder_frames: be_u32(&body[20..24]) as i32,
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.packets.to_be_bytes());
        out.extend_from_slice(&self.valid_frames.to_be_bytes());
        out.extend_from_slice(&self.priming_frames.to_be_bytes());
        out.extend_from_slice(&self.remainder_frames.to_be_bytes());
    }
}

/// Append a chunk header.
pub(super) fn write_chunk_header(out: &mut Vec<u8>, kind: &[u8; 4], size: i64) {
    out.extend_from_slice(kind);
    out.extend_from_slice(&size.to_be_bytes());
}

/// Append one of the packet table's integers.
///
/// Base 128, most significant group first, with the top bit set on every byte
/// but the last — the same encoding MPEG-4 uses for descriptor lengths, which
/// is why FFmpeg reads it with `ff_mp4_read_descr_len`.
pub(super) fn write_varint(out: &mut Vec<u8>, value: u64) {
    // Ten groups of seven bits cover a u64; emit from the first non-zero one.
    let mut groups = [0u8; 10];
    let mut n = 0;
    let mut v = value;
    loop {
        groups[n] = (v & 0x7f) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        let continued = if i > 0 { 0x80 } else { 0 };
        out.push(groups[i] | continued);
    }
}

/// Read one of the packet table's integers, advancing `pos`.
pub(super) fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    // A u64 is at most ten groups; an eleventh means the value does not fit,
    // which is corruption rather than a very large packet.
    for _ in 0..10 {
        let &b = bytes
            .get(*pos)
            .ok_or(Error::InvalidStream("caf packet table is truncated"))?;
        *pos += 1;
        value = (value << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::InvalidStream(
        "caf packet table holds an integer wider than 64 bits",
    ))
}

pub(super) fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().expect("4 bytes"))
}

pub(super) fn be_i64(b: &[u8]) -> i64 {
    i64::from_be_bytes(b[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_every_width() {
        for &v in &[
            0u64,
            1,
            15,
            127,
            128,
            960,
            16_383,
            16_384,
            1 << 21,
            (1 << 35) - 1,
            u64::MAX >> 1,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            write_varint(&mut out, v);
            let mut pos = 0;
            assert_eq!(read_varint(&out, &mut pos).unwrap(), v, "value {v}");
            assert_eq!(pos, out.len(), "value {v} left bytes unread");
        }
    }

    /// 960, the frame count Apple writes, is the two bytes FFmpeg reads it as.
    #[test]
    fn varints_match_the_mp4_descriptor_encoding() {
        let mut out = Vec::new();
        write_varint(&mut out, 960);
        assert_eq!(out, [0x87, 0x40]);
        let mut out = Vec::new();
        write_varint(&mut out, 15);
        assert_eq!(out, [15]);
    }

    #[test]
    fn a_varint_that_never_terminates_is_rejected() {
        let bytes = [0xffu8; 12];
        let mut pos = 0;
        assert!(read_varint(&bytes, &mut pos).is_err());
        let mut pos = 0;
        assert!(read_varint(&[0x80, 0x80], &mut pos).is_err());
    }
}
