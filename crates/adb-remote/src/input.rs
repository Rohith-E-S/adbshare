//! Control-message encoding (client → device), byte-compatible with scrcpy's
//! `sc_control_msg_serialize` (app/src/control_msg.c).
//!
//! Message types (first byte), fixed layouts are big-endian:
//! ```text
//! 0  inject keycode:      [action][keycode u32][repeat u32][metastate u32]  (14 B)
//! 1  inject text:         [len u32][utf8 ≤ 300]
//! 2  inject touch event:  [action][pointer_id u64][x u32][y u32]
//!                         [w u16][h u16][pressure u16fp][action_button u32][buttons u32] (32 B)
//! 3  inject scroll event: [x u32][y u32][w u16][h u16][hscroll i16fp][vscroll i16fp][buttons u32] (21 B)
//! 4  back or screen on:   [action]                                          (2 B)
//! 5  expand notifications / 6 expand settings / 7 collapse panels / 11 rotate (1 B)
//! 8  get clipboard:       [copy_key]                                        (2 B)
//! 9  set clipboard:       [seq u64][paste u8][len u32][text]
//! 10 set display power:   [on u8]                                           (2 B)
//! 17 reset video:         —                                                 (1 B)
//! 21 resize display:      [w u16][h u16]                                    (5 B)
//! ```

pub const MAX_TEXT_LENGTH: usize = 300;

/// Android keycodes (android/view/KeyEvent.java values).
pub mod keycode {
    pub const UNKNOWN: i32 = 0;
    pub const HOME: i32 = 3;
    pub const BACK: i32 = 4;
    pub const DPAD_UP: i32 = 19;
    pub const DPAD_DOWN: i32 = 20;
    pub const DPAD_LEFT: i32 = 21;
    pub const DPAD_RIGHT: i32 = 22;
    pub const DPAD_CENTER: i32 = 23;
    pub const VOLUME_UP: i32 = 24;
    pub const VOLUME_DOWN: i32 = 25;
    pub const POWER: i32 = 26;
    pub const CAMERA: i32 = 27;
    pub const TAB: i32 = 61;
    pub const SPACE: i32 = 62;
    pub const DEL: i32 = 67;
    pub const ENTER: i32 = 66;
    pub const ESCAPE: i32 = 111;
    pub const SHIFT_LEFT: i32 = 59;
    pub const SHIFT_RIGHT: i32 = 60;
}

/// Android MotionEvent actions.
pub mod motion_action {
    pub const DOWN: i32 = 0;
    pub const UP: i32 = 1;
    pub const MOVE: i32 = 2;
    pub const CANCEL: i32 = 3;
}

pub const POINTER_ID_MOUSE: u64 = u64::MAX; // -1
pub const POINTER_ID_FINGER: u64 = u64::MAX - 1; // -2
pub const POINTER_ID_VIRTUAL_FINGER: u64 = u64::MAX - 2; // -3

/// Key event actions.
pub mod key_action {
    pub const DOWN: u8 = 0;
    pub const UP: u8 = 1;
}

/// Convert float [0,1] to U16 fixed-point, matching sc_float_to_u16fp.
pub fn float_to_u16fp(f: f32) -> u16 {
    let f = f.clamp(0.0, 1.0);
    let u = (f * 65536.0) as u32;
    u.min(0xffff) as u16
}

/// Convert float [-1,1] to I16 fixed-point, matching sc_float_to_i16fp.
pub fn float_to_i16fp(f: f32) -> i16 {
    let f = f.clamp(-1.0, 1.0);
    let i = (f * 32768.0) as i32;
    i.clamp(-0x8000, 0x7fff) as i16
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlMessage {
    InjectKeycode {
        action: u8,
        keycode: i32,
        repeat: u32,
        metastate: u32,
    },
    InjectText(String),
    InjectTouchEvent {
        action: i32,
        pointer_id: u64,
        x: i32,
        y: i32,
        screen_w: u16,
        screen_h: u16,
        pressure: f32,
        action_button: u32,
        buttons: u32,
    },
    InjectScrollEvent {
        x: i32,
        y: i32,
        screen_w: u16,
        screen_h: u16,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    },
    BackOrScreenOn {
        action: u8,
    },
    ExpandNotificationPanel,
    ExpandSettingsPanel,
    CollapsePanels,
    GetClipboard {
        copy: bool,
    },
    SetClipboard {
        sequence: u64,
        paste: bool,
        text: String,
    },
    SetDisplayPower {
        on: bool,
    },
    RotateDevice,
    ResetVideo,
    ResizeDisplay {
        width: u16,
        height: u16,
    },
}

impl ControlMessage {
    /// Serialise to the wire format (no length prefix; the proxy layer frames it).
    pub fn serialize(&self) -> Vec<u8> {
        match self {
            ControlMessage::InjectKeycode {
                action,
                keycode,
                repeat,
                metastate,
            } => {
                let mut b = vec![0u8, *action];
                b.extend_from_slice(&(*keycode as u32).to_be_bytes());
                b.extend_from_slice(&repeat.to_be_bytes());
                b.extend_from_slice(&metastate.to_be_bytes());
                b
            }
            ControlMessage::InjectText(text) => {
                let bytes = truncate_utf8(text, MAX_TEXT_LENGTH);
                let mut b = vec![1u8];
                b.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                b.extend_from_slice(bytes);
                b
            }
            ControlMessage::InjectTouchEvent {
                action,
                pointer_id,
                x,
                y,
                screen_w,
                screen_h,
                pressure,
                action_button,
                buttons,
            } => {
                let x = (*x).clamp(0, (*screen_w as i32).saturating_sub(1).max(0));
                let y = (*y).clamp(0, (*screen_h as i32).saturating_sub(1).max(0));
                let mut b = vec![2u8, *action as u8];
                b.extend_from_slice(&pointer_id.to_be_bytes());
                b.extend_from_slice(&x.to_be_bytes());
                b.extend_from_slice(&y.to_be_bytes());
                b.extend_from_slice(&screen_w.to_be_bytes());
                b.extend_from_slice(&screen_h.to_be_bytes());
                b.extend_from_slice(&float_to_u16fp(*pressure).to_be_bytes());
                b.extend_from_slice(&action_button.to_be_bytes());
                b.extend_from_slice(&buttons.to_be_bytes());
                b
            }
            ControlMessage::InjectScrollEvent {
                x,
                y,
                screen_w,
                screen_h,
                hscroll,
                vscroll,
                buttons,
            } => {
                let x = (*x).clamp(0, (*screen_w as i32).saturating_sub(1).max(0));
                let y = (*y).clamp(0, (*screen_h as i32).saturating_sub(1).max(0));
                // scrcpy accepts [-16,16] and normalises to [-1,1] before fp conversion.
                let h = float_to_i16fp((hscroll / 16.0).clamp(-1.0, 1.0));
                let v = float_to_i16fp((vscroll / 16.0).clamp(-1.0, 1.0));
                let mut b = vec![3u8];
                b.extend_from_slice(&x.to_be_bytes());
                b.extend_from_slice(&y.to_be_bytes());
                b.extend_from_slice(&screen_w.to_be_bytes());
                b.extend_from_slice(&screen_h.to_be_bytes());
                b.extend_from_slice(&h.to_be_bytes());
                b.extend_from_slice(&v.to_be_bytes());
                b.extend_from_slice(&buttons.to_be_bytes());
                b
            }
            ControlMessage::BackOrScreenOn { action } => vec![4u8, *action],
            ControlMessage::ExpandNotificationPanel => vec![5u8],
            ControlMessage::ExpandSettingsPanel => vec![6u8],
            ControlMessage::CollapsePanels => vec![7u8],
            ControlMessage::GetClipboard { copy } => vec![8u8, u8::from(*copy)],
            ControlMessage::SetClipboard {
                sequence,
                paste,
                text,
            } => {
                let bytes = truncate_utf8(text, 1 << 20);
                let mut b = vec![9u8];
                b.extend_from_slice(&sequence.to_be_bytes());
                b.push(u8::from(*paste));
                b.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                b.extend_from_slice(bytes);
                b
            }
            ControlMessage::SetDisplayPower { on } => vec![10u8, u8::from(*on)],
            ControlMessage::RotateDevice => vec![11u8],
            ControlMessage::ResetVideo => vec![17u8],
            ControlMessage::ResizeDisplay { width, height } => {
                let mut b = vec![21u8];
                b.extend_from_slice(&width.to_be_bytes());
                b.extend_from_slice(&height.to_be_bytes());
                b
            }
        }
    }
}

/// Truncate to at most `max` bytes without splitting a UTF-8 codepoint.
fn truncate_utf8(s: &str, max: usize) -> &[u8] {
    let bytes = s.as_bytes();
    if bytes.len() <= max {
        return bytes;
    }
    let mut end = max;
    while end > 0 && (bytes[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_is_14_bytes() {
        let m = ControlMessage::InjectKeycode {
            action: 0,
            keycode: 66,
            repeat: 0,
            metastate: 0,
        };
        let b = m.serialize();
        assert_eq!(b.len(), 14);
        assert_eq!(b[0], 0);
        assert_eq!(u32::from_be_bytes([b[2], b[3], b[4], b[5]]), 66);
    }

    #[test]
    fn touch_event_is_32_bytes() {
        let m = ControlMessage::InjectTouchEvent {
            action: 0,
            pointer_id: POINTER_ID_FINGER,
            x: 100,
            y: 200,
            screen_w: 1080,
            screen_h: 2400,
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        };
        let b = m.serialize();
        assert_eq!(b.len(), 32);
        assert_eq!(b[0], 2);
        assert_eq!(u32::from_be_bytes([b[10], b[11], b[12], b[13]]), 100);
        assert_eq!(u16::from_be_bytes([b[22], b[23]]), 0xffff);
    }

    #[test]
    fn scroll_normalises_to_fp() {
        let m = ControlMessage::InjectScrollEvent {
            x: 0,
            y: 0,
            screen_w: 1,
            screen_h: 1,
            hscroll: 16.0,
            vscroll: -16.0,
            buttons: 0,
        };
        let b = m.serialize();
        assert_eq!(b.len(), 21);
        assert_eq!(i16::from_be_bytes([b[13], b[14]]), 0x7fff);
        assert_eq!(i16::from_be_bytes([b[15], b[16]]), -0x8000);
    }

    #[test]
    fn text_truncates_on_codepoint_boundary() {
        let long = "é".repeat(200); // 400 bytes, 2 bytes per char
        let b = ControlMessage::InjectText(long).serialize();
        assert!(b.len() <= 1 + 4 + MAX_TEXT_LENGTH);
        let len = u32::from_be_bytes([b[1], b[2], b[3], b[4]]) as usize;
        assert_eq!(b.len(), 5 + len);
        assert!(len <= MAX_TEXT_LENGTH);
        assert!(std::str::from_utf8(&b[5..]).is_ok());
    }

    #[test]
    fn clipboard_and_power() {
        let b = ControlMessage::SetClipboard {
            sequence: 7,
            paste: false,
            text: "hi".into(),
        }
        .serialize();
        assert_eq!(b[0], 9);
        assert_eq!(u64::from_be_bytes(b[1..9].try_into().unwrap()), 7);
        assert_eq!(&b[14..], b"hi");
        assert_eq!(
            ControlMessage::SetDisplayPower { on: true }.serialize(),
            vec![10, 1]
        );
        assert_eq!(
            ControlMessage::BackOrScreenOn { action: 0 }.serialize(),
            vec![4, 0]
        );
    }
}
