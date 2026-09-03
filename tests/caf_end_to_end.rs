//! Real Opus audio through the Core Audio Format container and back.
//!
//! `src/caf/tests.rs` covers the framing with synthetic payloads; this covers
//! the seam between the codec and the container — the gapless recipe written
//! for Ogg producing the same result through CAF, a recording from Apple's own
//! encoder decoding to the length it declares, and a stream crossing from one
//! container to the other with nothing lost.

mod common;
use common::*;
use opus_pure::{
    Application, CafOpusReader, CafOpusWriter, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OggOpusReader,
    OggOpusWriter, OpusEncoder, OpusHead, Trim,
};
use std::io::Cursor;

/// One second of mono, encoded by macOS's Core Audio at 16 kbit/s from a clip
/// that is not a whole number of frames long, so the file carries both a
/// priming count and a remainder. What it holds, from its own packet table:
/// 51 packets, priming 312, valid 48137, remainder 511.
const CORE_AUDIO_FILE: &[u8] = include_bytes!("fixtures/coreaudio-mono.caf");
const CORE_AUDIO_SAMPLES: u64 = 48_137;

/// Rate, channels, bitrate.
const CASES: &[(i32, usize, i32)] = &[
    (48_000, 1, 96_000),
    (48_000, 2, 128_000),
    (24_000, 2, 48_000),
    (16_000, 1, 24_000),
];

/// The Ogg gapless recipe from `tests/ogg_gapless.rs`, with the writer's type
/// changed and nothing else.
fn encode_gapless(rate: i32, channels: usize, bitrate: i32, pcm: &[f32]) -> Vec<u8> {
    let frame = (rate / 50) as usize;
    let mut encoder = OpusEncoder::new(rate, channels, Application::Audio).unwrap();
    encoder.bitrate_bps = bitrate;
    let head = OpusHead::for_encoder(&encoder, rate as u32);

    let ticks = 48_000 / rate as usize;
    let total = pcm.len() / channels;
    let frames = (total + (head.pre_skip as usize).div_ceil(ticks)).div_ceil(frame);
    let final_granule = u64::from(head.pre_skip) + (total * ticks) as u64;

    let mut w = CafOpusWriter::new(Cursor::new(Vec::new()), head).unwrap();
    let mut packet = vec![0u8; MAX_PACKET_BYTES];
    let per_frame = frame * channels;
    let mut block = vec![0.0f32; per_frame];

    for i in 0..frames {
        let start = (i * per_frame).min(pcm.len());
        let end = (start + per_frame).min(pcm.len());
        block[..end - start].copy_from_slice(&pcm[start..end]);
        block[end - start..].fill(0.0);

        let n = encoder.encode(&block, frame, &mut packet).unwrap();
        if i + 1 == frames {
            let duration = final_granule - w.granule() as u64;
            w.write_packet_with_duration(&packet[..n], duration as u32)
                .unwrap();
        } else {
            w.write_packet(&packet[..n]).unwrap();
        }
    }
    w.finish().unwrap().into_inner()
}

/// The Ogg decode recipe, with the reader's type changed.
fn decode_trimmed(file: &[u8], rate: i32) -> (OpusHead, Vec<f32>) {
    let mut reader = CafOpusReader::new(Cursor::new(file)).unwrap();
    let head = reader.head().clone();
    let channels = head.channel_count as usize;
    let mut decoder = head.decoder(rate).unwrap();
    let mut trim = Trim::new(&head, rate, channels).unwrap();
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut pcm = Vec::new();
    for packet in reader.packets() {
        let packet = packet.unwrap();
        let n = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .unwrap();
        pcm.extend_from_slice(trim.keep(&packet, &block[..n * channels]));
    }
    (head, pcm)
}

/// Every packet in a file, and its header.
fn caf_packets(file: &[u8]) -> (OpusHead, Vec<Vec<u8>>) {
    let mut r = CafOpusReader::new(Cursor::new(file)).unwrap();
    let head = r.head().clone();
    let packets = r.packets().map(|p| p.unwrap().data).collect();
    (head, packets)
}

/// The recipe that ends an Ogg file where the audio ends does the same for a
/// CAF: what comes back is the clip, to the sample.
#[test]
fn a_clip_comes_back_the_length_it_went_in() {
    for &(rate, channels, bitrate) in CASES {
        // Deliberately not a whole number of frames.
        let samples = rate as usize * 3 / 2 + 137;
        let mono = music_like(rate, samples);
        let src = if channels == 2 {
            interleave(&[mono.clone(), mono])
        } else {
            mono
        };

        let file = encode_gapless(rate, channels, bitrate, &src);
        let (head, decoded) = decode_trimmed(&file, rate);
        assert_eq!(head.channel_count as usize, channels);
        assert_eq!(
            decoded.len(),
            src.len(),
            "{rate} Hz {channels}ch: sample count changed"
        );

        let skip = (rate / 50) as usize * 10 * channels;
        let ch0 = deinterleave(&decoded[skip..], channels, 0);
        let src0 = deinterleave(&src[skip..], channels, 0);
        let (corr, _) = aligned_correlation(&ch0, &src0, (rate / 50) as usize);
        assert!(
            corr > 0.98,
            "{rate} Hz {channels}ch: correlation {corr:.4} after the round trip"
        );
    }
}

/// A file from Apple's encoder is read as the file says it should be: its
/// header, its packet count, and the audio length its table declares.
#[test]
fn a_core_audio_recording_decodes_to_its_declared_length() {
    let mut r = CafOpusReader::new(Cursor::new(CORE_AUDIO_FILE)).unwrap();
    assert_eq!(r.head().channel_count, 1);
    assert_eq!(r.head().pre_skip, 312);
    assert_eq!(r.head().input_sample_rate, 48_000);
    assert_eq!(r.packet_count(), 51);
    assert_eq!(r.audio_samples_48k(), CORE_AUDIO_SAMPLES);

    let mut decoder = r.head().decoder(48_000).unwrap();
    let mut trim = Trim::new(r.head(), 48_000, 1).unwrap();
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES];
    let mut pcm = Vec::new();
    for packet in r.packets() {
        let packet = packet.unwrap();
        let n = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .unwrap();
        pcm.extend_from_slice(trim.keep(&packet, &block[..n]));
    }
    assert_eq!(trim.samples_emitted(), CORE_AUDIO_SAMPLES);
    assert_eq!(pcm.len() as u64, CORE_AUDIO_SAMPLES);
    assert!(pcm.iter().all(|s| s.is_finite()));
    // The clip was music, not silence.
    assert!(energy(&pcm) > 1e-3, "decoded to near-silence");
}

/// The reason the module exists: an Apple recording becomes an `.opus` file
/// that everything else plays, without touching a sample, and the file it
/// produces says the same things about the audio.
#[test]
fn a_core_audio_recording_remuxes_to_ogg_and_back() {
    let (head, original) = caf_packets(CORE_AUDIO_FILE);

    // CAF → Ogg, as the module docs show it.
    let mut reader = CafOpusReader::new(Cursor::new(CORE_AUDIO_FILE)).unwrap();
    let mut ogg = OggOpusWriter::new(Vec::new(), reader.head().clone()).unwrap();
    let mut packets = reader.packets().peekable();
    while let Some(packet) = packets.next() {
        let packet = packet.unwrap();
        if packets.peek().is_none() {
            let duration = packet.page_granule - ogg.granule();
            ogg.write_packet_with_duration(&packet.data, duration as u32)
                .unwrap();
        } else {
            ogg.write_packet(&packet.data).unwrap();
        }
    }
    let ogg = ogg.finish().unwrap();

    let mut r = OggOpusReader::new(Cursor::new(&ogg)).unwrap();
    assert_eq!(r.head(), &head);
    let mut last_granule = -1;
    let mut from_ogg = Vec::new();
    for packet in r.packets() {
        let packet = packet.unwrap();
        last_granule = packet.page_granule;
        from_ogg.push(packet.data);
    }
    assert_eq!(from_ogg, original, "packets changed on the way to Ogg");
    assert_eq!(
        last_granule as u64,
        u64::from(head.pre_skip) + CORE_AUDIO_SAMPLES,
        "the Ogg file ends somewhere other than where the recording did"
    );

    // Ogg → CAF: the same loop with the types swapped.
    let mut reader = OggOpusReader::new(Cursor::new(&ogg)).unwrap();
    let mut caf = CafOpusWriter::new(Cursor::new(Vec::new()), reader.head().clone()).unwrap();
    let mut packets = reader.packets().peekable();
    while let Some(packet) = packets.next() {
        let packet = packet.unwrap();
        if packets.peek().is_none() {
            let duration = packet.page_granule - caf.granule();
            caf.write_packet_with_duration(&packet.data, duration as u32)
                .unwrap();
        } else {
            caf.write_packet(&packet.data).unwrap();
        }
    }
    let caf = caf.finish().unwrap().into_inner();

    let mut r = CafOpusReader::new(Cursor::new(&caf)).unwrap();
    assert_eq!(r.head(), &head);
    assert_eq!(r.audio_samples_48k(), CORE_AUDIO_SAMPLES);
    let back: Vec<_> = r.packets().map(|p| p.unwrap().data).collect();
    assert_eq!(back, original, "packets changed on the way back to CAF");
}
