//! Container-level round-trip and robustness tests.
//!
//! These exercise the framing on its own — synthetic packet payloads, no codec —
//! so a failure points at the muxer/demuxer rather than at the encoder.

use super::page::{CAPTURE_PATTERN, HEADER_LEN};
use super::*;
use crate::Error;

/// Deterministic pseudo-random bytes; `Math.random`-free so failures reproduce.
fn pseudo_packet(seed: u32, len: usize) -> Vec<u8> {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

fn mux(head: OpusHead, tags: OpusTags, packets: &[(Vec<u8>, u32)]) -> Vec<u8> {
    let mut w = OggOpusWriter::with_tags(Vec::new(), head, tags).unwrap();
    for (p, dur) in packets {
        w.write_packet_with_duration(p, *dur).unwrap();
    }
    w.finish().unwrap()
}

fn demux(bytes: &[u8]) -> (OpusHead, OpusTags, Vec<OggPacket>) {
    let mut r = OggOpusReader::new(std::io::Cursor::new(bytes)).unwrap();
    let head = r.head().clone();
    let tags = r.tags().clone();
    let mut out = Vec::new();
    while let Some(p) = r.read_packet().unwrap() {
        out.push(p);
    }
    (head, tags, out)
}

#[test]
fn round_trips_a_simple_stream() {
    let head = OpusHead::new(2, 48_000).unwrap();
    let mut tags = OpusTags::new();
    tags.push("TITLE", "test").unwrap();

    let packets: Vec<(Vec<u8>, u32)> = (0..50)
        .map(|i| (pseudo_packet(i, 40 + i as usize), 960))
        .collect();

    let bytes = mux(head.clone(), tags.clone(), &packets);
    let (got_head, got_tags, got) = demux(&bytes);

    assert_eq!(got_head, head);
    assert_eq!(got_tags, tags);
    assert_eq!(got.len(), packets.len());
    for (g, (p, _)) in got.iter().zip(&packets) {
        assert_eq!(&g.data, p);
    }
    assert!(got.last().unwrap().end_of_stream);
}

/// A page's granule position counts the samples a decoder can produce from the
/// packets completed on it. The pre-skip samples are the first of those, so
/// they must be counted once, by the packets that carry them — never added on
/// top. A granule past the end of the audio makes players report a duration
/// they cannot deliver.
#[test]
fn granule_counts_decodable_samples_not_pre_skip_plus_them() {
    let head = OpusHead::new(1, 48_000).unwrap();
    let packets: Vec<(Vec<u8>, u32)> = (0..10).map(|i| (pseudo_packet(i, 60), 960)).collect();

    let bytes = mux(head, OpusTags::new(), &packets);
    let (_, _, got) = demux(&bytes);

    // Everything fits one page here, so all packets report the final granule.
    let expected = 960 * 10;
    assert_eq!(got.last().unwrap().page_granule, expected);
    assert!(got.iter().all(|p| p.page_granule == expected));
}

#[test]
fn granule_advances_across_multiple_pages() {
    let head = OpusHead::new(1, 48_000).unwrap();
    // 1 KiB packets against the 4 KiB default target: several pages.
    let packets: Vec<(Vec<u8>, u32)> = (0..40).map(|i| (pseudo_packet(i, 1024), 960)).collect();

    let bytes = mux(head, OpusTags::new(), &packets);
    let (_, _, got) = demux(&bytes);

    assert_eq!(got.len(), 40);
    let granules: Vec<i64> = got.iter().map(|p| p.page_granule).collect();
    assert!(
        granules.windows(2).all(|w| w[1] >= w[0]),
        "granule must not go backwards"
    );
    assert_eq!(*granules.last().unwrap(), 960 * 40);
    assert!(
        granules.first() < granules.last(),
        "expected more than one page"
    );
}

/// A packet longer than one page's 255 segments must span pages, with the
/// continuation flag set — the case a naive one-page-per-packet muxer gets wrong.
#[test]
fn packets_spanning_pages_reassemble() {
    let head = OpusHead::new(2, 48_000).unwrap();
    // 70000 > 255*255 = 65025, so this needs more than one page on its own.
    let big = pseudo_packet(7, 70_000);
    let packets = vec![
        (pseudo_packet(1, 100), 960),
        (big.clone(), 960),
        (pseudo_packet(2, 100), 960),
    ];

    let bytes = mux(head, OpusTags::new(), &packets);
    let (_, _, got) = demux(&bytes);

    assert_eq!(got.len(), 3);
    assert_eq!(got[1].data, big);
    assert_eq!(got[0].data.len(), 100);
    assert_eq!(got[2].data.len(), 100);
}

/// A packet whose length is an exact multiple of 255 needs a trailing
/// zero-length lacing value, or the demuxer runs it into the next packet.
#[test]
fn packet_lengths_that_are_multiples_of_255_round_trip() {
    let head = OpusHead::new(1, 48_000).unwrap();
    let packets: Vec<(Vec<u8>, u32)> = [255usize, 510, 765, 1275]
        .iter()
        .enumerate()
        .map(|(i, &n)| (pseudo_packet(i as u32, n), 960))
        .collect();

    let bytes = mux(head, OpusTags::new(), &packets);
    let (_, _, got) = demux(&bytes);

    assert_eq!(got.len(), packets.len());
    for (g, (p, _)) in got.iter().zip(&packets) {
        assert_eq!(g.data.len(), p.len());
        assert_eq!(&g.data, p);
    }
}

#[test]
fn header_pages_are_separate_and_flagged() {
    let bytes = mux(
        OpusHead::new(2, 48_000).unwrap(),
        OpusTags::new(),
        &[(vec![0xfc; 8], 960)],
    );

    // First page: BOS, one packet, OpusHead.
    assert_eq!(&bytes[0..4], CAPTURE_PATTERN);
    assert_eq!(bytes[5] & 0x02, 0x02, "first page must set BOS");
    let seg_count = bytes[26] as usize;
    assert_eq!(seg_count, 1, "OpusHead must sit alone on its page");
    let head_len = bytes[HEADER_LEN] as usize;
    let head_start = HEADER_LEN + seg_count;
    assert_eq!(&bytes[head_start..head_start + 8], b"OpusHead");

    // Second page starts right after and carries OpusTags.
    let second = head_start + head_len;
    assert_eq!(&bytes[second..second + 4], CAPTURE_PATTERN);
    assert_eq!(bytes[second + 5] & 0x02, 0, "only the first page sets BOS");
    let tags_start = second + HEADER_LEN + bytes[second + 26] as usize;
    assert_eq!(&bytes[tags_start..tags_start + 8], b"OpusTags");
}

#[test]
fn last_page_is_flagged_end_of_stream() {
    let bytes = mux(
        OpusHead::new(1, 48_000).unwrap(),
        OpusTags::new(),
        &[(vec![1, 2, 3], 960)],
    );
    // Walk the pages; only the final one may carry EOS.
    let mut off = 0usize;
    let mut eos_pages = 0;
    let mut last_off = 0;
    while off + HEADER_LEN <= bytes.len() {
        assert_eq!(&bytes[off..off + 4], CAPTURE_PATTERN);
        let segs = bytes[off + 26] as usize;
        let payload: usize = bytes[off + HEADER_LEN..off + HEADER_LEN + segs]
            .iter()
            .map(|&b| b as usize)
            .sum();
        if bytes[off + 5] & 0x04 != 0 {
            eos_pages += 1;
            last_off = off;
        }
        off += HEADER_LEN + segs + payload;
    }
    assert_eq!(off, bytes.len(), "pages must tile the file exactly");
    assert_eq!(eos_pages, 1);
    assert_eq!(
        last_off + HEADER_LEN + bytes[last_off + 26] as usize + 3,
        bytes.len()
    );
}

#[test]
fn a_corrupt_page_is_an_error_not_silent_truncation() {
    let head = OpusHead::new(1, 48_000).unwrap();
    let packets: Vec<(Vec<u8>, u32)> = (0..30).map(|i| (pseudo_packet(i, 300), 960)).collect();
    let mut bytes = mux(head, OpusTags::new(), &packets);

    // Flip a byte deep in the audio payload, past both header pages.
    let at = bytes.len() * 3 / 4;
    bytes[at] ^= 0xff;

    let mut r = OggOpusReader::new(std::io::Cursor::new(&bytes)).unwrap();
    let mut err = None;
    loop {
        match r.read_packet() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(
        matches!(err, Some(Error::InvalidStream(_))),
        "corrupt payload must surface as an error, got {err:?}"
    );
}

/// The cap on a reassembled packet is what stops a chain of continued pages
/// from growing `partial` without limit. It sits far above any real packet, so
/// exactly the cap still reads back and one byte more is refused.
#[test]
fn a_packet_past_the_size_cap_is_refused() {
    use super::reader::MAX_OGG_PACKET_BYTES;
    let head = OpusHead::new(1, 48_000).unwrap();
    for (len, accepted) in [
        (MAX_OGG_PACKET_BYTES, true),
        (MAX_OGG_PACKET_BYTES + 1, false),
    ] {
        let pkt = pseudo_packet(7, len);
        let bytes = mux(head.clone(), OpusTags::new(), &[(pkt.clone(), 960)]);
        let mut r = OggOpusReader::new(std::io::Cursor::new(&bytes)).unwrap();
        let got = r.read_packet();
        if accepted {
            assert_eq!(got.unwrap().unwrap().data, pkt);
        } else {
            assert!(matches!(got, Err(Error::InvalidStream(_))), "{got:?}");
        }
    }
}

#[test]
fn truncated_stream_is_rejected() {
    let head = OpusHead::new(1, 48_000).unwrap();
    let packets: Vec<(Vec<u8>, u32)> = (0..20).map(|i| (pseudo_packet(i, 500), 960)).collect();
    let bytes = mux(head, OpusTags::new(), &packets);

    let cut = &bytes[..bytes.len() - 200];
    let mut r = OggOpusReader::new(std::io::Cursor::new(cut)).unwrap();
    let mut saw_err = false;
    loop {
        match r.read_packet() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                saw_err = true;
                break;
            }
        }
    }
    assert!(
        saw_err,
        "a truncated final page must not read as a clean end of stream"
    );
}

#[test]
fn rejects_a_stream_that_is_not_ogg() {
    let err = OggOpusReader::new(std::io::Cursor::new(b"this is not an ogg file".as_slice()));
    assert!(matches!(err, Err(Error::InvalidStream(_))));
}

#[test]
fn rejects_ogg_that_is_not_opus() {
    // A well-formed page whose first packet is not OpusHead.
    let mut page = Vec::new();
    super::page::write_page(0x02, 0, 1, 0, &[8], b"NotOpus!", &mut page);
    assert!(matches!(
        OggOpusReader::new(std::io::Cursor::new(page)),
        Err(Error::InvalidStream(_))
    ));
}

#[test]
fn writer_rejects_impossible_packets() {
    let mut w = OggOpusWriter::new(Vec::new(), OpusHead::new(1, 48_000).unwrap()).unwrap();
    assert!(matches!(
        w.write_packet_with_duration(&[], 960),
        Err(Error::InvalidArgument(_))
    ));
    // 120 ms is the ceiling for a single Opus packet.
    assert!(w.write_packet_with_duration(&[0xfc], 5760).is_ok());
    assert!(matches!(
        w.write_packet_with_duration(&[0xfc], 5761),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn output_is_reproducible() {
    let packets: Vec<(Vec<u8>, u32)> = (0..20).map(|i| (pseudo_packet(i, 200), 960)).collect();
    let a = mux(OpusHead::new(2, 48_000).unwrap(), OpusTags::new(), &packets);
    let b = mux(OpusHead::new(2, 48_000).unwrap(), OpusTags::new(), &packets);
    assert_eq!(a, b, "same input must produce a byte-identical file");
}

#[test]
fn distinct_headers_get_distinct_serials() {
    let mono = mux(
        OpusHead::new(1, 48_000).unwrap(),
        OpusTags::new(),
        &[(vec![0xfc], 960)],
    );
    let stereo = mux(
        OpusHead::new(2, 48_000).unwrap(),
        OpusTags::new(),
        &[(vec![0xfc], 960)],
    );
    assert_ne!(mono[14..18], stereo[14..18]);
}

#[test]
fn explicit_serial_is_honoured() {
    let w = OggOpusWriter::with_serial(
        Vec::new(),
        OpusHead::new(1, 48_000).unwrap(),
        OpusTags::new(),
        0xabcd_1234,
    )
    .unwrap();
    let bytes = w.finish().unwrap();
    assert_eq!(
        u32::from_le_bytes(bytes[14..18].try_into().unwrap()),
        0xabcd_1234
    );
    let r = OggOpusReader::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(r.serial(), 0xabcd_1234);
}

#[test]
fn page_target_controls_page_count() {
    let packets: Vec<(Vec<u8>, u32)> = (0..60).map(|i| (pseudo_packet(i, 200), 960)).collect();

    let count_pages = |target: usize| {
        let mut w = OggOpusWriter::new(Vec::new(), OpusHead::new(1, 48_000).unwrap()).unwrap();
        w.set_page_target(target);
        for (p, d) in &packets {
            w.write_packet_with_duration(p, *d).unwrap();
        }
        let bytes = w.finish().unwrap();
        bytes.windows(4).filter(|c| *c == CAPTURE_PATTERN).count()
    };

    assert!(count_pages(1000) > count_pages(60_000));
}

/// A stream with headers but no audio is still structurally valid.
#[test]
fn empty_stream_round_trips() {
    let bytes = mux(OpusHead::new(1, 48_000).unwrap(), OpusTags::new(), &[]);
    let (_, _, got) = demux(&bytes);
    assert!(got.is_empty());
}

/// The reader must not build an unbounded packet out of a page that never
/// terminates it.
#[test]
fn resyncs_past_leading_garbage() {
    let head = OpusHead::new(1, 48_000).unwrap();
    let good = mux(head, OpusTags::new(), &[(vec![0xfc, 1, 2, 3], 960)]);
    let mut bytes = vec![0x00u8; 64];
    bytes.extend_from_slice(&good);

    let (_, _, got) = demux(&bytes);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].data, vec![0xfc, 1, 2, 3]);
}
