# Changelog

Notable changes to this crate, newest first. Nothing is recorded here from before the first public release; what this crate changed relative to the fork it came from is described in [ATTRIBUTION.md](ATTRIBUTION.md).

## Unreleased

- **Core Audio Format (`.caf`) mux and demux.** Apple's audio frameworks record and play Opus only inside CAF, and little else reads one; `CafOpusReader` and `CafOpusWriter` present the Ogg pair's API over that container, so a recording moves between the two packet for packet, without decoding. The packet table's priming and remainder frames carry across as pre-skip and end-trim, so the gapless recipe works unchanged. Files written here decode through Apple's tools to the sample; a recording from Apple's encoder is in `tests/fixtures/` and read in CI. Mono and stereo.
- `Error::InvalidStream` now covers both containers, and its message no longer names Ogg.
- **Decoding and analysis no longer allocate per packet.** The range decoder borrows the packet instead of copying it, tonality analysis keeps its downmix scratch in the encoder state, the multistream decoder rebuilds each stream's packet into one reused buffer, and the Ogg reader reuses its page buffer.
- **The Ogg reader caps a reassembled packet at 16 MiB.** A hostile chain of continued pages could previously grow the reader's buffer without limit; it is now refused as an invalid stream.
- `RangeCoder::shrink` checks its size preconditions in release builds.

## 0.2.1 — 2026-08-31

- **`Trim::keep_range`** returns the same cut as `keep`, as indices into the decoded PCM, for a playback path that has to hold its position across buffer fills — a borrowed slice cannot, and the trimmed length alone does not say where the audio starts. The README shows the pattern.

## 0.2.0 — 2026-08-31

- **`Trim` applies RFC 7845's pre-skip and end-trim to a decoded stream.** The documented decode recipe took only the pre-skip, leaving up to a frame of padding past the end of the audio on any file that carries an end-trim — which every `opusenc` file does. README, crate docs and `examples/decode.rs` now use it.
- **The documented encode recipe writes a gapless file.** It flushes the encoder's delay and states the final granule with `write_packet_with_duration`, which existed but appeared in no example. Previously the last few milliseconds of a clip never left the encoder. `tests/ogg_gapless.rs` pins the round trip at every rate.
- `OpusHead::decoder` builds a decoder carrying the header's channel count and output gain, the one of the two that is silent when it is missed.
- `MAX_PACKET_SAMPLES` sizes a decode buffer, the companion to `MAX_PACKET_BYTES`.
- `OggOpusWriter::granule`, so stating an end-trim is a subtraction rather than a derivation from a frame count.
- `OggPacket::new`, so code that consumes packets can be tested without muxing a stream.
- `OpusMSDecoder::streams` and `streams_mut`, mirroring the encoder. Without them a surround stream's declared output gain could not be applied at all, which RFC 7845 §5.1 asks a player to do whatever the mapping family.
- The reader documents rewinding for playback loops. There is still no seek.

## 0.1.0 — 2026-08-26

First release.

- Opus encoder and decoder (RFC 6716) covering all three coding modes — SILK, CELT and the hybrid of both — at 8, 12, 16, 24 and 48 kHz, mono and stereo, at every one of the nine Opus frame sizes.
- Ogg mux and demux (RFC 7845), so the crate reads and writes `.opus` files rather than only raw packets: `OpusHead`, `OpusTags`, page CRCs and granule positions.
- Multistream encoding and decoding, a repacketizer, packet inspection without decoding, and parallel encoding.
- No C, no FFI, no `build.rs` and no dependencies. Requires Rust 1.88.
