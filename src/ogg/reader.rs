//! Ogg Opus demuxer.

use std::io::Read;

use super::header::{OpusHead, OpusTags};
use super::page::{CAPTURE_PATTERN, HEADER_LEN, MAX_PAGE_PAYLOAD, PageHeader, verify_crc};
use crate::{Error, Result};

/// Largest packet the reader will reassemble from continued pages, 16 MiB.
///
/// A packet's own framing bounds its frames but not its padding, so nothing in
/// the format stops a chain of continued pages from growing `partial` for as
/// long as the source keeps delivering. This is far above any packet an
/// encoder produces (a 120 ms packet at the highest rate is under 8 KiB) and
/// exists only so a hostile stream cannot make the reader allocate without
/// limit.
pub(crate) const MAX_OGG_PACKET_BYTES: usize = 16 * 1024 * 1024;

/// One Opus packet recovered from the container.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OggPacket {
    /// The packet, ready to hand to a decoder.
    pub data: Vec<u8>,
    /// Granule position of the page this packet completed on, or `-1` if that
    /// page completed no packet. This is a *page* property: several packets
    /// completing on one page all report the same value, which is the granule
    /// after the last of them.
    pub page_granule: i64,
    /// The packet completed the final page of the stream.
    pub end_of_stream: bool,
}

impl OggPacket {
    /// Build a packet directly, without a container to read it out of.
    ///
    /// The reader produces these; this exists so code that *consumes* them can
    /// be tested without muxing a file first. The interesting logic on the
    /// consuming side is what a caller does with `page_granule` and
    /// `end_of_stream` — the end-trim arithmetic of RFC 7845 §4.4, which
    /// [`Trim`](super::Trim) implements — and a test for it should be able to
    /// state the two edge cases directly rather than construct a stream that
    /// happens to produce them.
    ///
    /// ```
    /// use opus_pure::OggPacket;
    ///
    /// // The last packet of a stream whose final granule trims 160 samples.
    /// let packet = OggPacket::new(vec![0xfc], 48_000, true);
    /// assert!(packet.end_of_stream);
    /// ```
    pub fn new(data: Vec<u8>, page_granule: i64, end_of_stream: bool) -> Self {
        OggPacket {
            data,
            page_granule,
            end_of_stream,
        }
    }
}

/// Reads Opus packets out of an Ogg stream (RFC 7845).
///
/// The constructor consumes the two header packets, so [`head`](Self::head) and
/// [`tags`](Self::tags) are available immediately and
/// [`read_packet`](Self::read_packet) yields audio from the first call.
///
/// Pages failing their CRC are an error rather than a silent skip: a truncated
/// or corrupt file should not decode as if it were fine.
///
/// ```no_run
/// use opus_pure::OggOpusReader;
///
/// let file = std::fs::File::open("in.opus")?;
/// let mut r = OggOpusReader::new(file)?;
/// println!("{} channels, pre-skip {}", r.head().channel_count, r.head().pre_skip);
/// while let Some(packet) = r.read_packet()? {
///     // decoder.decode(&packet.data, ...)
///     let _ = packet;
/// }
/// # Ok::<(), opus_pure::Error>(())
/// ```
///
/// [`head().decoder(rate)`](OpusHead::decoder) builds a decoder configured for
/// the stream, and [`Trim`](super::Trim) turns its output back into the audio
/// that was encoded. Decoding without that second step leaves the encoder delay
/// on the front and the final page's end-trim on the back.
///
/// # Reading forward
///
/// This reads forward from the first audio packet and does not seek. Playing a
/// stream again means starting over: take the source back with
/// [`into_inner`](Self::into_inner), rewind it, and construct a new reader,
/// which re-reads only the two header pages.
///
/// ```no_run
/// use std::io::Seek;
/// use opus_pure::OggOpusReader;
///
/// let mut reader = OggOpusReader::new(std::fs::File::open("in.opus")?)?;
/// // ... read to the end of the stream ...
/// let mut file = reader.into_inner();
/// file.rewind()?;
/// reader = OggOpusReader::new(file)?;   // back at the first packet
/// # Ok::<(), opus_pure::Error>(())
/// ```
///
/// A decoder carried across that boundary needs
/// [`reset_state`](crate::OpusDecoder::reset_state), and the
/// [`Trim`](super::Trim) needs replacing, or the second pass begins with the
/// first one's state and counts.
pub struct OggOpusReader<R: Read> {
    source: R,
    head: OpusHead,
    tags: OpusTags,
    serial: u32,

    /// Packets fully recovered from the last page read, oldest first.
    ready: std::collections::VecDeque<OggPacket>,
    /// Bytes of a packet that spilled past the end of a page.
    partial: Vec<u8>,
    /// The last page carried the end-of-stream flag.
    saw_eos: bool,
    /// The source returned EOF.
    exhausted: bool,
    /// Reusable payload buffer to avoid 64 KB heap allocation per page read.
    payload_buf: Vec<u8>,
}

/// Shows where the reader has got to in the stream.
///
/// Deliberately does not require `R: Debug`: the source is a file or a socket
/// far more often than it is something printable, and requiring it would leave
/// most readers with no `Debug` at all.
impl<R: Read> std::fmt::Debug for OggOpusReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OggOpusReader")
            .field("head", &self.head)
            .field("serial", &format_args!("{:#010x}", self.serial))
            .field("packets_ready", &self.ready.len())
            .field("saw_eos", &self.saw_eos)
            .field("exhausted", &self.exhausted)
            .finish_non_exhaustive()
    }
}

impl<R: Read> OggOpusReader<R> {
    /// Read the header packets and position the reader at the first audio
    /// packet.
    pub fn new(source: R) -> Result<Self> {
        let mut r = OggOpusReader {
            source,
            head: OpusHead::new(1, 0)?,
            tags: OpusTags::new(),
            serial: 0,
            ready: std::collections::VecDeque::new(),
            partial: Vec::new(),
            saw_eos: false,
            exhausted: false,
            payload_buf: Vec::new(),
        };

        let first = r
            .next_packet_raw()?
            .ok_or(Error::InvalidStream("stream ends before OpusHead"))?;
        r.head = OpusHead::parse(&first.data)?;

        let second = r
            .next_packet_raw()?
            .ok_or(Error::InvalidStream("stream ends before OpusTags"))?;
        r.tags = OpusTags::parse(&second.data)?;

        Ok(r)
    }

    /// The stream's identification header.
    pub fn head(&self) -> &OpusHead {
        &self.head
    }

    /// The stream's comment header.
    pub fn tags(&self) -> &OpusTags {
        &self.tags
    }

    /// Serial number of the logical bitstream being read.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// The next audio packet, or `None` at end of stream.
    pub fn read_packet(&mut self) -> Result<Option<OggPacket>> {
        self.next_packet_raw()
    }

    /// The remaining audio packets, as an iterator.
    ///
    /// The same packets [`read_packet`](Self::read_packet) yields, in a form
    /// that composes: `for`, `take_while`, `filter`, or
    /// `collect::<Result<Vec<_>>>()` to stop at the first error.
    ///
    /// ```
    /// # use opus_pure::{OggOpusReader, Result};
    /// # fn f(bytes: &[u8]) -> Result<()> {
    /// let mut reader = OggOpusReader::new(std::io::Cursor::new(bytes))?;
    /// for packet in reader.packets() {
    ///     let packet = packet?;
    ///     // ... decode packet.data
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The iterator ends at the first error as well as at end of stream, so a
    /// truncated file stops rather than looping.
    pub fn packets(&mut self) -> Packets<'_, R> {
        Packets {
            reader: self,
            done: false,
        }
    }

    /// The underlying reader, giving up the ability to read further packets.
    pub fn into_inner(self) -> R {
        self.source
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.source
    }

    fn next_packet_raw(&mut self) -> Result<Option<OggPacket>> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Ok(Some(p));
            }
            if self.saw_eos || self.exhausted {
                // A packet still in `partial` was cut off by the end of the
                // stream; report that rather than returning it as if complete.
                if !self.partial.is_empty() {
                    self.partial.clear();
                    return Err(Error::InvalidStream(
                        "stream ends in the middle of a packet",
                    ));
                }
                return Ok(None);
            }
            self.read_page()?;
        }
    }

    /// Read one page and split it into packets, appending to `ready`.
    fn read_page(&mut self) -> Result<()> {
        let Some(raw) = self.read_page_header()? else {
            self.exhausted = true;
            return Ok(());
        };
        let header = PageHeader::parse(&raw)?;

        if header.segment_count == 0 {
            return Err(Error::InvalidStream("page has an empty segment table"));
        }

        let mut segments_arr = [0u8; 255];
        let segments = &mut segments_arr[..header.segment_count as usize];
        read_exact(&mut self.source, segments)?;
        let payload_len: usize = segments.iter().map(|&s| s as usize).sum();
        debug_assert!(payload_len <= MAX_PAGE_PAYLOAD);
        self.payload_buf.resize(payload_len, 0);
        read_exact(&mut self.source, &mut self.payload_buf)?;

        if !verify_crc(&raw, segments, &self.payload_buf, header.crc) {
            return Err(Error::InvalidStream("page CRC mismatch"));
        }

        if header.is_bos() {
            self.serial = header.serial;
        } else if header.serial != self.serial {
            // Multiplexed or chained streams are out of scope: a second logical
            // stream would need its own decoder state and pre-skip.
            return Err(Error::InvalidStream(
                "stream contains more than one logical bitstream",
            ));
        }

        // A page that claims to continue a packet when none is pending — or that
        // starts fresh while one is pending — means pages were lost or reordered.
        if header.is_continued() == self.partial.is_empty() {
            self.partial.clear();
            return Err(Error::InvalidStream(
                "page continuation flag does not match the pending packet",
            ));
        }

        self.saw_eos = header.is_eos();

        let mut off = 0usize;
        for (i, &lace) in segments.iter().enumerate() {
            let lace_sz = lace as usize;
            if self.partial.len().saturating_add(lace_sz) > MAX_OGG_PACKET_BYTES {
                self.partial.clear();
                return Err(Error::InvalidStream("packet exceeds maximum allowed size"));
            }
            self.partial
                .extend_from_slice(&self.payload_buf[off..off + lace_sz]);
            off += lace_sz;

            if lace < 255 {
                // Terminating segment: the packet is complete.
                let data = std::mem::take(&mut self.partial);
                let last_segment = i + 1 == segments.len();
                if !data.is_empty() {
                    self.ready.push_back(OggPacket {
                        data,
                        page_granule: header.granule_position,
                        end_of_stream: self.saw_eos && last_segment,
                    });
                }
                // An empty packet is dropped: muxers emit one as the payload of
                // a bare EOS page, and it is not decodable audio.
            }
        }
        Ok(())
    }

    /// Find and read the next page header, resynchronising on the capture
    /// pattern if the stream does not sit on a page boundary.
    fn read_page_header(&mut self) -> Result<Option<[u8; HEADER_LEN]>> {
        let mut buf = [0u8; HEADER_LEN];
        match read_exact_or_eof(&mut self.source, &mut buf)? {
            0 => return Ok(None),
            n if n < HEADER_LEN => {
                return Err(Error::InvalidStream("stream ends inside a page header"));
            }
            _ => {}
        }
        if &buf[0..4] == CAPTURE_PATTERN {
            return Ok(Some(buf));
        }

        // Resync: slide a one-byte window until the capture pattern appears.
        // Bounded so a stream of garbage terminates instead of spinning.
        const RESYNC_LIMIT: usize = 1 << 20;
        for _ in 0..RESYNC_LIMIT {
            buf.copy_within(1..HEADER_LEN, 0);
            let mut b = [0u8; 1];
            if read_exact_or_eof(&mut self.source, &mut b)? == 0 {
                return Err(Error::InvalidStream("stream ends without a valid page"));
            }
            buf[HEADER_LEN - 1] = b[0];
            if &buf[0..4] == CAPTURE_PATTERN {
                return Ok(Some(buf));
            }
        }
        Err(Error::InvalidStream(
            "no Ogg page found while resynchronising",
        ))
    }
}

fn read_exact<R: Read>(source: &mut R, buf: &mut [u8]) -> Result<()> {
    if read_exact_or_eof(source, buf)? < buf.len() {
        return Err(Error::InvalidStream("stream ends inside a page"));
    }
    Ok(())
}

/// Fill `buf`, returning how many bytes were read; short only at EOF.
fn read_exact_or_eof<R: Read>(source: &mut R, buf: &mut [u8]) -> Result<usize> {
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

/// Iterator over an [`OggOpusReader`]'s remaining packets, from
/// [`OggOpusReader::packets`].
#[derive(Debug)]
pub struct Packets<'a, R: Read> {
    reader: &'a mut OggOpusReader<R>,
    /// Set once the stream has ended or errored, so a caller who keeps polling
    /// gets `None` rather than the same error for ever.
    done: bool,
}

impl<R: Read> Iterator for Packets<'_, R> {
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
