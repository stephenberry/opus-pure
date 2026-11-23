//! Core Audio Format encapsulation of Opus streams — the `.caf` files Apple's
//! platforms record and play.
//!
//! iOS and macOS encode Opus natively, but only into CAF: `AVAudioRecorder`
//! asked for Opus writes one, and `AVAudioPlayer` reads one, while neither
//! touches an Ogg `.opus` file. Everywhere else it is the other way round. The
//! packets inside are ordinary Opus, so moving audio across that line is a
//! matter of re-framing rather than re-encoding, and this module is the other
//! half of that: [`CafOpusReader`] and [`CafOpusWriter`] present the same API
//! as [`OggOpusReader`](crate::OggOpusReader) and
//! [`OggOpusWriter`](crate::OggOpusWriter), so a stream goes from one container
//! to the other packet by packet, byte for byte.
//!
//! # How the two containers correspond
//!
//! An Ogg Opus stream is described by an `OpusHead` and a granule position on
//! every page; a CAF is described by a `desc` chunk and a packet table
//! (`pakt`) whose three frame counts say where the audio starts and ends.
//! They carry the same facts:
//!
//! | Ogg (RFC 7845)                            | CAF                                   |
//! |-------------------------------------------|---------------------------------------|
//! | `OpusHead::channel_count`                 | `desc` channels per frame             |
//! | `OpusHead::pre_skip` (§4.2)               | `pakt` priming frames                 |
//! | final granule − pre-skip: the audio       | `pakt` valid frames                   |
//! | end-trim (§4.4): a short final granule    | `pakt` remainder frames               |
//! | a packet's duration, from its TOC byte    | `desc` frames per packet, or per entry |
//!
//! Both count in 48 kHz samples: Ogg by definition, and CAF because every file
//! Apple or FFmpeg writes declares 48 kHz in `desc`. The reader converts if a
//! file declares one of Opus's lower rates instead, and the writer always
//! declares 48 kHz.
//!
//! # Remuxing
//!
//! Each reader yields [`OggPacket`](crate::OggPacket)s whose `page_granule` is
//! the sample count decodable through that packet, and each writer's
//! [`granule`](CafOpusWriter::granule) is the count it has written, so the
//! end-trim carries across as a subtraction on the last packet. This loop is
//! the whole of a CAF-to-Ogg conversion, and swapping the two types is the
//! whole of the reverse:
//!
//! ```
//! use opus_pure::{CafOpusReader, OggOpusWriter, Result};
//! use std::io::{Read, Seek, Write};
//!
//! fn caf_to_ogg<R: Read + Seek, W: Write>(source: R, sink: W) -> Result<W> {
//!     let mut reader = CafOpusReader::new(source)?;
//!     let mut writer = OggOpusWriter::new(sink, reader.head().clone())?;
//!     let mut packets = reader.packets().peekable();
//!     while let Some(packet) = packets.next() {
//!         let packet = packet?;
//!         if packets.peek().is_none() {
//!             // The last packet states where the audio ends, which may be
//!             // short of what it decodes to.
//!             let duration = packet.page_granule - writer.granule();
//!             writer.write_packet_with_duration(&packet.data, duration as u32)?;
//!         } else {
//!             writer.write_packet(&packet.data)?;
//!         }
//!     }
//!     writer.finish()
//! }
//! #
//! # // A file to convert: one second of silence, encoded and written as CAF.
//! # use opus_pure::{Application, CafOpusWriter, MAX_PACKET_BYTES, OpusEncoder, OpusHead};
//! # let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip)?;
//! # let head = OpusHead::for_encoder(&encoder, 48_000);
//! # let mut caf = CafOpusWriter::new(std::io::Cursor::new(Vec::new()), head)?;
//! # let mut packet = vec![0u8; MAX_PACKET_BYTES];
//! # for _ in 0..50 {
//! #     let n = encoder.encode(&[0.0f32; 960], 960, &mut packet)?;
//! #     caf.write_packet(&packet[..n])?;
//! # }
//! # let caf = caf.finish()?.into_inner();
//! # let ogg = caf_to_ogg(std::io::Cursor::new(caf), Vec::new())?;
//! # assert_eq!(&ogg[..4], b"OggS");
//! # Ok::<(), opus_pure::Error>(())
//! ```
//!
//! The reader is generic over `Read + Seek` and the writer over `Write + Seek`,
//! where the Ogg pair need only `Read` and `Write`. That is the format: the
//! packet table is a chunk that may sit on either side of the audio, and its
//! contents are not known until the audio has all been seen. Files and
//! [`Cursor`](std::io::Cursor)s seek; a socket does not, and a CAF could not
//! be streamed over one in any case.
//!
//! # Scope
//!
//! Mono and stereo streams, which is every Opus file Apple's platforms make.
//! Multichannel Opus in CAF has no fixed convention for carrying the channel
//! mapping, and is refused rather than guessed at. The reader accepts the
//! files Apple's Core Audio and FFmpeg write, including FFmpeg's without a
//! frame count anywhere; the writer's files read back through Apple's decoder
//! and FFmpeg's.

mod chunk;
mod reader;
mod writer;

pub use reader::{CafOpusReader, Packets};
pub use writer::CafOpusWriter;

#[cfg(test)]
mod tests;
