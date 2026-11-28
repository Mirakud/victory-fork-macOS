//! Foreground, physical game controls sampled by the title's input poll.
//! A launcher can subscribe on a per-run UDP loopback port.
use super::{FALLBACK_BUTTONS, PadState, fallback_buttons};
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

const HEARTBEAT: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq)]
struct Sample {
    source: &'static str,
    focus: bool,
    keys: Vec<u8>,
    buttons: u32,
    axes: [i8; 6],
}

impl Sample {
    fn new(focus: bool, held: &[u8], pad: Option<(&'static str, PadState)>) -> Self {
        let keys = if focus {
            held.iter()
                .copied()
                .filter(|vk| {
                    b"WQSDIJKLZC".contains(vk) || FALLBACK_BUTTONS.iter().any(|(key, _)| key == vk)
                })
                .collect()
        } else {
            Vec::new()
        };
        let (source, state) =
            pad.unwrap_or_else(|| ("keyboard", fallback_buttons(|vk| keys.contains(&vk))));
        let axes = [
            state.left_x,
            state.left_y,
            state.right_x,
            state.right_y,
            state.left_trigger,
            state.right_trigger,
        ]
        .map(|axis| (axis * 100.).round() as i8);
        Self {
            source,
            focus,
            keys,
            buttons: state.buttons,
            axes,
        }
    }

    fn json(&self) -> String {
        format!(
            r#"{{"v":1,"source":"{}","focus":{},"keys":{:?},"buttons":{},"axes":{:?}}}"#,
            self.source, self.focus, self.keys, self.buttons, self.axes
        )
    }
}

struct Sender {
    socket: UdpSocket,
    last: Option<(Sample, Instant)>,
}

impl Sender {
    fn new(port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        socket.set_nonblocking(true)?;
        socket.connect((Ipv4Addr::LOCALHOST, port))?;
        Ok(Self { socket, last: None })
    }

    fn publish(&mut self, sample: Sample, now: Instant) {
        if self
            .last
            .as_ref()
            .is_some_and(|(sent, at)| *sent == sample && now.duration_since(*at) < HEARTBEAT)
        {
            return;
        }
        // Treat telemetry as best effort, also backing off after socket errors.
        let _ = self.socket.send(sample.json().as_bytes());
        self.last = Some((sample, now));
    }
}

pub(super) struct InputHud {
    pub(super) pad: Option<(&'static str, PadState)>,
    sender: Option<Sender>,
}

impl Default for InputHud {
    fn default() -> Self {
        let sender = std::env::var("VICTORY_INPUT_HUD_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|&port| port != 0)
            .and_then(|port| Sender::new(port).ok());
        Self { pad: None, sender }
    }
}

impl InputHud {
    pub(super) fn publish(&mut self, focus: bool, physical_keys: &[u8]) {
        if let Some(sender) = self.sender.as_mut() {
            sender.publish(Sample::new(focus, physical_keys, self.pad), Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::gameinput::{A, MENU};

    #[test]
    fn keyboard_fallback_reports_mapped_keys_and_releases_on_focus_loss() {
        let sample = Sample::new(true, &[13, 27, b'W', b'L', b'Z', b'T'], None);
        assert_eq!(sample.source, "keyboard");
        assert_eq!(sample.keys, [13, 27, b'W', b'L', b'Z']);
        assert_eq!(sample.buttons, A | MENU);
        assert_eq!(sample.axes, [0, 100, 100, 0, 100, 0]);
        let blurred = Sample::new(false, &[13, b'W'], None);
        assert!(blurred.keys.is_empty());
        assert_eq!((blurred.buttons, blurred.axes), (0, [0; 6]));
    }

    #[test]
    fn physical_pad_takes_priority_and_only_keys_are_hidden_when_blurred() {
        let pad = PadState {
            buttons: A,
            left_x: -0.49,
            right_trigger: 0.99,
            ..Default::default()
        };
        for source in ["xinput", "dualsense"] {
            let sample = Sample::new(true, &[27], Some((source, pad)));
            assert_eq!((sample.source, sample.buttons), (source, A));
            assert_eq!(sample.keys, [27]);
            assert_eq!(sample.axes, [-49, 0, 0, 0, 0, 99]);
            let blurred = Sample::new(false, &[27], Some((source, pad)));
            assert_eq!(blurred.source, source);
            assert!(blurred.keys.is_empty());
            assert_eq!((blurred.buttons, blurred.axes), (A, [-49, 0, 0, 0, 0, 99]));
        }
    }

    #[test]
    fn loopback_sends_changes_immediately_and_repeats_held_state() {
        let listener = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut sender = Sender::new(listener.local_addr().unwrap().port()).unwrap();
        let now = Instant::now();
        let sample = |held| Sample::new(true, held, None);
        sender.publish(sample(&[13]), now);
        sender.publish(sample(&[13]), now + Duration::from_millis(8));
        sender.publish(sample(&[]), now + Duration::from_millis(16));
        sender.publish(sample(&[]), now + Duration::from_millis(266));

        let mut buffer = [0; 512];
        for expected in [sample(&[13]), sample(&[]), sample(&[])] {
            let length = listener.recv(&mut buffer).unwrap();
            assert_eq!(&buffer[..length], expected.json().as_bytes());
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.recv(&mut buffer).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
