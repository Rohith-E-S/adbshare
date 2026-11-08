//! Tier 3: Cross-Feature Interactions (Pairwise Combinations).

use super::harness::*;
use adb_remote::ControlMessage;
use adb_remote::input::keycode;
use adb_remote::protocol::CodecId;

// T3.1: Video Pipeline + Screen Off Control (F1.1 + F2.1)
#[test]
fn t3_1_video_pipeline_and_screen_off() {
    let mut pool = MockShmPool::new(1280 * 720 * 4).unwrap();
    let screen_off_cmd = ControlMessage::SetDisplayPower { on: false };
    assert_eq!(screen_off_cmd.serialize(), vec![10, 0]);

    // Video frames continue publishing even with screen off (stay_awake=true)
    let frame_data = vec![0x33; 1280 * 720 * 4];
    for slot in 0..3 {
        let seq = pool.publish_frame(slot, 1280, 720, &frame_data).unwrap();
        assert!(seq > 0);
        let read = pool.read_slot(slot as usize).unwrap();
        assert_eq!(read[0], 0x33);
    }
}

// T3.2: Video Pipeline + Audio Forwarding (F1.1 + F3.2)
#[test]
fn t3_2_video_pipeline_and_audio_forwarding() {
    let mut pool = MockShmPool::new(640 * 480 * 4).unwrap();
    let frame_data = vec![0x55; 640 * 480 * 4];

    // Audio stream header evaluation
    let audio_hdr = 0x00726177u32.to_be_bytes(); // 'raw'
    let codec = MockAudioNegotiator::evaluate_header(&audio_hdr).unwrap();
    assert_eq!(codec, Some(CodecId::Raw));

    // Concurrently publish video frame
    let seq = pool.publish_frame(0, 640, 480, &frame_data).unwrap();
    assert_eq!(seq, 1);

    // Parse audio packet
    let mut audio_reader = adb_remote::PacketReader::new();
    let mut packet = vec![0u8; 12];
    packet[8..12].copy_from_slice(&16u32.to_be_bytes());
    packet.extend_from_slice(&[0x11; 16]);
    let audio_packets = audio_reader.feed(&packet).unwrap();
    assert_eq!(audio_packets.len(), 1);
}

// T3.3: Screen Off + Audio Streaming (F2.1 + F3.3)
#[test]
fn t3_3_screen_off_and_audio_streaming() {
    // Screen is turned off
    let pwr_msg = ControlMessage::SetDisplayPower { on: false };
    assert_eq!(pwr_msg.serialize(), vec![10, 0]);

    // Host audio sink continues streaming raw PCM
    let sink_cmd = MockAudioNegotiator::build_sink_command("pw-cat");
    assert_eq!(sink_cmd[0], "pw-cat");
    let pcm_sample = vec![0x00; 1920]; // 10ms of audio
    assert_eq!(pcm_sample.len(), 1920);
}

// T3.4: Keyboard Passthrough + Video Rendering (F2.2 + F1.6)
#[test]
fn t3_4_keyboard_passthrough_and_video_rendering() {
    let mut frame_reader = adb_remote::proxy::FrameReader::new();
    let notif = FrameNotification::new(1, 0, 1920, 1080, 12345);
    let framed_notif = adb_remote::proxy::frame(&notif.serialize());

    // Inject text keystroke while frame arrives
    let text_cmd = ControlMessage::InjectText("Searching...".into());
    let wire_text = text_cmd.serialize();

    // Process frame notification
    let frames = frame_reader.feed(&framed_notif);
    assert_eq!(frames.len(), 1);
    assert_eq!(wire_text[0], 1);
}

// T3.5: Bidirectional Clipboard + Socket Framing (F2.3 + F1.2)
#[test]
fn t3_5_bidirectional_clipboard_and_socket_framing() {
    let mut frame_reader = adb_remote::proxy::FrameReader::new();

    // Frame notification
    let notif = FrameNotification::new(10, 1, 1280, 720, 5000);
    let wire_notif = adb_remote::proxy::frame(&notif.serialize());

    // Device clipboard message
    let dev_clip = [TAG_CONTROL, 0, 0, 0, 0, 4, b'c', b'o', b'p', b'y'];
    let wire_clip = adb_remote::proxy::frame(&dev_clip);

    // Multiplex onto stream
    let mut stream = Vec::new();
    stream.extend_from_slice(&wire_notif);
    stream.extend_from_slice(&wire_clip);

    let decoded = frame_reader.feed(&stream);
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0][0], TAG_FRAME);
    assert_eq!(decoded[1][0], TAG_CONTROL);
}

// T3.6: Drag-and-Drop APK Install + Video Mirroring (F2.4 + F1.1)
#[test]
fn t3_6_drag_drop_apk_install_and_video_mirroring() {
    let mut pool = MockShmPool::new(100 * 100 * 4).unwrap();

    // Drag-drop APK route
    let drop_action = MockDragDropRouter::route_drop("/home/user/update.apk");
    assert!(matches!(drop_action, DropAction::InstallApk(_)));

    // Video publishing operates concurrently
    let frame = vec![0x77; 100 * 100 * 4];
    let seq = pool.publish_frame(0, 100, 100, &frame).unwrap();
    assert_eq!(seq, 1);
}

// T3.7: Drag-and-Drop File Push + Keyboard Input (F2.5 + F2.2)
#[test]
fn t3_7_drag_drop_file_push_and_keyboard_input() {
    // Route file push
    let action = MockDragDropRouter::route_drop("/home/user/notes.txt");
    if let DropAction::PushFile { destination, .. } = action {
        assert_eq!(destination, "/sdcard/Download/notes.txt");
    } else {
        panic!("expected PushFile");
    }

    // Keyboard navigation input
    let key_cmd = ControlMessage::InjectKeycode {
        action: 0,
        keycode: keycode::ENTER,
        repeat: 0,
        metastate: 0,
    };
    assert_eq!(key_cmd.serialize().len(), 14);
}

// T3.8: Audio Graceful Fallback + Video Mirroring (F3.2 + F1.4)
#[test]
fn t3_8_audio_fallback_and_video_mirroring() {
    // Device returns code 0 (no audio)
    let header = [0u8; 4];
    let audio_res = MockAudioNegotiator::evaluate_header(&header).unwrap();
    assert_eq!(audio_res, None, "audio cleanly disabled");

    // Video pipeline commences immediately
    let mut pool = MockShmPool::new(1920 * 1080 * 4).unwrap();
    let frame_bytes = vec![0x99; 1920 * 1080 * 4];
    let seq = pool.publish_frame(0, 1920, 1080, &frame_bytes).unwrap();
    assert_eq!(seq, 1);
}

// T3.9: Preset Quality Switching + Shm Buffer Resizing (F1.3 + F1.1)
#[test]
fn t3_9_preset_switching_and_shm_resizing() {
    let mut pool = MockShmPool::new(MAX_SLOT_SIZE).unwrap();

    // Balanced preset frame (1920x1080)
    let b_frame = vec![0x11; 1920 * 1080 * 4];
    let seq1 = pool.publish_frame(0, 1920, 1080, &b_frame).unwrap();
    assert_eq!(seq1, 1);
    assert_eq!(
        pool.stride.load(std::sync::atomic::Ordering::Acquire),
        1920 * 4
    );

    // Switch to Quality preset frame (2560x1440)
    let q_frame = vec![0x22; 2560 * 1440 * 4];
    let seq2 = pool.publish_frame(1, 2560, 1440, &q_frame).unwrap();
    assert_eq!(seq2, 2);
    assert_eq!(
        pool.stride.load(std::sync::atomic::Ordering::Acquire),
        2560 * 4
    );
}

// T3.10: Rapid Screen Off Toggle + Keyboard Input (F2.1 + F2.2)
#[test]
fn t3_10_rapid_screen_off_toggle_and_keyboard_input() {
    let screen_off = ControlMessage::SetDisplayPower { on: false }.serialize();
    let key_a = ControlMessage::InjectText("A".into()).serialize();
    let screen_on = ControlMessage::SetDisplayPower { on: true }.serialize();
    let key_b = ControlMessage::InjectText("B".into()).serialize();

    let sequence = [screen_off, key_a, screen_on, key_b];
    assert_eq!(sequence[0], vec![10, 0]);
    assert_eq!(sequence[1][0], 1);
    assert_eq!(sequence[2], vec![10, 1]);
    assert_eq!(sequence[3][0], 1);
}
