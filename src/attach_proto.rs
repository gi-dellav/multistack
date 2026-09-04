//! Wire protocol for `--attach` (feature `attach`).
//!
//! Two directions over a `tokio::net::UnixStream`:
//!
//! * C→S: [`ClientMsg`] — key/resize/paste input plus an initial `Hello`.
//! * S→C: [`ServerMsg`] — raw terminal byte frames plus `Evicted`/`Bye`.
//!
//! Framing is `u32 LE length + payload` where the first payload byte is a tag.
//! Encoding is manual (no serde) to avoid new dependencies. `KeyEvent`
//! encoding covers every `crossterm 0.29` `KeyCode` variant.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode, ModifierKeyCode,
};

/// Client → server message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Key(KeyEvent),
    Resize {
        cols: u16,
        rows: u16,
    },
    Paste(String),
    /// First message a client sends: its terminal size.
    Hello {
        cols: u16,
        rows: u16,
    },
}

/// Server → client message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    /// Raw terminal bytes the client must write verbatim to stdout.
    Frame(Vec<u8>),
    /// A newer client took this client's slot; disconnect.
    Evicted,
    /// Server is shutting down; disconnect.
    Bye,
}

// ---- KeyCode discriminants ----

const KC_BACKSPACE: u8 = 0;
const KC_ENTER: u8 = 1;
const KC_LEFT: u8 = 2;
const KC_RIGHT: u8 = 3;
const KC_UP: u8 = 4;
const KC_DOWN: u8 = 5;
const KC_HOME: u8 = 6;
const KC_END: u8 = 7;
const KC_PAGEUP: u8 = 8;
const KC_PAGEDOWN: u8 = 9;
const KC_TAB: u8 = 10;
const KC_BACKTAB: u8 = 11;
const KC_DELETE: u8 = 12;
const KC_INSERT: u8 = 13;
const KC_F: u8 = 14;
const KC_CHAR: u8 = 15;
const KC_NULL: u8 = 16;
const KC_ESC: u8 = 17;
const KC_CAPSLOCK: u8 = 18;
const KC_SCROLLLOCK: u8 = 19;
const KC_NUMLOCK: u8 = 20;
const KC_PRINTSCREEN: u8 = 21;
const KC_PAUSE: u8 = 22;
const KC_MENU: u8 = 23;
const KC_KEYPADBEGIN: u8 = 24;
const KC_MEDIA: u8 = 25;
const KC_MODIFIER: u8 = 26;

fn media_to_u8(m: MediaKeyCode) -> u8 {
    match m {
        MediaKeyCode::Play => 0,
        MediaKeyCode::Pause => 1,
        MediaKeyCode::PlayPause => 2,
        MediaKeyCode::Reverse => 3,
        MediaKeyCode::Stop => 4,
        MediaKeyCode::FastForward => 5,
        MediaKeyCode::Rewind => 6,
        MediaKeyCode::TrackNext => 7,
        MediaKeyCode::TrackPrevious => 8,
        MediaKeyCode::Record => 9,
        MediaKeyCode::LowerVolume => 10,
        MediaKeyCode::RaiseVolume => 11,
        MediaKeyCode::MuteVolume => 12,
    }
}

fn u8_to_media(v: u8) -> Option<MediaKeyCode> {
    match v {
        0 => Some(MediaKeyCode::Play),
        1 => Some(MediaKeyCode::Pause),
        2 => Some(MediaKeyCode::PlayPause),
        3 => Some(MediaKeyCode::Reverse),
        4 => Some(MediaKeyCode::Stop),
        5 => Some(MediaKeyCode::FastForward),
        6 => Some(MediaKeyCode::Rewind),
        7 => Some(MediaKeyCode::TrackNext),
        8 => Some(MediaKeyCode::TrackPrevious),
        9 => Some(MediaKeyCode::Record),
        10 => Some(MediaKeyCode::LowerVolume),
        11 => Some(MediaKeyCode::RaiseVolume),
        12 => Some(MediaKeyCode::MuteVolume),
        _ => None,
    }
}

fn modifier_key_to_u8(m: ModifierKeyCode) -> u8 {
    match m {
        ModifierKeyCode::LeftShift => 0,
        ModifierKeyCode::LeftControl => 1,
        ModifierKeyCode::LeftAlt => 2,
        ModifierKeyCode::LeftSuper => 3,
        ModifierKeyCode::LeftHyper => 4,
        ModifierKeyCode::LeftMeta => 5,
        ModifierKeyCode::RightShift => 6,
        ModifierKeyCode::RightControl => 7,
        ModifierKeyCode::RightAlt => 8,
        ModifierKeyCode::RightSuper => 9,
        ModifierKeyCode::RightHyper => 10,
        ModifierKeyCode::RightMeta => 11,
        ModifierKeyCode::IsoLevel3Shift => 12,
        ModifierKeyCode::IsoLevel5Shift => 13,
    }
}

fn u8_to_modifier_key(v: u8) -> Option<ModifierKeyCode> {
    match v {
        0 => Some(ModifierKeyCode::LeftShift),
        1 => Some(ModifierKeyCode::LeftControl),
        2 => Some(ModifierKeyCode::LeftAlt),
        3 => Some(ModifierKeyCode::LeftSuper),
        4 => Some(ModifierKeyCode::LeftHyper),
        5 => Some(ModifierKeyCode::LeftMeta),
        6 => Some(ModifierKeyCode::RightShift),
        7 => Some(ModifierKeyCode::RightControl),
        8 => Some(ModifierKeyCode::RightAlt),
        9 => Some(ModifierKeyCode::RightSuper),
        10 => Some(ModifierKeyCode::RightHyper),
        11 => Some(ModifierKeyCode::RightMeta),
        12 => Some(ModifierKeyCode::IsoLevel3Shift),
        13 => Some(ModifierKeyCode::IsoLevel5Shift),
        _ => None,
    }
}

fn encode_keycode(out: &mut Vec<u8>, code: &KeyCode) {
    match code {
        KeyCode::Backspace => out.push(KC_BACKSPACE),
        KeyCode::Enter => out.push(KC_ENTER),
        KeyCode::Left => out.push(KC_LEFT),
        KeyCode::Right => out.push(KC_RIGHT),
        KeyCode::Up => out.push(KC_UP),
        KeyCode::Down => out.push(KC_DOWN),
        KeyCode::Home => out.push(KC_HOME),
        KeyCode::End => out.push(KC_END),
        KeyCode::PageUp => out.push(KC_PAGEUP),
        KeyCode::PageDown => out.push(KC_PAGEDOWN),
        KeyCode::Tab => out.push(KC_TAB),
        KeyCode::BackTab => out.push(KC_BACKTAB),
        KeyCode::Delete => out.push(KC_DELETE),
        KeyCode::Insert => out.push(KC_INSERT),
        KeyCode::F(n) => {
            out.push(KC_F);
            out.push(*n);
        }
        KeyCode::Char(c) => {
            out.push(KC_CHAR);
            out.extend_from_slice(&(*c as u32).to_le_bytes());
        }
        KeyCode::Null => out.push(KC_NULL),
        KeyCode::Esc => out.push(KC_ESC),
        KeyCode::CapsLock => out.push(KC_CAPSLOCK),
        KeyCode::ScrollLock => out.push(KC_SCROLLLOCK),
        KeyCode::NumLock => out.push(KC_NUMLOCK),
        KeyCode::PrintScreen => out.push(KC_PRINTSCREEN),
        KeyCode::Pause => out.push(KC_PAUSE),
        KeyCode::Menu => out.push(KC_MENU),
        KeyCode::KeypadBegin => out.push(KC_KEYPADBEGIN),
        KeyCode::Media(m) => {
            out.push(KC_MEDIA);
            out.push(media_to_u8(*m));
        }
        KeyCode::Modifier(m) => {
            out.push(KC_MODIFIER);
            out.push(modifier_key_to_u8(*m));
        }
    }
}

fn decode_keycode(buf: &[u8], pos: &mut usize) -> Option<KeyCode> {
    let d = *buf.get(*pos)?;
    *pos += 1;
    match d {
        KC_BACKSPACE => Some(KeyCode::Backspace),
        KC_ENTER => Some(KeyCode::Enter),
        KC_LEFT => Some(KeyCode::Left),
        KC_RIGHT => Some(KeyCode::Right),
        KC_UP => Some(KeyCode::Up),
        KC_DOWN => Some(KeyCode::Down),
        KC_HOME => Some(KeyCode::Home),
        KC_END => Some(KeyCode::End),
        KC_PAGEUP => Some(KeyCode::PageUp),
        KC_PAGEDOWN => Some(KeyCode::PageDown),
        KC_TAB => Some(KeyCode::Tab),
        KC_BACKTAB => Some(KeyCode::BackTab),
        KC_DELETE => Some(KeyCode::Delete),
        KC_INSERT => Some(KeyCode::Insert),
        KC_F => Some(KeyCode::F(*buf.get(*pos)?).tap_advance(pos)),
        KC_CHAR => {
            if *pos + 4 > buf.len() {
                return None;
            }
            let v = u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
            *pos += 4;
            Some(KeyCode::Char(char::from_u32(v)?))
        }
        KC_NULL => Some(KeyCode::Null),
        KC_ESC => Some(KeyCode::Esc),
        KC_CAPSLOCK => Some(KeyCode::CapsLock),
        KC_SCROLLLOCK => Some(KeyCode::ScrollLock),
        KC_NUMLOCK => Some(KeyCode::NumLock),
        KC_PRINTSCREEN => Some(KeyCode::PrintScreen),
        KC_PAUSE => Some(KeyCode::Pause),
        KC_MENU => Some(KeyCode::Menu),
        KC_KEYPADBEGIN => Some(KeyCode::KeypadBegin),
        KC_MEDIA => {
            let v = *buf.get(*pos)?;
            *pos += 1;
            Some(KeyCode::Media(u8_to_media(v)?))
        }
        KC_MODIFIER => {
            let v = *buf.get(*pos)?;
            *pos += 1;
            Some(KeyCode::Modifier(u8_to_modifier_key(v)?))
        }
        _ => None,
    }
}

trait TapAdvance {
    fn tap_advance(self, pos: &mut usize) -> Self;
}

impl TapAdvance for KeyCode {
    fn tap_advance(self, pos: &mut usize) -> Self {
        *pos += 1;
        self
    }
}

fn kind_to_u8(k: KeyEventKind) -> u8 {
    match k {
        KeyEventKind::Press => 0,
        KeyEventKind::Repeat => 1,
        KeyEventKind::Release => 2,
    }
}

fn u8_to_kind(v: u8) -> Option<KeyEventKind> {
    match v {
        0 => Some(KeyEventKind::Press),
        1 => Some(KeyEventKind::Repeat),
        2 => Some(KeyEventKind::Release),
        _ => None,
    }
}

fn encode_key(out: &mut Vec<u8>, key: &KeyEvent) {
    encode_keycode(out, &key.code);
    out.push(key.modifiers.bits());
    out.push(kind_to_u8(key.kind));
    out.push(key.state.bits());
}

fn decode_key(buf: &[u8], pos: &mut usize) -> Option<KeyEvent> {
    let code = decode_keycode(buf, pos)?;
    let modifiers = KeyModifiers::from_bits(*buf.get(*pos)?)?;
    *pos += 1;
    let kind = u8_to_kind(*buf.get(*pos)?)?;
    *pos += 1;
    let state = KeyEventState::from_bits(*buf.get(*pos)?)?;
    *pos += 1;
    Some(KeyEvent::new_with_kind_and_state(
        code, modifiers, kind, state,
    ))
}

// ---- ClientMsg ----

const C_KEY: u8 = 0x01;
const C_RESIZE: u8 = 0x02;
const C_PASTE: u8 = 0x03;
const C_HELLO: u8 = 0x04;

impl ClientMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ClientMsg::Key(k) => {
                out.push(C_KEY);
                encode_key(&mut out, k);
            }
            ClientMsg::Resize { cols, rows } => {
                out.push(C_RESIZE);
                out.extend_from_slice(&cols.to_le_bytes());
                out.extend_from_slice(&rows.to_le_bytes());
            }
            ClientMsg::Paste(s) => {
                out.push(C_PASTE);
                out.extend_from_slice(s.as_bytes());
            }
            ClientMsg::Hello { cols, rows } => {
                out.push(C_HELLO);
                out.extend_from_slice(&cols.to_le_bytes());
                out.extend_from_slice(&rows.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        let (&tag, rest) = buf.split_first()?;
        match tag {
            C_KEY => {
                let mut pos = 0;
                let key = decode_key(rest, &mut pos)?;
                if pos != rest.len() {
                    return None;
                }
                Some(ClientMsg::Key(key))
            }
            C_RESIZE => {
                if rest.len() != 4 {
                    return None;
                }
                let cols = u16::from_le_bytes([rest[0], rest[1]]);
                let rows = u16::from_le_bytes([rest[2], rest[3]]);
                Some(ClientMsg::Resize { cols, rows })
            }
            C_PASTE => Some(ClientMsg::Paste(
                std::str::from_utf8(rest).ok()?.to_string(),
            )),
            C_HELLO => {
                if rest.len() != 4 {
                    return None;
                }
                let cols = u16::from_le_bytes([rest[0], rest[1]]);
                let rows = u16::from_le_bytes([rest[2], rest[3]]);
                Some(ClientMsg::Hello { cols, rows })
            }
            _ => None,
        }
    }
}

// ---- ServerMsg ----

const S_FRAME: u8 = 0x01;
const S_EVICTED: u8 = 0x02;
const S_BYE: u8 = 0x03;

impl ServerMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ServerMsg::Frame(b) => {
                out.push(S_FRAME);
                out.extend_from_slice(b);
            }
            ServerMsg::Evicted => out.push(S_EVICTED),
            ServerMsg::Bye => out.push(S_BYE),
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        let (&tag, rest) = buf.split_first()?;
        match tag {
            S_FRAME => Some(ServerMsg::Frame(rest.to_vec())),
            S_EVICTED => {
                if rest.is_empty() {
                    Some(ServerMsg::Evicted)
                } else {
                    None
                }
            }
            S_BYE => {
                if rest.is_empty() {
                    Some(ServerMsg::Bye)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

// ---- Framing helpers (tokio) ----

/// Max single message: 4 MiB. Frames are full-screen repaints (tens of KB);
/// anything larger is a corrupt length prefix — drop the peer.
pub const MAX_MSG_LEN: u32 = 4 * 1024 * 1024;

pub async fn write_msg<S>(stream: &mut S, payload: &[u8]) -> std::io::Result<()>
where
    S: tokio::io::AsyncWriteExt + Unpin,
{
    let len = u32::try_from(payload.len()).map_err(std::io::Error::other)?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

pub async fn read_msg<S>(stream: &mut S) -> std::io::Result<Option<Vec<u8>>>
where
    S: tokio::io::AsyncReadExt + Unpin,
{
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MSG_LEN {
        return Err(std::io::Error::other("attach message too large"));
    }
    let mut buf = vec![0u8; len as usize];
    match stream.read_exact(&mut buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn kc(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn key_roundtrip_plain() {
        for code in [
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Null,
            KeyCode::Esc,
            KeyCode::CapsLock,
            KeyCode::ScrollLock,
            KeyCode::NumLock,
            KeyCode::PrintScreen,
            KeyCode::Pause,
            KeyCode::Menu,
            KeyCode::KeypadBegin,
        ] {
            let msg = ClientMsg::Key(kc(code));
            assert_eq!(ClientMsg::decode(&msg.encode()), Some(msg), "{code:?}");
        }
    }

    #[test]
    fn key_roundtrip_payload() {
        let cases = vec![
            kc(KeyCode::F(1)),
            kc(KeyCode::F(12)),
            kc(KeyCode::Char('a')),
            kc(KeyCode::Char('é')),
            kc(KeyCode::Media(MediaKeyCode::Play)),
            kc(KeyCode::Media(MediaKeyCode::MuteVolume)),
            kc(KeyCode::Modifier(ModifierKeyCode::LeftShift)),
            kc(KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift)),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release),
        ];
        for key in cases {
            let msg = ClientMsg::Key(key);
            assert_eq!(ClientMsg::decode(&msg.encode()), Some(msg), "{key:?}");
        }
    }

    #[test]
    fn resize_hello_paste_roundtrip() {
        let m = ClientMsg::Resize { cols: 80, rows: 24 };
        assert_eq!(ClientMsg::decode(&m.encode()), Some(m));
        let m = ClientMsg::Hello { cols: 0, rows: 0 };
        assert_eq!(ClientMsg::decode(&m.encode()), Some(m));
        let m = ClientMsg::Paste("hello wörld".to_string());
        assert_eq!(ClientMsg::decode(&m.encode()), Some(m));
        let m = ClientMsg::Paste(String::new());
        assert_eq!(ClientMsg::decode(&m.encode()), Some(m));
    }

    #[test]
    fn server_msg_roundtrip() {
        let m = ServerMsg::Frame(vec![0x1b, b'[', b'H', 1, 2, 3]);
        assert_eq!(ServerMsg::decode(&m.encode()), Some(m));
        let m = ServerMsg::Frame(vec![]);
        assert_eq!(ServerMsg::decode(&m.encode()), Some(m));
        assert_eq!(
            ServerMsg::decode(&ServerMsg::Evicted.encode()),
            Some(ServerMsg::Evicted)
        );
        assert_eq!(
            ServerMsg::decode(&ServerMsg::Bye.encode()),
            Some(ServerMsg::Bye)
        );
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(ClientMsg::decode(&[]), None);
        assert_eq!(ClientMsg::decode(&[0x99]), None);
        assert_eq!(ClientMsg::decode(&[C_KEY, 0xFF]), None);
        assert_eq!(ClientMsg::decode(&[C_RESIZE, 1, 2]), None);
        assert_eq!(ClientMsg::decode(&[C_HELLO, 1, 2, 3]), None);
        assert_eq!(ClientMsg::decode(&[C_PASTE, 0xFF, 0xFE]), None);
        assert_eq!(ServerMsg::decode(&[]), None);
        assert_eq!(ServerMsg::decode(&[0x99]), None);
        assert_eq!(ServerMsg::decode(&[S_EVICTED, 0x00]), None);
    }
}
