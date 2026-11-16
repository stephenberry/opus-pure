//! Regression: the CELT prefilter must not signal a pitch period outside the
//! alphabet it codes it with.
//!
//! A deterministic 9-frame window of real 48 kHz stereo audio (encoded at
//! 128 kb/s, serial) drove the encoder's pitch search to `pitch_index = 1023`
//! after `remove_doubling`. Signalling adds one, so `octave = EC_ILOG(pi) - 5`
//! became 6 — but the octave is coded with `ec_enc_uint(_, 6)`, an alphabet of
//! six symbols (0..=5). The out-of-range encode desynchronised the range coder
//! from the decoder, and the rest of the packet decoded as garbage: the decoded
//! peak reached ~7500 full-scale, where libopus on the same input rings slightly
//! past the input peak (~1.3). libopus caps the period to `max_period - 2` after
//! the doubling search (`run_prefilter`, celt_encoder.c); the encoder must too.
//!
//! The data is raw little-endian f32 interleaved stereo, frames of 960 samples
//! per channel, taken from 0.18 s of MP3-decoded music (a loud passage -> deep
//! dip -> transient). Its input peak is 0.84; synthetic tones do not reach this
//! path — the trigger needs real pitch-search dynamics plus accumulated encoder
//! state.

use opus_pure::{Application, MAX_PACKET_BYTES, OpusDecoder, OpusEncoder};

const RATE: i32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_SIZE: usize = 960;

/// 9 frames x 960 samples x 2 channels, little-endian f32.
const SLICE: &[u8] = include_bytes!("data/prefilter_desync_slice.f32");

fn samples() -> Vec<f32> {
    SLICE
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[test]
fn hot_input_prefilter_frames_decode_in_range() {
    let input_peak = samples().iter().fold(0f32, |m, &v| m.max(v.abs()));
    assert!(
        (input_peak - 0.8387374).abs() < 1e-4,
        "test slice drifted from its recorded input peak: {input_peak}"
    );

    let mut encoder = OpusEncoder::new(RATE, CHANNELS, Application::Audio).unwrap();
    encoder.bitrate_bps = 128_000;
    let mut decoder = OpusDecoder::new(RATE, CHANNELS).unwrap();
    let mut packet = [0u8; MAX_PACKET_BYTES];
    let mut decoded = [0f32; FRAME_SIZE * CHANNELS * 2];

    let mut peak = 0f32;
    for frame in samples().chunks_exact(FRAME_SIZE * CHANNELS) {
        let n = encoder.encode(frame, FRAME_SIZE, &mut packet).unwrap();
        let m = decoder
            .decode(&packet[..n], FRAME_SIZE, &mut decoded)
            .unwrap();
        for v in &decoded[..m * CHANNELS] {
            peak = peak.max(v.abs());
        }
    }

    // With the desync the peak is ~7500; healthy output of this slice peaks at
    // ~1.3. The bound is a codec-sanity ceiling, not a fidelity target.
    assert!(
        peak < 4.0,
        "decoded peak {peak} far past full scale: encoder/decoder range coder desynced"
    );
}
