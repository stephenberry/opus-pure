# Fuzzing: what happens when the bytes are not ours

Every other directory in this repository asks whether the codec is *right*. This one asks what it does when it is lied to. A decoder's whole job is to read bytes somebody else wrote, and this crate carries hand-written SIMD, hand-transcribed fixed-point arithmetic and a container parser, so "it does not crash" is a claim that has to be tested rather than assumed.

## Run

```sh
cargo test --test fuzz_corpus -- --ignored write_fuzz_seeds   # once, writes corpus/
cargo +nightly fuzz run decode_stream -j 6
cargo +nightly fuzz run ogg_read -j 6
cargo +nightly fuzz run caf_read -j 6
cargo +nightly fuzz run packet_shape -j 6
```

Nightly is `cargo-fuzz`'s requirement, not the library's: `-Z sanitizer=address` is nightly-only. Nothing in `opus-pure` needs it.

| target | input | what it reaches |
| --- | --- | --- |
| `decode_stream` | a configuration byte, then length-prefixed packets | the decoder, across a whole stream: LTP and LPC history, overlap-add, the resampler, mode switches, concealment, in-band FEC |
| `ogg_read` | an Ogg Opus file | page headers, the segment table, lacing, `OpusHead`, `OpusTags`, and then the decoder on whatever comes out |
| `caf_read` | a Core Audio Format file | the chunk walk, the `desc` chunk, the packet table in each of its shapes, and then the decoder on whatever comes out |
| `packet_shape` | one packet | `packet::{samples, frame_count, channels, mode, bandwidth}`, `repacketizer::{Repacketizer, pad_packet, unpad_packet}`, and the self-delimited framing by way of `OpusMSDecoder` |

## The bodies are not here

They are in [`tests/common/fuzz.rs`](../tests/common/fuzz.rs), and the files in `fuzz_targets/` are two lines each. A body that only the fuzzer can run is a body no ordinary test can replay, and a crash found on one developer's laptop has to keep being checked everywhere else. [`tests/fuzz_corpus.rs`](../tests/fuzz_corpus.rs) runs the same bodies over the tracked corpus on every `cargo test`, on every platform CI covers.

It is the same reason `reference/rust` includes `tests/common/` rather than copying it: this repository has already been bitten by a harness carrying its own stale copy of shared code.

## Three things that make the targets worth running

**One decoder for the whole input.** A harness that builds a fresh decoder per packet only ever tests a decoder that has just been reset, and nearly everything interesting in this codec is carried *between* packets. The defects already found in that machinery — a mode switch with no cross-fade, a stereo-to-mono path resuming from stale history — were reachable only from a decoder with a past. So `decode_stream` frames its input as a sequence, with zero-length packets standing for losses and a flag bit selecting the FEC entry point.

**Page checksums are resealed.** An Ogg page carries a CRC and the reader rejects a page whose checksum does not match. That is correct, and it is also a wall the fuzzer cannot climb: any mutation of a valid file breaks the checksum, so every input that would have been interesting is turned away at the check rather than at what it says. It is not a wall for an attacker, who computes the checksum like anyone else. `reseal_pages` recomputes it, which put `ogg_read` from 6,223 coverage points after ten minutes to 6,970 after twenty-seven seconds. Rejecting a bad checksum is still covered, by `ogg_end_to_end.rs::corrupted_container_is_reported`.

**The assertions are not only "did not crash".** Each body checks the contracts a caller is entitled to rely on: that a decode reports no more samples than the buffer it was given, that accepted audio is finite, that a frame the repacketizer describes lies inside the packet it came from, and that `packet::samples` and the decoder agree about a packet's duration — because `OggOpusWriter::write_packet` takes that number on trust for the granule position, and a decoder that quietly disagrees with it produces a stream claiming audio it does not carry.

## What this found

**Concealing more than 20 ms walked off the CELT decode buffer.** 20 ms is the longest frame CELT has and its decode buffer holds exactly one; the pitch branch of concealment indexes `DECODE_BUFFER_SIZE - MAX_PERIOD - n`, which goes negative past that, and the noise branch slides the buffer by `n` and runs off the end. Nothing bounded the request. Concealing a lost 40, 60 or 120 ms packet — which a caller asks for by handing `decode` an empty slice, and which any stream coded at those durations eventually needs — panicked. `decode_plc` now conceals in 20 ms pieces, as `opus_decoder.c:345` does.

`decode_stream` found it within a second of first running, from a mutation of a seed. It needed no malice: an ordinary lossy 60 ms stream reaches it. It is pinned twice now, once as a named case in `robustness.rs` and once as the fuzzer's own witness in `tests/fuzz-corpus/decode_stream/`.

Beyond that: 715k executions of `decode_stream`, 20M of `ogg_read` and 15M of `packet_shape` with no crash, hang or out-of-memory.

## The corpus

`corpus/` and `artifacts/` are untracked working directories, and they reach tens of megabytes within an hour. `tests/fuzz-corpus/` is the tracked one and is deliberately small: crash witnesses at any size, plus a sample of the minimised corpus bounded at 24 files and 16 KB a target. `tests/` ships inside the published crate, so the bound is not tidiness — it is what keeps a `.crate` that is 465 KB from becoming several megabytes of somebody else's random bytes. The full corpus is regenerated by running the fuzzer; what is tracked exists so the replay in `tests/fuzz_corpus.rs` has something real to run everywhere.

When a run produces a crash:

```sh
cargo +nightly fuzz tmin decode_stream fuzz/artifacts/decode_stream/crash-...
cp fuzz/artifacts/decode_stream/minimized-from-... tests/fuzz-corpus/decode_stream/
```

and then write the named test as well. A corpus file records *that* something broke; only a named test records *what*.
