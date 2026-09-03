//! Core Audio Format muxer.

use std::io::{Seek, SeekFrom, Write};

use super::chunk::{
    CHUNK_HEADER_LEN, DATA_EDIT_COUNT_LEN, DESC_FRAMES_PER_PACKET_OFFSET, DESC_LEN, Desc,
    FILE_HEADER_LEN, LAYOUT_TAG_MONO, LAYOUT_TAG_STEREO, MAGIC, PaktHeader, SIZE_TO_END, VERSION,
    chunk_type, write_chunk_header, write_varint,
};
use crate::ogg::{GRANULE_RATE, OpusHead};
use crate::{Error, Result};

/// Writes Opus packets into a Core Audio Format file — the container Apple's
/// own recorder and player use for Opus.
///
/// The header chunks are written by the constructor and the packets as they
/// arrive; [`finish`](Self::finish) writes the packet table, patches the two
/// sizes that could not be known until then, and **must** be called. Dropping
/// the writer does the same on a best-effort basis but cannot report an I/O
/// failure, and a file without its table cannot be read at all.
///
/// ```no_run
/// use opus_pure::{CafOpusWriter, OpusHead};
///
/// let file = std::fs::File::create("out.caf")?;
/// let mut w = CafOpusWriter::new(std::io::BufWriter::new(file), OpusHead::new(2, 48_000)?)?;
/// // w.write_packet(&packet)?;  // duration read from the packet itself
/// w.finish()?;
/// # Ok::<(), opus_pure::Error>(())
/// ```
///
/// The API is the [`OggOpusWriter`](crate::OggOpusWriter)'s, so the encode
/// recipes written for one container work for the other by changing the type:
/// [`write_packet`](Self::write_packet) reads each packet's duration out of
/// its TOC byte, [`granule`](Self::granule) is the running 48 kHz sample count,
/// and [`write_packet_with_duration`](Self::write_packet_with_duration) states
/// an end-trim — which this container records as its remainder frames.
///
/// # What the file says
///
/// A `desc` chunk declaring Opus at 48 kHz, a `chan` chunk naming the mono or
/// stereo layout, an `info` chunk naming this crate as the encoder, the `data`
/// chunk, and the `pakt` packet table after it. The `desc` rate is 48 kHz
/// whatever rate the encoder ran at, as it is in every file Apple writes: the
/// table's frame counts have to be in some rate, and 48 kHz is the one Opus
/// itself counts in, so pre-skip and durations go in unchanged.
///
/// The table states one frame count for every packet when they all share one,
/// which is the shape Apple's own files have, and a count per packet when they
/// do not. Both read back on macOS and iOS. No `kuki` chunk is written: Apple's
/// carries its encoder's settings rather than anything a decoder needs, and
/// Apple's decoder reads a file without one.
///
/// # Scope
///
/// Mono and stereo, which is every file Apple's recorder produces. A header
/// with an output gain is refused rather than written without it, because the
/// container has nowhere to carry the gain and a file that silently plays at
/// the wrong level is worse than an error.
pub struct CafOpusWriter<W: Write + Seek> {
    /// `None` only after `finish` has taken it.
    sink: Option<W>,
    /// The header's pre-skip, which becomes the table's priming frames.
    pre_skip: u32,
    /// Where `desc` states the frames per packet, patched once they are known.
    frames_per_packet_pos: u64,
    /// Where `data` states its size, patched once it is known.
    data_size_pos: u64,
    /// Every packet's size in bytes and duration in 48 kHz samples.
    packets: Vec<(u64, u32)>,
    /// Bytes of packets written so far.
    data_bytes: u64,
    /// Running granule position, as the caller stated it: the pre-skip is
    /// counted in it, as in Ogg, since the first packets carry those samples.
    granule: i64,
    /// What the packets written so far actually decode to, at 48 kHz.
    decoded: i64,
    finished: bool,
}

/// Shows how much of the file has been written, without requiring `W: Debug`.
impl<W: Write + Seek> std::fmt::Debug for CafOpusWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CafOpusWriter")
            .field("packets", &self.packets.len())
            .field("data_bytes", &self.data_bytes)
            .field("granule", &self.granule)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Seek> CafOpusWriter<W> {
    /// Start a file, writing every chunk ahead of the audio.
    ///
    /// Writing starts at the sink's current position, so a file with something
    /// ahead of the CAF is fine; the positions patched at
    /// [`finish`](Self::finish) are measured from there.
    pub fn new(mut sink: W, head: OpusHead) -> Result<Self> {
        let layout = match head.channel_count {
            1 => LAYOUT_TAG_MONO,
            2 => LAYOUT_TAG_STEREO,
            _ => {
                return Err(Error::InvalidArgument(
                    "caf Opus is mono or stereo; a surround stream cannot be written",
                ));
            }
        };
        if head.output_gain_q8 != 0 {
            return Err(Error::InvalidArgument(
                "caf has nowhere to carry an output gain; apply it or zero it first",
            ));
        }

        let start = sink.stream_position()?;
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // file flags
        debug_assert_eq!(out.len(), FILE_HEADER_LEN);

        // The frames per packet are not known until the last packet has been
        // written; the field is patched in `finish`.
        let frames_per_packet_pos =
            start + (out.len() + CHUNK_HEADER_LEN + DESC_FRAMES_PER_PACKET_OFFSET) as u64;
        write_chunk_header(&mut out, chunk_type::DESC, DESC_LEN as i64);
        Desc {
            sample_rate: f64::from(GRANULE_RATE),
            bytes_per_packet: 0,
            frames_per_packet: 0,
            channels: u32::from(head.channel_count),
        }
        .write(&mut out);

        write_chunk_header(&mut out, chunk_type::CHAN, 12);
        out.extend_from_slice(&layout.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // channel bitmap: unused with a tag
        out.extend_from_slice(&0u32.to_be_bytes()); // no channel descriptions

        let encoder = concat!("opus-pure ", env!("CARGO_PKG_VERSION"));
        let info_len = 4 + "encoder\0".len() + encoder.len() + 1;
        write_chunk_header(&mut out, chunk_type::INFO, info_len as i64);
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(b"encoder\0");
        out.extend_from_slice(encoder.as_bytes());
        out.push(0);

        // The size is patched in `finish`; until then the chunk is marked as
        // running to the end of the file, which is what it does.
        let data_size_pos = start + (out.len() + 4) as u64;
        write_chunk_header(&mut out, chunk_type::DATA, SIZE_TO_END);
        out.extend_from_slice(&0u32.to_be_bytes()); // edit count

        sink.write_all(&out)?;
        Ok(CafOpusWriter {
            sink: Some(sink),
            pre_skip: u32::from(head.pre_skip),
            frames_per_packet_pos,
            data_size_pos,
            packets: Vec::new(),
            data_bytes: 0,
            granule: 0,
            decoded: 0,
            finished: false,
        })
    }

    /// The granule position the file has reached: the 48 kHz samples every
    /// packet written so far decodes to, pre-skip included.
    ///
    /// The same number [`OggOpusWriter::granule`](crate::OggOpusWriter::granule)
    /// reports, for the same use: the final granule a gapless file wants is the
    /// header's pre-skip plus the audio, and the difference between that and
    /// this, read just before the last packet, is that packet's stated
    /// duration.
    pub fn granule(&self) -> i64 {
        self.granule
    }

    /// Append one Opus packet, taking its duration from the packet itself.
    ///
    /// Use [`write_packet_with_duration`](Self::write_packet_with_duration)
    /// only for the end-trim: a final packet stated shorter than it decodes,
    /// so the file ends where the audio does.
    pub fn write_packet(&mut self, packet: &[u8]) -> Result<()> {
        let samples_48k = crate::packet::samples_48k(packet)? as u32;
        self.write_packet_with_duration(packet, samples_48k)
    }

    /// Append one Opus packet, stating its duration explicitly.
    ///
    /// `samples_48k` is the packet's duration in 48 kHz samples. The packet
    /// table records what the packet *actually* decodes to, read from its TOC
    /// byte; what is stated here moves [`granule`](Self::granule), and the gap
    /// between the two at [`finish`](Self::finish) becomes the file's
    /// remainder frames — the padding a player drops from the end. Stating
    /// more than the packets carry is an error at `finish`, since a file
    /// cannot promise audio it does not hold.
    pub fn write_packet_with_duration(&mut self, packet: &[u8], samples_48k: u32) -> Result<()> {
        if self.finished {
            return Err(Error::InvalidArgument("writer has already been finished"));
        }
        if packet.is_empty() {
            return Err(Error::InvalidArgument("Opus packets cannot be empty"));
        }
        if samples_48k > 120 * GRANULE_RATE / 1000 {
            return Err(Error::InvalidArgument(
                "an Opus packet cannot exceed 120 ms (5760 samples at 48 kHz)",
            ));
        }
        let actual = crate::packet::samples_48k(packet)? as u32;

        let sink = self
            .sink
            .as_mut()
            .ok_or(Error::Internal("caf writer used after finish"))?;
        sink.write_all(packet)?;

        self.packets.push((packet.len() as u64, actual));
        self.data_bytes += packet.len() as u64;
        self.granule += i64::from(samples_48k);
        self.decoded += i64::from(actual);
        Ok(())
    }

    /// The sink being written to.
    pub fn get_ref(&self) -> Option<&W> {
        self.sink.as_ref()
    }

    /// The sink being written to, mutably.
    ///
    /// Writing to it, or moving its position, corrupts the file; this is for
    /// the sink's own controls, like asking a file for its handle.
    pub fn get_mut(&mut self) -> Option<&mut W> {
        self.sink.as_mut()
    }

    /// Write the packet table, patch the sizes, flush, and return the sink.
    pub fn finish(mut self) -> Result<W> {
        self.finish_in_place()?;
        self.sink
            .take()
            .ok_or(Error::Internal("caf writer sink taken twice"))
    }

    fn finish_in_place(&mut self) -> Result<()> {
        if self.finished || self.sink.is_none() {
            return Ok(());
        }
        self.finished = true;

        if self.granule > self.decoded {
            return Err(Error::InvalidArgument(
                "stated durations exceed what the packets decode to",
            ));
        }

        // One count for all when the packets agree, as Apple's files have it;
        // otherwise zero here and a count per packet in the table. A file with
        // no packets states the conventional 20 ms frame, since Apple's reader
        // wants a non-zero count and there is no packet to take one from.
        let uniform = self
            .packets
            .first()
            .map(|p| p.1)
            .filter(|&f| self.packets.iter().all(|p| p.1 == f));
        let frames_per_packet = match uniform {
            Some(f) => f,
            None if self.packets.is_empty() => GRANULE_RATE / 50,
            None => 0,
        };

        // priming + valid + remainder is what the packets decode to. A stream
        // stated shorter than its own pre-skip has no audio, and its padding is
        // whatever lies past the pre-skip.
        let pre_skip = i64::from(self.pre_skip);
        let valid = (self.granule - pre_skip).max(0);
        let remainder = (self.decoded - pre_skip - valid).max(0);
        let remainder = i32::try_from(remainder)
            .map_err(|_| Error::InvalidArgument("the end padding does not fit the packet table"))?;

        let mut pakt = Vec::with_capacity(24 + self.packets.len() * 3);
        PaktHeader {
            packets: self.packets.len() as i64,
            valid_frames: valid,
            priming_frames: self.pre_skip as i32,
            remainder_frames: remainder,
        }
        .write(&mut pakt);
        for &(bytes, frames) in &self.packets {
            write_varint(&mut pakt, bytes);
            if frames_per_packet == 0 {
                write_varint(&mut pakt, u64::from(frames));
            }
        }

        let sink = self
            .sink
            .as_mut()
            .ok_or(Error::Internal("caf writer used after finish"))?;
        sink.seek(SeekFrom::Start(self.frames_per_packet_pos))?;
        sink.write_all(&frames_per_packet.to_be_bytes())?;
        sink.seek(SeekFrom::Start(self.data_size_pos))?;
        let data_size = self.data_bytes + DATA_EDIT_COUNT_LEN as u64;
        sink.write_all(&(data_size as i64).to_be_bytes())?;
        // Back to the end of the packets, which is also the end of what has
        // been written: the table goes after the data, as FFmpeg's does.
        sink.seek(SeekFrom::Start(self.data_size_pos + 8 + data_size))?;

        let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + pakt.len());
        write_chunk_header(&mut out, chunk_type::PAKT, pakt.len() as i64);
        out.extend_from_slice(&pakt);
        sink.write_all(&out)?;
        sink.flush()?;
        Ok(())
    }
}

impl<W: Write + Seek> Drop for CafOpusWriter<W> {
    fn drop(&mut self) {
        // Best-effort: `finish` is the supported way to learn whether the
        // write worked.
        let _ = self.finish_in_place();
    }
}
