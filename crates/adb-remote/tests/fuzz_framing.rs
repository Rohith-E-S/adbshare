//! Adversarial fuzzing and empirical stress tests for adb-remote framing and protocol parsers.
//!
//! Covers:
//! - FrameNotification serialization, parsing, truncation (< 29 bytes), corrupt tags, extreme values
//! - FrameReader fragmentation (1 byte at a time, arbitrary chunking), zero-length frames, MAX_FRAME bounds
//! - Codec ID parsing (parse_codec_id_with_status, parse_codec_id), status codes (0, 1), random bytes, truncations
//! - Zero panics, zero out-of-bounds slicing, robust error returns.

use adb_remote::protocol::{CodecId, ProtocolError, parse_codec_id, parse_codec_id_with_status};
use adb_remote::proxy::{
    FRAME_NOTIFICATION_BODY_SIZE, FRAME_NOTIFICATION_SIZE, FrameNotification, FrameReader,
    MAX_FRAME, TAG_CONTROL, TAG_FRAME, TAG_META, TAG_VIDEO, frame,
};

/// Deterministic Pseudo-Random Number Generator (Xorshift64*).
/// Fast, zero-dependency, and completely reproducible.
struct FuzzRng {
    state: u64,
}

impl FuzzRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x8a5cd789635d2dff } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let val = self.next_u64().to_le_bytes();
            let len = chunk.len();
            chunk.copy_from_slice(&val[..len]);
        }
    }

    fn gen_vec(&mut self, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        self.fill_bytes(&mut v);
        v
    }
}

// =========================================================================
// 1. FrameNotification Fuzzing & Truncation Tests
// =========================================================================

#[test]
fn test_frame_notification_truncation_all_sizes() {
    let valid = FrameNotification::new(12345, 1, 1080, 2400, 999_999_999);
    let valid_bytes = valid.to_bytes();
    assert_eq!(valid_bytes.len(), FRAME_NOTIFICATION_SIZE);

    // Every prefix shorter than 29 bytes MUST return None and NEVER panic
    for len in 0..FRAME_NOTIFICATION_SIZE {
        let truncated = &valid_bytes[..len];
        assert_eq!(
            FrameNotification::parse(truncated),
            None,
            "Truncated length {len} should fail parse"
        );
    }

    // Every body shorter than 28 bytes MUST return None and NEVER panic
    let valid_body = &valid_bytes[1..];
    for len in 0..FRAME_NOTIFICATION_BODY_SIZE {
        let truncated_body = &valid_body[..len];
        assert_eq!(
            FrameNotification::parse_body(truncated_body),
            None,
            "Truncated body length {len} should fail parse_body"
        );
    }
}

#[test]
fn test_frame_notification_corrupted_tags() {
    let valid = FrameNotification::new(42, 0, 1920, 1080, 1_000_000);
    let mut bytes = valid.to_bytes();

    // Any tag byte other than TAG_FRAME (b'F') must reject the frame
    for bad_tag in 0..=255u8 {
        if bad_tag == TAG_FRAME {
            continue;
        }
        bytes[0] = bad_tag;
        assert_eq!(
            FrameNotification::parse(&bytes),
            None,
            "Tag {bad_tag:#04x} should have been rejected"
        );
    }
}

#[test]
fn test_frame_notification_extreme_values_roundtrip() {
    let extreme_cases = [
        // All zeros
        (0u64, 0u32, 0u32, 0u32, 0u64),
        // All u64::MAX / u32::MAX
        (u64::MAX, u32::MAX, u32::MAX, u32::MAX, u64::MAX),
        // Alternating bit patterns
        (
            0x5555_5555_5555_5555,
            0x5555_5555,
            0xAAAA_AAAA,
            0x5555_5555,
            0xAAAA_AAAA_AAAA_AAAA,
        ),
        // High bit set (signed negative if misparsed as signed)
        (1 << 63, 1 << 31, 1 << 31, 1 << 31, 1 << 63),
        // Standard Android 4K resolution at 120 FPS timestamp
        (1_000_000, 2, 3840, 2160, 1_700_000_000_000_000_000),
        // Real-world zero width/height edge case
        (100, 0, 0, 0, 12345),
    ];

    for (seq, slot, width, height, ts) in extreme_cases {
        let notif = FrameNotification::new(seq, slot, width, height, ts);
        let bytes = notif.to_bytes();
        assert_eq!(bytes[0], TAG_FRAME);

        let parsed = FrameNotification::parse(&bytes)
            .unwrap_or_else(|| panic!("Failed to parse extreme case: seq={seq}, slot={slot}"));
        assert_eq!(parsed, notif);
        assert_eq!(parsed.seq, seq);
        assert_eq!(parsed.slot, slot);
        assert_eq!(parsed.width, width);
        assert_eq!(parsed.height, height);
        assert_eq!(parsed.timestamp_ns, ts);

        // Test parse_body directly
        let parsed_body =
            FrameNotification::parse_body(&bytes[1..]).expect("Failed to parse body directly");
        assert_eq!(parsed_body, notif);

        // Test wire format frame wrapping
        let framed = notif.to_frame();
        assert_eq!(framed.len(), 4 + FRAME_NOTIFICATION_SIZE);
        let frame_len = u32::from_be_bytes([framed[0], framed[1], framed[2], framed[3]]);
        assert_eq!(frame_len, FRAME_NOTIFICATION_SIZE as u32);
        assert_eq!(&framed[4..], &bytes[..]);
    }
}

#[test]
fn test_frame_notification_fuzz_random_bytes() {
    let mut rng = FuzzRng::new(0xCAFE_BABE_1234_5678);

    // Fuzz 50,000 arbitrary byte vectors: MUST NEVER PANIC
    for _ in 0..50_000 {
        let len = (rng.next_u32() % 128) as usize;
        let buf = rng.gen_vec(len);

        let parsed = FrameNotification::parse(&buf);
        if len < FRAME_NOTIFICATION_SIZE || buf[0] != TAG_FRAME {
            assert_eq!(parsed, None);
        } else {
            // If tag is TAG_FRAME and len >= 29, it should successfully parse
            let notif = parsed.expect("Should parse valid prefix");
            assert_eq!(notif.tag, TAG_FRAME);
        }

        // Test parse_body with same random slice
        let body_parsed = FrameNotification::parse_body(&buf);
        if len < FRAME_NOTIFICATION_BODY_SIZE {
            assert_eq!(body_parsed, None);
        } else {
            assert!(body_parsed.is_some());
        }
    }
}

// =========================================================================
// 2. FrameReader Fragmentation & Chunking Tests
// =========================================================================

#[test]
fn test_frame_reader_single_byte_fragmentation() {
    let mut notifs = Vec::new();
    let mut stream = Vec::new();

    // Create 30 distinct frame notifications
    for i in 0..30 {
        let n = FrameNotification::new(
            i as u64,
            (i % 3) as u32,
            1080 + (i * 10) as u32,
            1920 + (i * 10) as u32,
            1_700_000_000 + i as u64,
        );
        stream.extend_from_slice(&n.to_frame());
        notifs.push(n);
    }

    assert_eq!(stream.len(), 30 * 33);

    // Feed the stream 1 byte at a time!
    let mut reader = FrameReader::new();
    let mut recovered = Vec::new();

    for byte in &stream {
        let frames = reader.feed(std::slice::from_ref(byte));
        for f in frames {
            let n = FrameNotification::parse(&f).expect("Failed to parse recovered frame");
            recovered.push(n);
        }
    }

    assert_eq!(recovered.len(), notifs.len());
    assert_eq!(recovered, notifs);
}

#[test]
fn test_frame_reader_arbitrary_chunking_stress() {
    let mut rng = FuzzRng::new(0xDEAD_BEEF_9876_5432);

    // Generate a diverse payload stream: frames of various lengths and tags
    let mut expected_payloads = Vec::new();
    let mut stream = Vec::new();

    for i in 0..100 {
        let payload: Vec<u8> = match i % 5 {
            0 => {
                // TAG_FRAME
                let n = FrameNotification::new(i, (i % 3) as u32, 1920, 1080, i * 1000);
                n.to_vec()
            }
            1 => {
                // Empty payload
                vec![]
            }
            2 => {
                // TAG_META
                vec![TAG_META, b'h', b'2', b'6', b'4']
            }
            3 => {
                // TAG_CONTROL
                let mut c = vec![TAG_CONTROL];
                let sz = (rng.next_u32() % 200) as usize;
                let payload_bytes = rng.gen_vec(sz);
                c.extend_from_slice(&payload_bytes);
                c
            }
            _ => {
                // TAG_VIDEO variable packet
                let mut v = vec![TAG_VIDEO];
                let sz = (rng.next_u32() % 1024) as usize;
                let payload_bytes = rng.gen_vec(sz);
                v.extend_from_slice(&payload_bytes);
                v
            }
        };

        let wire = frame(&payload);
        stream.extend_from_slice(&wire);
        expected_payloads.push(payload);
    }

    // Test with prime and odd chunk sizes
    let chunk_sizes = [1, 2, 3, 5, 7, 11, 13, 17, 31, 64, 127, 256, 512, 1024];

    for &chunk_size in &chunk_sizes {
        let mut reader = FrameReader::new();
        let mut got = Vec::new();

        for chunk in stream.chunks(chunk_size) {
            got.extend(reader.feed(chunk));
        }

        assert_eq!(
            got.len(),
            expected_payloads.len(),
            "Chunk size {chunk_size} yielded mismatched frame count"
        );
        assert_eq!(
            got, expected_payloads,
            "Chunk size {chunk_size} corrupted frame payloads"
        );
    }

    // Test with randomized chunk boundaries
    for run in 0..20 {
        let mut reader = FrameReader::new();
        let mut got = Vec::new();
        let mut pos = 0;

        while pos < stream.len() {
            let chunk_len = ((rng.next_u32() % 64) + 1) as usize;
            let end = (pos + chunk_len).min(stream.len());
            got.extend(reader.feed(&stream[pos..end]));
            pos = end;
        }

        assert_eq!(
            got.len(),
            expected_payloads.len(),
            "Random run {run} yielded mismatched frame count"
        );
        assert_eq!(got, expected_payloads, "Random run {run} corrupted frames");
    }
}

#[test]
fn test_frame_reader_boundary_max_frame() {
    let mut reader = FrameReader::new();

    // Exact MAX_FRAME payload
    let max_payload = vec![0x42; MAX_FRAME as usize];
    let wire = frame(&max_payload);
    assert_eq!(wire.len(), 4 + MAX_FRAME as usize);

    let got = reader.feed(&wire);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].len(), MAX_FRAME as usize);
    assert_eq!(got[0], max_payload);

    // Frame declaring (MAX_FRAME + 1): reader must NOT panic or slice out of bounds
    let mut oversized_wire = Vec::new();
    oversized_wire.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
    oversized_wire.extend_from_slice(&[0xAA; 100]);

    let got_oversized = reader.feed(&oversized_wire);
    assert!(
        got_oversized.is_empty(),
        "Oversized frame must not be returned"
    );

    // Frame declaring u32::MAX
    let mut max_u32_wire = Vec::new();
    max_u32_wire.extend_from_slice(&u32::MAX.to_be_bytes());
    max_u32_wire.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);

    let got_max = reader.feed(&max_u32_wire);
    assert!(got_max.is_empty(), "u32::MAX frame must not be returned");
}

#[test]
fn test_frame_reader_empty_feeds_and_zero_lengths() {
    let mut reader = FrameReader::new();

    // Feeding empty slices should be a harmless no-op
    assert!(reader.feed(&[]).is_empty());
    assert!(reader.feed(&[]).is_empty());

    // Feeding frame of length 0
    let zero_frame = frame(&[]);
    assert_eq!(zero_frame, vec![0, 0, 0, 0]);

    let got = reader.feed(&zero_frame);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], Vec::<u8>::new());

    // Multiple consecutive zero-length frames
    let multi_zero = [frame(&[]), frame(&[]), frame(&[])].concat();
    let got_multi = reader.feed(&multi_zero);
    assert_eq!(got_multi, vec![vec![], vec![], vec![]]);
}

// =========================================================================
// 3. Codec ID & Status Parsing Fuzzing Tests
// =========================================================================

#[test]
fn test_codec_id_truncation_all_sizes() {
    let full = b"h264";

    // Lengths 0, 1, 2, 3 must return Err(ProtocolError::TooShort)
    for len in 0..4 {
        assert_eq!(
            parse_codec_id_with_status(&full[..len]),
            Err(ProtocolError::TooShort),
            "Length {len} must return TooShort"
        );
        assert_eq!(
            parse_codec_id(&full[..len]),
            None,
            "Length {len} in parse_codec_id must return None"
        );
    }
}

#[test]
fn test_codec_id_status_codes_and_known_codecs() {
    // 0x00000000 -> Disabled
    assert_eq!(
        parse_codec_id_with_status(&[0x00, 0x00, 0x00, 0x00]),
        Ok(None)
    );
    assert_eq!(parse_codec_id(&[0x00, 0x00, 0x00, 0x00]), None);

    // 0x00000001 -> Configuration Error
    assert_eq!(
        parse_codec_id_with_status(&[0x00, 0x00, 0x00, 0x01]),
        Err(ProtocolError::ConfigurationError)
    );
    assert_eq!(parse_codec_id(&[0x00, 0x00, 0x00, 0x01]), None);

    // All registered codecs
    let all_codecs = [
        (CodecId::H264, *b"h264", false, true),
        (CodecId::H265, *b"h265", false, true),
        (CodecId::Av1, *b"\0av1", false, true),
        (CodecId::Vp8, *b"\0vp8", false, true),
        (CodecId::Vp9, *b"\0vp9", false, true),
        (CodecId::Opus, *b"opus", true, false),
        (CodecId::Aac, *b"\0aac", true, false),
        (CodecId::Flac, *b"flac", true, false),
        (CodecId::Raw, *b"\0raw", true, false),
    ];

    for (codec, expected_bytes, is_audio, is_video) in all_codecs {
        assert_eq!(parse_codec_id_with_status(&expected_bytes), Ok(Some(codec)));
        assert_eq!(parse_codec_id(&expected_bytes), Some(codec));
        assert_eq!(codec.to_be_bytes(), expected_bytes);
        assert_eq!(codec.to_u32(), u32::from_be_bytes(expected_bytes));
        assert_eq!(codec.is_audio(), is_audio);
        assert_eq!(codec.is_video(), is_video);

        // Forward compatibility: Trailing bytes must be ignored
        let mut with_trailing = expected_bytes.to_vec();
        with_trailing.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(parse_codec_id_with_status(&with_trailing), Ok(Some(codec)));
        assert_eq!(parse_codec_id(&with_trailing), Some(codec));
    }
}

#[test]
fn test_codec_id_fuzz_random_values() {
    let mut rng = FuzzRng::new(0x1357_9BDF_0246_8ACE);

    let known_u32s: [u32; 11] = [
        0x00000000, // disabled
        0x00000001, // config error
        0x68323634, // h264
        0x68323635, // h265
        0x00617631, // av1
        0x00767038, // vp8
        0x00767039, // vp9
        0x6F707573, // opus
        0x00616163, // aac
        0x666C6163, // flac
        0x00726177, // raw
    ];

    // Fuzz 100,000 random 4-byte values
    for _ in 0..100_000 {
        let val = rng.next_u32();
        let bytes = val.to_be_bytes();

        let res = parse_codec_id_with_status(&bytes);

        if !known_u32s.contains(&val) {
            assert_eq!(
                res,
                Err(ProtocolError::UnknownCodec),
                "Value {val:#010x} should be UnknownCodec"
            );
            assert_eq!(parse_codec_id(&bytes), None);
        } else {
            // Must succeed or be config error
            match val {
                0 => assert_eq!(res, Ok(None)),
                1 => assert_eq!(res, Err(ProtocolError::ConfigurationError)),
                _ => assert!(matches!(res, Ok(Some(_)))),
            }
        }
    }
}

// =========================================================================
// 4. End-to-End Framing Stream Stress Test
// =========================================================================

#[test]
fn test_interleaved_fuzz_stream_reassembly() {
    let mut rng = FuzzRng::new(0xFEED_FACE_CAFE_BEEF);

    // Simulate an interleaved stream over a live Unix socket connection
    let mut original_frames = Vec::new();
    let mut wire_stream = Vec::new();

    for i in 0..200 {
        let tag = match i % 4 {
            0 => TAG_FRAME,
            1 => TAG_META,
            2 => TAG_CONTROL,
            _ => TAG_VIDEO,
        };

        let payload = match tag {
            TAG_FRAME => {
                let n = FrameNotification::new(
                    i as u64,
                    (i % 3) as u32,
                    1920,
                    1080,
                    1_700_000_000 + i as u64,
                );
                n.to_vec()
            }
            TAG_META => {
                vec![TAG_META, b'h', b'2', b'6', b'4']
            }
            TAG_CONTROL => {
                let mut p = vec![TAG_CONTROL];
                let sz = (rng.next_u32() % 64) as usize;
                let payload_bytes = rng.gen_vec(sz);
                p.extend_from_slice(&payload_bytes);
                p
            }
            TAG_VIDEO => {
                let mut p = vec![TAG_VIDEO];
                let sz = (rng.next_u32() % 512) as usize;
                let payload_bytes = rng.gen_vec(sz);
                p.extend_from_slice(&payload_bytes);
                p
            }
            _ => unreachable!(),
        };

        wire_stream.extend_from_slice(&frame(&payload));
        original_frames.push(payload);
    }

    // Chop stream into random chunks (1..=37 bytes)
    let mut reader = FrameReader::new();
    let mut parsed_frames = Vec::new();
    let mut offset = 0;

    while offset < wire_stream.len() {
        let chunk_len = ((rng.next_u32() % 37) + 1) as usize;
        let end = (offset + chunk_len).min(wire_stream.len());
        parsed_frames.extend(reader.feed(&wire_stream[offset..end]));
        offset = end;
    }

    assert_eq!(parsed_frames.len(), original_frames.len());

    // Verify each parsed frame matches original, and FrameNotification parses cleanly
    for (i, (got, expected)) in parsed_frames.iter().zip(&original_frames).enumerate() {
        assert_eq!(got, expected, "Frame {i} mismatched");

        if got[0] == TAG_FRAME {
            let notif = FrameNotification::parse(got).expect("TAG_FRAME failed to parse");
            assert_eq!(notif.seq, i as u64);
            assert_eq!(notif.slot, (i % 3) as u32);
            assert_eq!(notif.width, 1920);
            assert_eq!(notif.height, 1080);
        }
    }
}

// =========================================================================
// 5. Single-Bit-Flip & Deep Mutation Fuzzing
// =========================================================================

#[test]
fn test_frame_notification_single_bit_flip_fuzzing() {
    let valid = FrameNotification::new(
        0x1122_3344_5566_7788,
        0x0102_0304,
        1920,
        1080,
        0x0807_0605_0403_0201,
    );
    let valid_bytes = valid.to_bytes();

    // Flip every single bit across the 29-byte payload (232 individual bit flips)
    for byte_idx in 0..FRAME_NOTIFICATION_SIZE {
        for bit_idx in 0..8 {
            let mut corrupted = valid_bytes;
            corrupted[byte_idx] ^= 1 << bit_idx;

            let parsed = FrameNotification::parse(&corrupted);
            if byte_idx == 0 {
                // Byte 0 is the tag: flipping any bit changes it from TAG_FRAME (b'F')
                // b'F' = 0x46 = 0b01000110. Flipping any bit produces != 0x46.
                assert_eq!(
                    parsed, None,
                    "Bit flip at byte {byte_idx} bit {bit_idx} should have invalidated tag"
                );
            } else {
                // Flipping bits in payload fields (seq, slot, width, height, ts)
                // must succeed in parsing, but must produce a DIFFERENT value than original
                let notif =
                    parsed.expect("Bit flip in payload must still produce valid FrameNotification");
                assert_eq!(notif.tag, TAG_FRAME);
                assert_ne!(
                    notif, valid,
                    "Corrupted byte {byte_idx} bit {bit_idx} produced identical notification"
                );
            }
        }
    }
}

// =========================================================================
// 6. FrameReader Corrupted Length Behavior & Protocol Recovery
// =========================================================================

#[test]
fn test_frame_reader_corrupt_length_prefix_behavior() {
    let mut reader = FrameReader::new();

    // Feeding a frame header with length declared as MAX_FRAME + 1
    let bad_len = (MAX_FRAME + 1).to_be_bytes();
    assert!(reader.feed(&bad_len).is_empty());

    // Subsequent valid frames fed into the reader will wait because the reader's
    // internal buffer holds the oversized length at offset 0
    let valid_frame = frame(b"valid message");
    let got = reader.feed(&valid_frame);
    assert!(
        got.is_empty(),
        "Reader should stall on oversized length header without crashing"
    );

    // Fresh reader processes valid frames normally
    let mut fresh_reader = FrameReader::new();
    let got_fresh = fresh_reader.feed(&valid_frame);
    assert_eq!(got_fresh, vec![b"valid message".to_vec()]);
}

#[test]
fn test_frame_reader_stress_random_bursts() {
    let mut rng = FuzzRng::new(0x8765_4321_ABCD_EF01);
    let mut reader = FrameReader::new();

    // Feed 10,000 random small chunks of noise (< 4 bytes)
    for _ in 0..10_000 {
        let chunk_len = (rng.next_u32() % 4) as usize;
        let chunk = rng.gen_vec(chunk_len);
        let _ = reader.feed(&chunk);
    }
    // Reader must survive without any panic or out-of-bounds index
}

// =========================================================================
// 7. PacketReader & Device Message Adversarial Boundaries
// =========================================================================

#[test]
fn test_packet_reader_and_device_message_adversarial() {
    use adb_remote::protocol::{PacketReader, parse_device_message};

    // 1. PacketReader with size > MAX_PACKET_SIZE
    let mut pkt_reader = PacketReader::new();
    let mut bad_header = [0u8; 12];
    // size at bytes 8..12 set to 65 MiB (exceeding 64 MiB limit)
    bad_header[8..12].copy_from_slice(&(65 * 1024 * 1024u32).to_be_bytes());
    let res = pkt_reader.feed(&bad_header);
    assert_eq!(res, Err(ProtocolError::PacketTooLarge(65 * 1024 * 1024)));

    // 2. parse_device_message with oversized clipboard (> 1 MiB)
    let mut bad_clipboard = vec![0u8]; // msg type 0 = Clipboard
    bad_clipboard.extend_from_slice(&(1024 * 1024 + 1u32).to_be_bytes()); // len > 1 MiB
    bad_clipboard.extend_from_slice(b"small text");
    assert_eq!(parse_device_message(&bad_clipboard), None);

    // 3. parse_device_message with unknown message types (3..=255)
    for bad_type in 3..=255u8 {
        let buf = [bad_type, 0x00, 0x00, 0x00];
        assert_eq!(parse_device_message(&buf), None);
    }
}
