//! The CAF framing, with synthetic payloads.
//!
//! Real audio through the container is `tests/caf_end_to_end.rs`; this covers
//! the chunks, the packet table in each of its shapes, and what the reader
//! makes of files it did not write.

use std::io::Cursor;

use super::chunk::{DESC_FRAMES_PER_PACKET_OFFSET, chunk_type};
use super::{CafOpusReader, CafOpusWriter};
use crate::{Error, OggPacket, OpusHead};

/// A 20 ms CELT fullband packet's TOC byte (configuration 31): 960 samples
/// at 48 kHz.
const TOC_20MS: u8 = 0xfc;
/// A 10 ms one (configuration 30): 480 samples.
const TOC_10MS: u8 = 0xf0;

/// A packet of `len` bytes with a real TOC and a recognisable body.
fn packet(toc: u8, len: usize, fill: u8) -> Vec<u8> {
    let mut p = vec![fill; len];
    p[0] = toc;
    p
}

fn head(pre_skip: u16, channels: u8) -> OpusHead {
    let mut h = OpusHead::new(channels, 48_000).unwrap();
    h.pre_skip = pre_skip;
    h
}

/// Write `packets` to a file in memory. The last packet's duration is stated
/// as `last_duration` if given, which is how an end-trim is expressed.
fn write(head: OpusHead, packets: &[Vec<u8>], last_duration: Option<u32>) -> Vec<u8> {
    let mut w = CafOpusWriter::new(Cursor::new(Vec::new()), head).unwrap();
    for (i, p) in packets.iter().enumerate() {
        match last_duration {
            Some(d) if i + 1 == packets.len() => w.write_packet_with_duration(p, d).unwrap(),
            _ => w.write_packet(p).unwrap(),
        }
    }
    w.finish().unwrap().into_inner()
}

fn read_all(file: &[u8]) -> (OpusHead, Vec<OggPacket>) {
    let mut r = CafOpusReader::new(Cursor::new(file)).unwrap();
    let head = r.head().clone();
    let packets: Vec<_> = r.packets().collect::<crate::Result<_>>().unwrap();
    (head, packets)
}

/// The chunks of a file, as `(type, stated size, body)`, so a test can rearrange
/// or corrupt them and put the file back together.
fn chunks(file: &[u8]) -> Vec<([u8; 4], i64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 8;
    while i < file.len() {
        let kind: [u8; 4] = file[i..i + 4].try_into().unwrap();
        let size = i64::from_be_bytes(file[i + 4..i + 12].try_into().unwrap());
        let end = if size < 0 {
            file.len()
        } else {
            i + 12 + size as usize
        };
        out.push((kind, size, file[i + 12..end].to_vec()));
        i = end;
    }
    out
}

fn assemble(chunks: &[([u8; 4], i64, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"caff\0\x01\0\0".to_vec();
    for (kind, size, body) in chunks {
        out.extend_from_slice(kind);
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(body);
    }
    out
}

fn find(chunks: &[([u8; 4], i64, Vec<u8>)], kind: &[u8; 4]) -> usize {
    chunks.iter().position(|c| &c.0 == kind).unwrap()
}

fn is_invalid_stream(r: crate::Result<CafOpusReader<Cursor<Vec<u8>>>>) -> bool {
    matches!(r, Err(Error::InvalidStream(_)))
}

#[test]
fn packets_survive_the_round_trip_byte_for_byte() {
    let packets: Vec<_> = (0..20)
        .map(|i| packet(TOC_20MS, 3 + i * 37, i as u8))
        .collect();
    let file = write(head(312, 2), &packets, None);
    let (h, back) = read_all(&file);

    assert_eq!(h.channel_count, 2);
    assert_eq!(h.pre_skip, 312);
    assert_eq!(h.input_sample_rate, 48_000);
    assert_eq!(back.len(), packets.len());
    for (i, (a, b)) in back.iter().zip(&packets).enumerate() {
        assert_eq!(
            &a.data, b,
            "packet {i} changed passing through the container"
        );
    }
}

/// Each packet reports the sample count decodable through it, and only the
/// last carries the end-of-stream flag — the shape `Trim` consumes.
#[test]
fn granules_count_up_through_the_packets() {
    let packets = vec![packet(TOC_20MS, 10, 1); 5];
    let file = write(head(312, 1), &packets, None);
    let (_, back) = read_all(&file);
    for (i, p) in back.iter().enumerate() {
        assert_eq!(p.page_granule, 960 * (i as i64 + 1), "packet {i}");
        assert_eq!(p.end_of_stream, i == 4, "packet {i}");
    }
}

/// An end-trim goes in as a short stated duration and comes out as a short
/// final granule, exactly as it does through Ogg.
#[test]
fn the_end_trim_survives_as_a_short_final_granule() {
    let packets = vec![packet(TOC_20MS, 10, 1); 3];
    let file = write(head(312, 1), &packets, Some(500));
    let mut r = CafOpusReader::new(Cursor::new(&file)).unwrap();
    assert_eq!(r.audio_samples_48k(), 960 + 960 + 500 - 312);
    let back: Vec<_> = r.packets().map(|p| p.unwrap()).collect();
    assert_eq!(back[1].page_granule, 1920);
    assert_eq!(back[2].page_granule, 1920 + 500);

    // What the table says: priming + valid + remainder is what the packets
    // decode to.
    let c = chunks(&file);
    let pakt = &c[find(&c, chunk_type::PAKT)].2;
    let valid = i64::from_be_bytes(pakt[8..16].try_into().unwrap());
    let priming = u32::from_be_bytes(pakt[16..20].try_into().unwrap());
    let remainder = u32::from_be_bytes(pakt[20..24].try_into().unwrap());
    assert_eq!(priming, 312);
    assert_eq!(valid, 1920 + 500 - 312);
    assert_eq!(remainder, 960 - 500);
}

/// The writer's own layout: the chunk order Apple's decoder was checked
/// against, and the constant frame count Apple's files carry.
#[test]
fn the_written_file_has_the_expected_chunks() {
    let file = write(head(312, 1), &vec![packet(TOC_20MS, 10, 1); 4], None);
    let c = chunks(&file);
    let order: Vec<_> = c.iter().map(|c| c.0).collect();
    assert_eq!(
        order,
        [
            *chunk_type::DESC,
            *chunk_type::CHAN,
            *chunk_type::INFO,
            *chunk_type::DATA,
            *chunk_type::PAKT
        ]
    );

    let desc = &c[0].2;
    assert_eq!(f64::from_be_bytes(desc[0..8].try_into().unwrap()), 48_000.0);
    assert_eq!(&desc[8..12], b"opus");
    let frames_per_packet = u32::from_be_bytes(
        desc[DESC_FRAMES_PER_PACKET_OFFSET..DESC_FRAMES_PER_PACKET_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    assert_eq!(frames_per_packet, 960);

    // The data chunk's size was patched to what was written: an edit count and
    // four ten-byte packets.
    let (kind, size, body) = &c[find(&c, chunk_type::DATA)];
    assert_eq!(kind, chunk_type::DATA);
    assert_eq!(*size, 4 + 40);
    assert_eq!(body.len(), 44);

    // One byte per packet in the table.
    let pakt = &c[find(&c, chunk_type::PAKT)].2;
    assert_eq!(pakt.len(), 24 + 4);
    assert_eq!(&pakt[24..], &[10, 10, 10, 10]);
}

/// Packets of unequal duration cannot share one frame count, so the table
/// states one per packet, and the reader takes them from there.
#[test]
fn mixed_durations_use_a_count_per_packet() {
    let packets = vec![
        packet(TOC_20MS, 10, 1),
        packet(TOC_10MS, 200, 2),
        packet(TOC_20MS, 10, 3),
    ];
    let file = write(head(0, 1), &packets, None);
    let c = chunks(&file);
    let desc = &c[find(&c, chunk_type::DESC)].2;
    assert_eq!(
        &desc[DESC_FRAMES_PER_PACKET_OFFSET..DESC_FRAMES_PER_PACKET_OFFSET + 4],
        &[0; 4]
    );
    let pakt = &c[find(&c, chunk_type::PAKT)].2;
    // (10, 960) (200, 480) (10, 960), with 960 and 200 as two-byte varints.
    assert_eq!(
        &pakt[24..],
        &[10, 0x87, 0x40, 0x81, 0x48, 0x83, 0x60, 10, 0x87, 0x40]
    );

    let (_, back) = read_all(&file);
    let granules: Vec<_> = back.iter().map(|p| p.page_granule).collect();
    assert_eq!(granules, [960, 1440, 2400]);
}

/// Apple's recorder puts the table ahead of the audio and lets the data chunk
/// run to the end of the file. Both are handled, separately and together.
#[test]
fn the_table_may_come_before_the_data() {
    let packets = vec![packet(TOC_20MS, 10, 1), packet(TOC_20MS, 20, 2)];
    let file = write(head(312, 1), &packets, None);
    let mut c = chunks(&file);
    let pakt = c.remove(find(&c, chunk_type::PAKT));
    let data_at = find(&c, chunk_type::DATA);
    c.insert(data_at, pakt);

    let apple_order = assemble(&c);
    let (_, back) = read_all(&apple_order);
    assert_eq!(back.len(), 2);
    assert_eq!(back[1].data, packets[1]);

    // And with the data chunk's size left open.
    let data_at = find(&c, chunk_type::DATA);
    c[data_at].1 = -1;
    let open_ended = assemble(&c);
    let (_, back) = read_all(&open_ended);
    assert_eq!(back.len(), 2);
    assert_eq!(back[1].data, packets[1]);
}

/// Chunks the reader has no use for — Apple writes `kuki` and `free` — are
/// stepped over.
#[test]
fn unknown_chunks_are_skipped() {
    let file = write(head(312, 1), &[packet(TOC_20MS, 10, 1)], None);
    let mut c = chunks(&file);
    c.insert(1, (*b"kuki", 28, vec![0xaa; 28]));
    c.insert(3, (*b"free", 100, vec![0; 100]));
    c.push((*b"uuid", 3, vec![1, 2, 3]));
    let (_, back) = read_all(&assemble(&c));
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].data, packet(TOC_20MS, 10, 1));
}

/// FFmpeg's layout when it does not know the frame size: zero frames per
/// packet in `desc` and one integer per packet in the table, where the
/// specification wants two. The packets say how long they are.
#[test]
fn a_table_with_no_frame_count_reads_durations_from_the_packets() {
    let packets = vec![packet(TOC_20MS, 15, 1), packet(TOC_10MS, 15, 2)];
    let file = write(head(0, 1), &packets, None);
    let mut c = chunks(&file);
    let desc = find(&c, chunk_type::DESC);
    c[desc].2[DESC_FRAMES_PER_PACKET_OFFSET..DESC_FRAMES_PER_PACKET_OFFSET + 4].fill(0);
    let pakt = find(&c, chunk_type::PAKT);
    c[pakt].2.truncate(24);
    c[pakt].2.extend_from_slice(&[15, 15]);
    c[pakt].1 = 26;
    // Valid frames as FFmpeg computes them, so the last granule is honest.
    c[pakt].2[8..16].copy_from_slice(&1440i64.to_be_bytes());

    let (_, back) = read_all(&assemble(&c));
    assert_eq!(back[0].page_granule, 960);
    assert_eq!(back[1].page_granule, 1440);
}

/// A file declaring one of Opus's lower rates counts its frames at that rate;
/// what comes out is still at 48 kHz.
#[test]
fn a_lower_desc_rate_is_scaled_to_48k() {
    let file = write(head(312, 1), &vec![packet(TOC_20MS, 10, 1); 2], None);
    let mut c = chunks(&file);
    let desc = find(&c, chunk_type::DESC);
    c[desc].2[0..8].copy_from_slice(&16_000f64.to_be_bytes());
    c[desc].2[DESC_FRAMES_PER_PACKET_OFFSET..DESC_FRAMES_PER_PACKET_OFFSET + 4]
        .copy_from_slice(&320u32.to_be_bytes());
    let pakt = find(&c, chunk_type::PAKT);
    c[pakt].2[8..16].copy_from_slice(&((640 - 104) as i64).to_be_bytes()); // valid
    c[pakt].2[16..20].copy_from_slice(&104u32.to_be_bytes()); // priming: 312 / 3

    let (h, back) = read_all(&assemble(&c));
    assert_eq!(h.pre_skip, 312);
    assert_eq!(h.input_sample_rate, 16_000);
    assert_eq!(back[0].page_granule, 960);
    assert_eq!(back[1].page_granule, 1920);
}

#[test]
fn a_file_with_no_packets_round_trips() {
    let file = write(head(312, 1), &[], None);
    let mut r = CafOpusReader::new(Cursor::new(&file)).unwrap();
    assert_eq!(r.packet_count(), 0);
    assert_eq!(r.audio_samples_48k(), 0);
    assert!(r.read_packet().unwrap().is_none());
}

/// The writer measures its patch positions from where the sink already is, so
/// a CAF written after something else lands intact.
#[test]
fn the_writer_starts_where_the_sink_is() {
    let mut sink = Cursor::new(b"twelve bytes".to_vec());
    sink.set_position(12);
    let mut w = CafOpusWriter::new(sink, head(312, 1)).unwrap();
    w.write_packet(&packet(TOC_20MS, 10, 1)).unwrap();
    let file = w.finish().unwrap().into_inner();
    assert_eq!(&file[..12], b"twelve bytes");
    let (_, back) = read_all(&file[12..]);
    assert_eq!(back.len(), 1);
}

#[test]
fn dropping_the_writer_finishes_the_file() {
    let mut sink = Cursor::new(Vec::new());
    {
        let mut w = CafOpusWriter::new(&mut sink, head(312, 1)).unwrap();
        w.write_packet(&packet(TOC_20MS, 10, 1)).unwrap();
    }
    let (_, back) = read_all(sink.get_ref());
    assert_eq!(back.len(), 1);
}

#[test]
fn the_writer_refuses_what_the_container_cannot_carry() {
    let sink = || Cursor::new(Vec::new());
    let mut surround = head(312, 2);
    surround.channel_count = 3;
    assert!(matches!(
        CafOpusWriter::new(sink(), surround),
        Err(Error::InvalidArgument(_))
    ));
    let mut gain = head(312, 2);
    gain.output_gain_q8 = 256;
    assert!(matches!(
        CafOpusWriter::new(sink(), gain),
        Err(Error::InvalidArgument(_))
    ));

    let mut w = CafOpusWriter::new(sink(), head(312, 1)).unwrap();
    assert!(matches!(w.write_packet(&[]), Err(Error::InvalidPacket(_))));
    assert!(matches!(
        w.write_packet_with_duration(&[], 960),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        w.write_packet_with_duration(&packet(TOC_20MS, 10, 1), 5761),
        Err(Error::InvalidArgument(_))
    ));
    // A packet without a readable duration has no place in the table.
    assert!(matches!(
        w.write_packet_with_duration(&[0xff, 0x30, 0, 0], 960),
        Err(Error::InvalidPacket(_))
    ));
}

/// A file cannot promise more audio than its packets decode to.
#[test]
fn stating_more_than_the_packets_carry_fails_at_finish() {
    let mut w = CafOpusWriter::new(Cursor::new(Vec::new()), head(0, 1)).unwrap();
    w.write_packet_with_duration(&packet(TOC_10MS, 10, 1), 960)
        .unwrap();
    assert!(matches!(w.finish(), Err(Error::InvalidArgument(_))));
}

#[test]
fn a_stream_shorter_than_its_pre_skip_has_no_audio() {
    let file = write(head(3000, 1), &[packet(TOC_20MS, 10, 1)], None);
    let r = CafOpusReader::new(Cursor::new(&file)).unwrap();
    assert_eq!(r.audio_samples_48k(), 0);
    let c = chunks(&file);
    let pakt = &c[find(&c, chunk_type::PAKT)].2;
    assert_eq!(i64::from_be_bytes(pakt[8..16].try_into().unwrap()), 0);
}

#[test]
fn files_that_are_not_caf_opus_are_rejected() {
    let file = write(head(312, 1), &[packet(TOC_20MS, 10, 1)], None);
    let open = |f: Vec<u8>| CafOpusReader::new(Cursor::new(f));

    assert!(is_invalid_stream(open(Vec::new())));
    assert!(is_invalid_stream(open(b"caff\0\x01".to_vec())));

    let mut wrong_magic = file.clone();
    wrong_magic[..4].copy_from_slice(b"RIFF");
    assert!(is_invalid_stream(open(wrong_magic)));

    let mut wrong_version = file.clone();
    wrong_version[5] = 2;
    assert!(is_invalid_stream(open(wrong_version)));

    let mut c = chunks(&file);
    let desc = find(&c, chunk_type::DESC);
    c[desc].2[8..12].copy_from_slice(b"aac ");
    assert!(is_invalid_stream(open(assemble(&c))));

    let mut c = chunks(&file);
    c[desc].2[24..28].copy_from_slice(&3u32.to_be_bytes());
    assert!(is_invalid_stream(open(assemble(&c))));

    let mut c = chunks(&file);
    c[desc].2[0..8].copy_from_slice(&44_100f64.to_be_bytes());
    assert!(is_invalid_stream(open(assemble(&c))));

    for kind in [chunk_type::DESC, chunk_type::PAKT, chunk_type::DATA] {
        let mut c = chunks(&file);
        c.remove(find(&c, kind));
        assert!(is_invalid_stream(open(assemble(&c))), "missing {kind:?}");
    }
}

#[test]
fn a_table_that_disagrees_with_the_data_is_rejected() {
    let file = write(
        head(312, 1),
        &[packet(TOC_20MS, 10, 1), packet(TOC_20MS, 20, 2)],
        None,
    );
    let open = |c: &[([u8; 4], i64, Vec<u8>)]| CafOpusReader::new(Cursor::new(assemble(c)));
    let base = chunks(&file);
    let pakt = find(&base, chunk_type::PAKT);

    // Sizes that add up to less than the data, and to more.
    let mut c = base.clone();
    c[pakt].2[24] = 9;
    assert!(is_invalid_stream(open(&c)));
    c[pakt].2[24] = 11;
    assert!(is_invalid_stream(open(&c)));

    // A zero-length packet.
    let mut c = base.clone();
    c[pakt].2[24] = 0;
    c[pakt].2[25] = 30;
    assert!(is_invalid_stream(open(&c)));

    // More packets than the table has entries for.
    let mut c = base.clone();
    c[pakt].2[0..8].copy_from_slice(&3i64.to_be_bytes());
    assert!(is_invalid_stream(open(&c)));
    c[pakt].2[0..8].copy_from_slice(&i64::MAX.to_be_bytes());
    assert!(is_invalid_stream(open(&c)));

    // Negative counts.
    let mut c = base.clone();
    c[pakt].2[16..20].copy_from_slice(&(-1i32).to_be_bytes());
    assert!(is_invalid_stream(open(&c)));

    // A pre-skip Opus cannot state.
    let mut c = base.clone();
    c[pakt].2[16..20].copy_from_slice(&70_000u32.to_be_bytes());
    assert!(is_invalid_stream(open(&c)));

    // A frame count above 120 ms.
    let mut c = base.clone();
    let desc = find(&c, chunk_type::DESC);
    c[desc].2[DESC_FRAMES_PER_PACKET_OFFSET..DESC_FRAMES_PER_PACKET_OFFSET + 4]
        .copy_from_slice(&5761u32.to_be_bytes());
    assert!(is_invalid_stream(open(&c)));

    // A table that is not whole.
    let mut c = base.clone();
    c[pakt].2.truncate(20);
    c[pakt].1 = 20;
    assert!(is_invalid_stream(open(&c)));

    // A chunk with a negative size that is not -1, and a non-data chunk that
    // claims to run to the end.
    let mut c = base.clone();
    c[pakt].1 = -2;
    assert!(is_invalid_stream(open(&c)));
    let mut c = base.clone();
    c.insert(1, (*b"free", -1, vec![0; 8]));
    assert!(is_invalid_stream(open(&c)));
}

/// A file cut short is an error where it is cut: in the headers it cannot be
/// opened, and in the audio the packet that runs off the end is reported.
#[test]
fn a_truncated_file_is_reported_where_it_ends() {
    let file = write(
        head(312, 1),
        &[packet(TOC_20MS, 10, 1), packet(TOC_20MS, 20, 2)],
        None,
    );
    let c = chunks(&file);
    let data = find(&c, chunk_type::DATA);
    let apple: Vec<_> = {
        // Table first, so the audio is what gets cut.
        let mut c = c.clone();
        let pakt = c.remove(find(&c, chunk_type::PAKT));
        c.insert(data, pakt);
        c
    };
    let whole = assemble(&apple);
    let mut r = CafOpusReader::new(Cursor::new(whole[..whole.len() - 5].to_vec())).unwrap();
    assert!(r.read_packet().unwrap().is_some());
    assert!(r.read_packet().is_err());

    // Cut inside the desc chunk.
    assert!(is_invalid_stream(CafOpusReader::new(Cursor::new(
        file[..30].to_vec()
    ))));
    // Cut inside a chunk header.
    assert!(is_invalid_stream(CafOpusReader::new(Cursor::new(
        file[..14].to_vec()
    ))));
}

#[test]
fn the_iterator_stops_after_an_error() {
    let file = write(head(312, 1), &vec![packet(TOC_20MS, 10, 1); 3], None);
    let mut c = chunks(&file);
    let pakt = c.remove(find(&c, chunk_type::PAKT));
    let data = find(&c, chunk_type::DATA);
    c.insert(data, pakt);
    let whole = assemble(&c);
    let mut r = CafOpusReader::new(Cursor::new(whole[..whole.len() - 3].to_vec())).unwrap();
    let results: Vec<_> = r.packets().collect();
    assert_eq!(results.len(), 3);
    assert!(results[0].is_ok() && results[1].is_ok() && results[2].is_err());
    assert!(r.packets().next().is_none());
}
