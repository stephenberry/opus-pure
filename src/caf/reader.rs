//! Core Audio Format demuxer.

use std::io::{Read, Seek, SeekFrom};

use super::chunk::{
    CHUNK_HEADER_LEN, DATA_EDIT_COUNT_LEN, DESC_LEN, Desc, FILE_HEADER_LEN, MAGIC, PAKT_HEADER_LEN,
    PaktHeader, SIZE_TO_END, VERSION, be_i64, chunk_type, read_varint,
};
use crate::ogg::{GRANULE_RATE, OggPacket, OpusHead};
use crate::{Error, Result};

/// The longest a `pakt` chunk is allowed to be before it is read into memory.
///
/// The table is about one byte per packet, so this is well over a year of
/// audio; a file claiming more is corrupt, not long, and the point is that a
/// claimed size is not an allocation until it has been checked against
/// something.
const MAX_PAKT_LEN: i64 = 1 << 28;

/// The longest an Opus packet can be, in 48 kHz samples (RFC 6716 §3.2.1).
const MAX_PACKET_SAMPLES_48K: u64 = 120 * GRANULE_RATE as u64 / 1000;

/// Where each packet's duration comes from.
///
/// The `desc` chunk either states one frame count for every packet or leaves
/// it to the table, which then states one per packet. A table that states
/// neither is malformed, but FFmpeg writes one when it does not know the frame
/// size, and its packets are still perfectly good Opus — each carries its own
/// duration in its TOC byte, so that is where it is read from.
#[derive(Debug)]
enum Duration {
    Constant(u64),
    PerPacket(Vec<u64>),
    FromPacket,
}

/// Reads Opus packets out of a Core Audio Format file.
///
/// The constructor reads every chunk header, so [`head`](Self::head) is
/// available immediately and [`read_packet`](Self::read_packet) yields audio
/// from the first call. The packets come back as [`OggPacket`]s, with
/// `page_granule` the 48 kHz sample count the file says is decodable through
/// that packet and `end_of_stream` set on the last, so the decode loop — and
/// [`Trim`](crate::Trim), which turns the decoder's output back into the audio
/// that was encoded — is the same one an [`OggOpusReader`](crate::OggOpusReader)
/// feeds:
///
/// ```no_run
/// use opus_pure::{CafOpusReader, MAX_PACKET_SAMPLES, Trim};
///
/// let file = std::fs::File::open("in.caf")?;
/// let mut reader = CafOpusReader::new(std::io::BufReader::new(file))?;
/// let channels = reader.head().channel_count as usize;
/// let mut decoder = reader.head().decoder(48_000)?;
/// let mut trim = Trim::new(reader.head(), 48_000, channels)?;
///
/// let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
/// let mut pcm = Vec::new();
/// for packet in reader.packets() {
///     let packet = packet?;
///     let n = decoder.decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)?;
///     pcm.extend_from_slice(trim.keep(&packet, &block[..n * channels]));
/// }
/// # Ok::<(), opus_pure::Error>(())
/// ```
///
/// # Why the source has to seek
///
/// A CAF file's packet table is a chunk of its own, and nothing fixes where it
/// sits: Apple's recorder writes it ahead of the audio and FFmpeg writes it
/// after, once it knows every packet's size. Opus packets are not
/// self-delimiting, so without the table there is no telling where one ends
/// and the next begins, and a reader that could not skip past the audio to
/// find it would have to hold the whole file in memory instead. Files seek,
/// and so does [`Cursor`](std::io::Cursor) over a buffer.
///
/// # Reading forward
///
/// Packets are read in order from the first. Playing a file again means
/// starting over: take the source back with [`into_inner`](Self::into_inner)
/// and construct a new reader. A decoder carried across that boundary needs
/// [`reset_state`](crate::OpusDecoder::reset_state), and the `Trim` needs
/// replacing.
pub struct CafOpusReader<R: Read + Seek> {
    source: R,
    head: OpusHead,
    /// Every packet's size in bytes, in file order.
    sizes: Vec<u64>,
    duration: Duration,
    /// Index into `sizes` of the next packet to yield.
    next: usize,
    /// 48 kHz samples decodable through the packets yielded so far.
    granule: i64,
    /// The file's own word on where the audio ends: priming plus valid frames,
    /// which is what the last packet reports whatever the packets add up to.
    final_granule: i64,
    /// 48 kHz samples per frame at the `desc` chunk's rate.
    ticks: u64,
}

/// Shows where the reader has got to, without requiring `R: Debug` — the
/// source is a file far more often than it is something printable.
impl<R: Read + Seek> std::fmt::Debug for CafOpusReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CafOpusReader")
            .field("head", &self.head)
            .field("packets", &self.sizes.len())
            .field("next", &self.next)
            .field("granule", &self.granule)
            .field("final_granule", &self.final_granule)
            .finish_non_exhaustive()
    }
}

/// The three chunks a readable file has to have, found by the chunk walk.
#[derive(Default)]
struct Chunks {
    desc: Option<Desc>,
    pakt: Option<Vec<u8>>,
    /// Offset of the first packet and the number of bytes of packets.
    data: Option<(u64, u64)>,
}

impl<R: Read + Seek> CafOpusReader<R> {
    /// Read the file's chunk headers and position the reader at the first
    /// packet.
    ///
    /// The file is checked for consistency before anything is returned: the
    /// packet table has to account for exactly the bytes in the `data` chunk,
    /// and every count in it has to be one Opus can honour.
    pub fn new(mut source: R) -> Result<Self> {
        let chunks = walk_chunks(&mut source)?;
        let desc = chunks
            .desc
            .ok_or(Error::InvalidStream("caf file has no desc chunk"))?;
        let pakt = chunks
            .pakt
            .ok_or(Error::InvalidStream("caf file has no packet table"))?;
        let (first_packet, data_len) = chunks
            .data
            .ok_or(Error::InvalidStream("caf file has no data chunk"))?;

        let ticks = ticks_per_frame(desc.sample_rate)?;
        let channels = match desc.channels {
            1 | 2 => desc.channels as u8,
            _ => {
                return Err(Error::InvalidStream(
                    "caf Opus with more than two channels is not supported",
                ));
            }
        };

        let (header, sizes, duration) = parse_packet_table(&desc, &pakt, data_len, ticks)?;

        let pre_skip = u64::from(header.priming_frames as u32) * ticks;
        let pre_skip = u16::try_from(pre_skip)
            .map_err(|_| Error::InvalidStream("caf priming frames exceed what Opus allows"))?;
        let final_granule = (u64::from(header.priming_frames as u32) + header.valid_frames as u64)
            .checked_mul(ticks)
            .and_then(|g| i64::try_from(g).ok())
            .ok_or(Error::InvalidStream("caf frame counts overflow"))?;

        let mut head = OpusHead::new(channels, desc.sample_rate as u32)?;
        head.pre_skip = pre_skip;

        source.seek(SeekFrom::Start(first_packet))?;
        Ok(CafOpusReader {
            source,
            head,
            sizes,
            duration,
            next: 0,
            granule: 0,
            final_granule,
            ticks,
        })
    }

    /// The stream's identification header, as an Ogg file would carry it.
    ///
    /// Built from the file: the channel count and sample rate from `desc`, and
    /// the pre-skip from the packet table's priming frames. It is what
    /// [`OggOpusWriter`](crate::OggOpusWriter) wants when the packets are
    /// being remuxed, and what [`OpusHead::decoder`] and [`Trim`](crate::Trim)
    /// want when they are being decoded.
    pub fn head(&self) -> &OpusHead {
        &self.head
    }

    /// How many packets the file holds.
    pub fn packet_count(&self) -> usize {
        self.sizes.len()
    }

    /// The samples of audio the file declares, at 48 kHz, after the pre-skip
    /// and the end padding have been taken off: the packet table's valid
    /// frames.
    ///
    /// Known before a packet has been read, which an Ogg file cannot say
    /// without reading to its end.
    pub fn audio_samples_48k(&self) -> u64 {
        (self.final_granule as u64).saturating_sub(u64::from(self.head.pre_skip))
    }

    /// The next packet, or `None` after the last.
    pub fn read_packet(&mut self) -> Result<Option<OggPacket>> {
        let Some(&size) = self.sizes.get(self.next) else {
            return Ok(None);
        };
        let mut data = vec![0u8; size as usize];
        if let Err(e) = self.source.read_exact(&mut data) {
            // The source's position is no longer known, so there is no
            // reading on from here: the error is reported once and the
            // stream is over, as an Ogg stream cut mid-packet is.
            self.next = self.sizes.len();
            return Err(Error::Io(e));
        }

        let samples_48k = match &self.duration {
            Duration::Constant(frames) => frames * self.ticks,
            Duration::PerPacket(frames) => frames[self.next] * self.ticks,
            // A packet the TOC cannot describe will not decode either; it
            // advances nothing and the decoder reports it.
            Duration::FromPacket => crate::packet::samples_48k(&data).unwrap_or(0) as u64,
        };
        self.next += 1;
        let last = self.next == self.sizes.len();

        // The last packet reports the file's own count of where the audio
        // ends rather than the running total, which is how the end padding is
        // stated: exactly as an Ogg file's final page under-claims.
        self.granule = if last {
            self.final_granule
        } else {
            self.granule.saturating_add(samples_48k as i64)
        };
        Ok(Some(OggPacket::new(data, self.granule, last)))
    }

    /// The remaining packets, as an iterator.
    ///
    /// The same packets [`read_packet`](Self::read_packet) yields, in a form
    /// that composes; it ends at the first error as well as after the last
    /// packet, so a truncated file stops rather than looping.
    pub fn packets(&mut self) -> Packets<'_, R> {
        Packets {
            reader: self,
            done: false,
        }
    }

    /// The underlying source, giving up the ability to read further packets.
    pub fn into_inner(self) -> R {
        self.source
    }

    /// The underlying source.
    pub fn get_ref(&self) -> &R {
        &self.source
    }
}

/// Read the file header and every chunk header, collecting the three chunks a
/// stream is made of and skipping the rest.
fn walk_chunks<R: Read + Seek>(source: &mut R) -> Result<Chunks> {
    let mut header = [0u8; FILE_HEADER_LEN];
    if read_or_eof(source, &mut header)? < FILE_HEADER_LEN {
        return Err(Error::InvalidStream("caf file is shorter than its header"));
    }
    if &header[..4] != MAGIC {
        return Err(Error::InvalidStream("not a caf file: no `caff` signature"));
    }
    if u16::from_be_bytes([header[4], header[5]]) != VERSION {
        return Err(Error::InvalidStream("unsupported caf file version"));
    }

    let mut chunks = Chunks::default();
    let mut raw = [0u8; CHUNK_HEADER_LEN];
    loop {
        match read_or_eof(source, &mut raw)? {
            0 => break,
            n if n < CHUNK_HEADER_LEN => {
                return Err(Error::InvalidStream("caf file ends inside a chunk header"));
            }
            _ => {}
        }
        let kind: [u8; 4] = raw[..4].try_into().expect("4 bytes");
        let size = be_i64(&raw[4..]);
        if size < SIZE_TO_END {
            return Err(Error::InvalidStream("caf chunk has a negative size"));
        }

        match &kind {
            chunk_type::DESC => {
                if size != DESC_LEN as i64 {
                    return Err(Error::InvalidStream("caf desc chunk is not 32 bytes"));
                }
                let mut body = [0u8; DESC_LEN];
                read_exact(source, &mut body)?;
                chunks.desc = Some(Desc::parse(&body)?);
            }
            chunk_type::PAKT => {
                if !((PAKT_HEADER_LEN as i64)..=MAX_PAKT_LEN).contains(&size) {
                    return Err(Error::InvalidStream(
                        "caf packet table has an impossible size",
                    ));
                }
                let mut body = vec![0u8; size as usize];
                read_exact(source, &mut body)?;
                chunks.pakt = Some(body);
            }
            chunk_type::DATA => {
                let start = source.stream_position()?;
                let len = if size == SIZE_TO_END {
                    // Runs to the end of the file, so nothing can follow it.
                    let end = source.seek(SeekFrom::End(0))?;
                    end.saturating_sub(start)
                } else {
                    size as u64
                };
                if len < DATA_EDIT_COUNT_LEN as u64 {
                    return Err(Error::InvalidStream(
                        "caf data chunk is shorter than its edit count",
                    ));
                }
                chunks.data = Some((
                    start + DATA_EDIT_COUNT_LEN as u64,
                    len - DATA_EDIT_COUNT_LEN as u64,
                ));
                if size == SIZE_TO_END {
                    break;
                }
                source.seek(SeekFrom::Start(start + len))?;
            }
            _ => {
                if size == SIZE_TO_END {
                    return Err(Error::InvalidStream(
                        "only a caf data chunk may run to the end of the file",
                    ));
                }
                source.seek(SeekFrom::Current(size))?;
            }
        }
    }
    Ok(chunks)
}

/// 48 kHz samples per frame at a `desc` sample rate.
///
/// CAF counts frames at the `desc` rate, while an Opus stream's pre-skip and
/// durations are counted at 48 kHz whatever rate the encoder ran at. Every
/// rate Opus runs at divides 48 000, so the conversion is exact; any other rate
/// is not an Opus stream.
fn ticks_per_frame(sample_rate: f64) -> Result<u64> {
    let rate = match sample_rate {
        r if r.fract() == 0.0 && r > 0.0 && r <= f64::from(GRANULE_RATE) => r as u32,
        _ => return Err(Error::InvalidStream("caf sample rate is not one Opus uses")),
    };
    match rate {
        48_000 => Ok(1),
        24_000 => Ok(2),
        16_000 => Ok(3),
        12_000 => Ok(4),
        8_000 => Ok(6),
        _ => Err(Error::InvalidStream("caf sample rate is not one Opus uses")),
    }
}

/// The packet table, checked against the `desc` chunk that decides its shape
/// and the `data` chunk it has to account for.
fn parse_packet_table(
    desc: &Desc,
    pakt: &[u8],
    data_len: u64,
    ticks: u64,
) -> Result<(PaktHeader, Vec<u64>, Duration)> {
    let header = PaktHeader::parse(pakt)?;
    if header.packets < 0
        || header.valid_frames < 0
        || header.priming_frames < 0
        || header.remainder_frames < 0
    {
        return Err(Error::InvalidStream(
            "caf packet table has a negative count",
        ));
    }
    // A table entry is at least one byte, so a packet count the table cannot
    // hold is a lie — unless there is no table at all, which only a constant
    // packet size allows, and then the data chunk bounds it instead.
    let table = &pakt[PAKT_HEADER_LEN..];
    let count = usize::try_from(header.packets)
        .ok()
        .filter(|&n| {
            if desc.bytes_per_packet != 0 && desc.frames_per_packet != 0 {
                (n as u64).checked_mul(u64::from(desc.bytes_per_packet)) == Some(data_len)
            } else {
                n <= table.len()
            }
        })
        .ok_or(Error::InvalidStream(
            "caf packet count does not fit its table",
        ))?;

    let mut values = Vec::with_capacity(count * 2);
    let mut pos = 0;
    while pos < table.len() {
        values.push(read_varint(table, &mut pos)?);
    }

    let (sizes, duration) = match (desc.bytes_per_packet == 0, desc.frames_per_packet == 0) {
        (false, false) => (
            vec![u64::from(desc.bytes_per_packet); count],
            Duration::Constant(u64::from(desc.frames_per_packet)),
        ),
        (true, false) if values.len() == count => (
            values,
            Duration::Constant(u64::from(desc.frames_per_packet)),
        ),
        (false, true) if values.len() == count => (
            vec![u64::from(desc.bytes_per_packet); count],
            Duration::PerPacket(values),
        ),
        (true, true) if values.len() == count * 2 => {
            let sizes = values.iter().step_by(2).copied().collect();
            let frames = values.iter().skip(1).step_by(2).copied().collect();
            (sizes, Duration::PerPacket(frames))
        }
        // The malformed-but-real case: neither the desc nor the table states a
        // frame count. The packets state their own.
        (true, true) if values.len() == count => (values, Duration::FromPacket),
        _ => {
            return Err(Error::InvalidStream(
                "caf packet table does not match the packet count",
            ));
        }
    };

    let mut total = 0u64;
    for &size in &sizes {
        if size == 0 {
            return Err(Error::InvalidStream(
                "caf packet table has a zero-length packet",
            ));
        }
        total = total
            .checked_add(size)
            .ok_or(Error::InvalidStream("caf packet sizes overflow"))?;
    }
    if total != data_len {
        return Err(Error::InvalidStream(
            "caf packet table does not account for the data chunk",
        ));
    }

    let too_long = |frames: u64| {
        frames
            .checked_mul(ticks)
            .is_none_or(|samples| samples > MAX_PACKET_SAMPLES_48K)
    };
    let over = match &duration {
        Duration::Constant(frames) => !sizes.is_empty() && too_long(*frames),
        Duration::PerPacket(frames) => frames.iter().any(|&f| too_long(f)),
        Duration::FromPacket => false,
    };
    if over {
        return Err(Error::InvalidStream(
            "caf packet table claims a packet over 120 ms",
        ));
    }

    Ok((header, sizes, duration))
}

fn read_exact<R: Read>(source: &mut R, buf: &mut [u8]) -> Result<()> {
    if read_or_eof(source, buf)? < buf.len() {
        return Err(Error::InvalidStream("caf file ends inside a chunk"));
    }
    Ok(())
}

/// Fill `buf`, returning how many bytes were read; short only at EOF.
fn read_or_eof<R: Read>(source: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(filled)
}

/// Iterator over a [`CafOpusReader`]'s remaining packets, from
/// [`CafOpusReader::packets`].
#[derive(Debug)]
pub struct Packets<'a, R: Read + Seek> {
    reader: &'a mut CafOpusReader<R>,
    /// Set once the file has ended or errored, so a caller who keeps polling
    /// gets `None` rather than the same error for ever.
    done: bool,
}

impl<R: Read + Seek> Iterator for Packets<'_, R> {
    type Item = Result<OggPacket>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.reader.read_packet() {
            Ok(Some(p)) => Some(Ok(p)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
