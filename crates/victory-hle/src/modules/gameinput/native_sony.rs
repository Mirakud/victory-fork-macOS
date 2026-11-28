//! macOS GameController's per-run, loopback DualSense reader. The native
//! helper samples the physical controls in response to each request.
use super::PadState;
use std::net::{Ipv4Addr, UdpSocket};
use std::time::Instant;

const MAX_AGE_US: u64 = 400_000;

pub(super) struct NativeSony {
    socket: UdpSocket,
    started: Instant,
    last: Option<(u64, Option<PadState>)>,
}

impl NativeSony {
    pub(super) fn from_env() -> Option<Self> {
        let port: u16 = std::env::var("VICTORY_DUALSENSE_NATIVE_PORT")
            .ok()?
            .parse()
            .ok()?;
        Self::new(port)
    }

    fn new(port: u16) -> Option<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
        socket.connect((Ipv4Addr::LOCALHOST, port)).ok()?;
        socket.set_nonblocking(true).ok()?;
        Some(Self {
            socket,
            started: Instant::now(),
            last: None,
        })
    }

    pub(super) fn read(&mut self) -> Option<PadState> {
        let stamp = self.started.elapsed().as_micros() as u64;
        let mut request = *b"DSQ1\0\0\0\0\0\0\0\0";
        request[4..].copy_from_slice(&stamp.to_le_bytes());
        let _ = self.socket.send(&request);
        let mut buf = [0; 64];
        for _ in 0..64 {
            let Ok(n) = self.socket.recv(&mut buf) else {
                break;
            };
            if let Some((at, pad)) = decode(&buf[..n], stamp)
                && self.last.is_none_or(|previous| at >= previous.0)
            {
                self.last = Some((at, pad));
            }
        }
        self.last
            .filter(|(at, _)| stamp.saturating_sub(*at) <= MAX_AGE_US)?
            .1
    }
}

fn decode(reply: &[u8], now: u64) -> Option<(u64, Option<PadState>)> {
    if reply.len() != 27 || &reply[..4] != b"DSR1" {
        return None;
    }
    let at = u64::from_le_bytes(reply[4..12].try_into().ok()?);
    if now.checked_sub(at)? > MAX_AGE_US {
        return None;
    }
    match reply[12] {
        0 => Some((at, None)),
        1 => {
            let buttons = u16::from_le_bytes(reply[13..15].try_into().ok()?);
            if buttons & !0x3fff != 0 {
                return None;
            }
            let axis = |n: usize| {
                i16::from_le_bytes([reply[15 + n * 2], reply[16 + n * 2]]) as f32 / 32767.
            };
            let pad = PadState {
                buttons: u32::from(buttons),
                left_x: axis(0),
                left_y: axis(1),
                right_x: axis(2),
                right_y: axis(3),
                left_trigger: axis(4),
                right_trigger: axis(5),
            };
            if pad.left_trigger < 0. || pad.right_trigger < 0. {
                return None;
            }
            Some((at, Some(pad)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_pad_verifies_buttons_axes_absence_and_age() {
        let mut bytes = [0; 27];
        bytes[..4].copy_from_slice(b"DSR1");
        bytes[4..12].copy_from_slice(&17u64.to_le_bytes());
        bytes[12] = 1;
        bytes[13..15].copy_from_slice(&((1u16 << 2) | (1u16 << 10)).to_le_bytes());
        bytes[15..17].copy_from_slice(&(-32767i16).to_le_bytes());
        bytes[25..27].copy_from_slice(&32767i16.to_le_bytes());
        let pad = decode(&bytes, 18).unwrap().1.unwrap();
        assert_eq!(
            (pad.buttons, pad.left_x, pad.right_trigger),
            (1028, -1., 1.)
        );
        assert!(decode(&bytes, 16).is_none());
        assert!(decode(&bytes, 400018).is_none());
        bytes[12] = 0;
        assert_eq!(decode(&bytes, 50), Some((17, None)));
    }
}
