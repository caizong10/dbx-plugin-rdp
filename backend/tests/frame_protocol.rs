//! Phase 3 unit tests: desktop frame codec + binary input decoding (pure).

use dbx_plugin_rdp::frame;

#[test]
fn frame_header_layout_is_stable() {
    // 'RDP1' | connLen u8 | connId | desktopW u16 | desktopH u16 | format u8
    //      | x y w h u16 each | seq u32 | RGBA pixels
    let frame = frame::encode_frame(
        "conn-7",
        1280,
        800,
        frame::FORMAT_RGBA32,
        (0, 0, 1280, 800),
        42,
        &[9, 9, 9, 9],
    );
    assert_eq!(&frame[0..4], b"RDP1");
    assert_eq!(frame[4], 6); // connLen("conn-7")
    assert_eq!(&frame[5..11], b"conn-7");
    assert_eq!(&frame[11..13], &1280u16.to_be_bytes());
    assert_eq!(&frame[13..15], &800u16.to_be_bytes());
    assert_eq!(frame[15], frame::FORMAT_RGBA32);
    assert_eq!(&frame[16..18], &0u16.to_be_bytes()); // x
    assert_eq!(&frame[18..20], &0u16.to_be_bytes()); // y
    assert_eq!(&frame[20..22], &1280u16.to_be_bytes()); // w
    assert_eq!(&frame[22..24], &800u16.to_be_bytes()); // h
    assert_eq!(&frame[24..28], &42u32.to_be_bytes());
    assert_eq!(&frame[28..], &[9, 9, 9, 9]);
}

#[test]
fn frame_roundtrip() {
    let pixels = vec![1u8; 2 * 2 * 4];
    let bytes = frame::encode_frame("c1", 64, 32, frame::FORMAT_RGBA32, (8, 8, 2, 2), 7, &pixels);
    let head = frame::parse_header(&bytes).unwrap();
    assert_eq!(head.connection_id, "c1");
    assert_eq!((head.desktop_width, head.desktop_height), (64, 32));
    assert_eq!(head.rect, (8, 8, 2, 2));
    assert_eq!(head.seq, 7);
    assert_eq!(&bytes[head.pixels_offset..], &pixels[..]);
}

#[test]
fn frame_rejects_bad_magic_and_truncated_input() {
    let good = frame::encode_frame("c1", 4, 4, frame::FORMAT_RGBA32, (0, 0, 4, 4), 1, &[0; 64]);
    let mut bad = good.clone();
    bad[0] = b'X';
    assert!(frame::parse_header(&bad).is_err());
    assert!(frame::parse_header(&good[..20]).is_err()); // truncated
    assert!(frame::parse_header(&[]).is_err());
}

#[test]
fn huge_rectangles_tile_under_pixel_budget() {
    // 4K full frame must split into row bands each <= MAX_TILE_PIXELS.
    let tiles = frame::tile_rect(3840, 2160, (0, 0, 3840, 2160));
    assert!(tiles.len() > 1);
    for tile in &tiles {
        assert!((tile.2 as usize) * (tile.3 as usize) <= frame::MAX_TILE_PIXELS);
    }
    let total: usize = tiles.iter().map(|t| (t.2 as usize) * (t.3 as usize)).sum();
    assert_eq!(total, 3840 * 2160);
}

#[test]
fn mouse_input_packet_roundtrips() {
    let packet = frame::mouse_bytes("conn-1", 0x8000 | 0x1000, 100, 200); // DOWN|LEFT
    let parsed = frame::parse_input(&packet).unwrap();
    assert_eq!(parsed.connection_id, "conn-1");
    match parsed.event {
        frame::InputKind::Mouse { flags, x, y } => {
            assert_eq!(flags, 0x9000);
            assert_eq!((x, y), (100, 200));
        }
        other => panic!("expected mouse, got {other:?}"),
    }
}

#[test]
fn keyboard_input_packet_roundtrips() {
    let packet = frame::key_bytes("c", 0x4000, 0x001E); // DOWN, 'a'
    let parsed = frame::parse_input(&packet).unwrap();
    match parsed.event {
        frame::InputKind::Key { flags, code } => {
            assert_eq!(flags, 0x4000);
            assert_eq!(code, 0x001E);
        }
        other => panic!("expected key, got {other:?}"),
    }
}

#[test]
fn wheel_input_packet_roundtrips() {
    let packet = frame::wheel_bytes("c", true, -120, 10, 20);
    let parsed = frame::parse_input(&packet).unwrap();
    match parsed.event {
        frame::InputKind::Wheel { is_horizontal, rotation, x, y } => {
            assert!(is_horizontal);
            assert_eq!(rotation, -120);
            assert_eq!((x, y), (10, 20));
        }
        other => panic!("expected wheel, got {other:?}"),
    }
}

#[test]
fn resize_input_packet_roundtrips() {
    let packet = frame::resize_bytes("c9", 1600, 900);
    let parsed = frame::parse_input(&packet).unwrap();
    assert_eq!(parsed.connection_id, "c9");
    match parsed.event {
        frame::InputKind::Resize { width, height } => assert_eq!((width, height), (1600, 900)),
        other => panic!("expected resize, got {other:?}"),
    }
}

#[test]
fn unicode_input_packet_roundtrips() {
    let packet = frame::unicode_bytes("c", 0, 0x4E2D); // '中'
    let parsed = frame::parse_input(&packet).unwrap();
    match parsed.event {
        frame::InputKind::Unicode { flags, code } => {
            assert_eq!(flags, 0);
            assert_eq!(code, 0x4E2D);
        }
        other => panic!("expected unicode, got {other:?}"),
    }
}

#[test]
fn garbage_input_is_rejected_not_panic() {
    assert!(frame::parse_input(&[]).is_err());
    assert!(frame::parse_input(&[9]).is_err()); // connLen says 9 but no id
    let mut bogus = frame::mouse_bytes("c", 0, 1, 2);
    bogus[1 + "c".len()] = 77; // unknown kind
    assert!(frame::parse_input(&bogus).is_err());
}

#[test]
fn long_connection_id_is_truncated_to_u8_prefix() {
    let too_long = "x".repeat(300);
    let packet = frame::mouse_bytes(&too_long, 0x0800, 0, 0);
    assert_eq!(packet[0], 255); // length prefix saturates at u8::MAX
    // The frame carries the first 255 bytes of the id; parsing round-trips it.
    let parsed = frame::parse_input(&packet).unwrap();
    assert_eq!(parsed.connection_id, "x".repeat(255));
}

#[test]
fn channels_and_events_are_named() {
    assert_eq!(frame::FRAME_CHANNEL, "frame");
    assert_eq!(frame::INPUT_CHANNEL, "input");
    assert_eq!(frame::STATUS_EVENT, "rdp/status");
    assert_eq!(frame::DESKTOP_EVENT, "rdp/desktop");
}
