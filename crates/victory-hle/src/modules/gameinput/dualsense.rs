//! A DualSense on Wine's raw HID bus is visible through WinMM/DirectInput.
//! Wine assigns X/Y and Z/R to the two sticks, V to L2, and U to R2.

use super::{
    A, B, DOWN, LEFT, LEFT_SHOULDER, LEFT_STICK, MENU, PadState, RIGHT, RIGHT_SHOULDER,
    RIGHT_STICK, UP, VIEW, X, Y,
};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Media::Multimedia::{
    JOY_RETURNBUTTONS, JOY_RETURNPOV, JOY_RETURNPOVCTS, JOY_RETURNR, JOY_RETURNU, JOY_RETURNV,
    JOY_RETURNX, JOY_RETURNY, JOY_RETURNZ, JOYCAPSW, JOYERR_NOERROR, JOYINFOEX, joyGetDevCapsW,
    joyGetNumDevs, joyGetPosEx,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

static WINE: LazyLock<bool> = LazyLock::new(|| {
    // SAFETY: ntdll is loaded for the lifetime of a Windows process; the symbol is only queried.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
    }
});

// WinMM enumeration can block indefinitely in joyGetDevCapsW on Wine.
// Keep keyboard, XInput, device callbacks and HUD heartbeats on the ordinary poll.
#[derive(Default)]
pub(super) struct Reader {
    worker: OnceLock<Option<Worker>>,
}

impl Reader {
    pub(super) fn read(&mut self) -> Option<PadState> {
        if !*WINE {
            return None;
        }
        self.worker.get_or_init(|| {
            if std::env::var_os("VICTORY_DUALSENSE").is_some_and(|value| value == "0") {
                return None;
            }
            let mut presence = std::env::var_os("VICTORY_DUALSENSE_PRESENT_FILE")
                .filter(|path| !path.is_empty())
                .map(|path| HardwarePresence::new(path.into()));
            let mut device = Device::default();
            Worker::start(move || device.read(presence.as_mut()))
        });
        self.worker.get_mut()?.as_mut()?.poll(Instant::now())
    }
}

struct Worker {
    request: SyncSender<()>,
    answer: Receiver<(Instant, Option<PadState>)>,
    last: Option<(Instant, PadState)>,
}

impl Worker {
    fn start(mut read: impl FnMut() -> Option<PadState> + Send + 'static) -> Option<Self> {
        let (request, pending) = sync_channel(1);
        let (reply, answer) = sync_channel(1);
        std::thread::Builder::new()
            .name("victory-dualsense".into())
            .spawn(move || {
                while pending.recv().is_ok() {
                    let state = read();
                    if reply.send((Instant::now(), state)).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self {
            request,
            answer,
            last: None,
        })
    }

    fn poll(&mut self, now: Instant) -> Option<PadState> {
        for (at, state) in self.answer.try_iter() {
            self.last = state.map(|pad| (at, pad));
        }
        let _ = self.request.try_send(());
        self.last
            .filter(|(at, _)| now.saturating_duration_since(*at) < Duration::from_millis(500))
            .map(|(_, pad)| pad)
    }
}

// The Mac launcher can report physical Sony hardware using a per-run file.
// The worker polls that file independently of the game's input thread.
struct HardwarePresence {
    path: PathBuf,
    next_check: Option<Instant>,
    connected: bool,
}

impl HardwarePresence {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            next_check: None,
            connected: false,
        }
    }

    fn connected(&mut self, now: Instant) -> bool {
        if self.next_check.is_none_or(|at| now >= at) {
            self.connected = std::fs::read(&self.path).is_ok_and(|data| data.trim_ascii() == b"1");
            self.next_check = Some(now + Duration::from_millis(500));
        }
        self.connected
    }
}

#[derive(Default)]
struct Device {
    device: Option<u32>,
    state: Option<PadState>,
    next_poll: Option<Instant>,
}

impl Device {
    fn read(&mut self, presence: Option<&mut HardwarePresence>) -> Option<PadState> {
        let now = Instant::now();
        if presence.is_some_and(|gate| !gate.connected(now)) {
            *self = Self::default();
            return None;
        }
        if self.next_poll.is_some_and(|next| now < next) {
            return self.state;
        }
        let device = self.device.or_else(|| {
            // SAFETY: the count has no parameters; each probe supplies a full writable JOYCAPSW.
            (0..unsafe { joyGetNumDevs() }).find(|&id| {
                let mut caps = JOYCAPSW::default();
                let result =
                    unsafe { joyGetDevCapsW(id as usize, &mut caps, size_of::<JOYCAPSW>() as u32) };
                result == JOYERR_NOERROR
                    && caps.wMid == 0x054c
                    && matches!(caps.wPid, 0x0ce6 | 0x0df2)
                    && caps.wNumAxes >= 6
                    && caps.wNumButtons >= 12
            })
        });
        self.state = device.and_then(|id| {
            let mut info = JOYINFOEX {
                dwSize: size_of::<JOYINFOEX>() as u32,
                dwFlags: (JOY_RETURNX
                    | JOY_RETURNY
                    | JOY_RETURNZ
                    | JOY_RETURNR
                    | JOY_RETURNU
                    | JOY_RETURNV
                    | JOY_RETURNBUTTONS
                    | JOY_RETURNPOV
                    | JOY_RETURNPOVCTS) as u32,
                ..Default::default()
            };
            // SAFETY: info is sized and initialized according to the WinMM contract.
            (unsafe { joyGetPosEx(id, &mut info) } == JOYERR_NOERROR).then(|| convert(&info))
        });
        self.device = device.filter(|_| self.state.is_some());
        let delay = if self.device.is_some() {
            Duration::from_millis(4)
        } else {
            Duration::from_secs(1)
        };
        self.next_poll = Some(now + delay);
        self.state
    }
}

fn convert(s: &JOYINFOEX) -> PadState {
    // Square, cross, circle, triangle; L1/R1; digital L2/R2 also have analog axes;
    // Create, Options, L3/R3. The game reads the analog trigger fields directly.
    const BUTTONS: [u32; 12] = [
        X,
        A,
        B,
        Y,
        LEFT_SHOULDER,
        RIGHT_SHOULDER,
        0,
        0,
        VIEW,
        MENU,
        LEFT_STICK,
        RIGHT_STICK,
    ];
    const HAT: [u32; 8] = [
        UP,
        UP | RIGHT,
        RIGHT,
        DOWN | RIGHT,
        DOWN,
        DOWN | LEFT,
        LEFT,
        UP | LEFT,
    ];
    let hat = if s.dwPOV < 36_000 {
        HAT[((s.dwPOV + 2250) / 4500 % 8) as usize]
    } else {
        0
    };
    let buttons = BUTTONS.iter().enumerate().fold(hat, |mask, (i, &button)| {
        if s.dwButtons & (1 << i) != 0 {
            mask | button
        } else {
            mask
        }
    });
    let axis = |raw: u32| ((raw as f32 - 32767.5) / 32767.5).clamp(-1.0, 1.0);
    // Wine supplies the midpoint for every axis until its first HID report arrives.
    if s.dwButtons == 0
        && hat == 0
        && [s.dwXpos, s.dwYpos, s.dwZpos, s.dwRpos, s.dwUpos, s.dwVpos] == [32767; 6]
    {
        return PadState::default();
    }
    PadState {
        buttons,
        left_x: axis(s.dwXpos),
        left_y: -axis(s.dwYpos),
        right_x: axis(s.dwZpos),
        right_y: -axis(s.dwRpos),
        left_trigger: s.dwVpos.min(65535) as f32 / 65535.0,
        right_trigger: s.dwUpos.min(65535) as f32 / 65535.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faces_shoulders_menu_and_hat_follow_gameinput_layout() {
        for (bit, expected) in [
            (0, X),
            (1, A),
            (2, B),
            (3, Y),
            (4, LEFT_SHOULDER),
            (5, RIGHT_SHOULDER),
            (6, 0),
            (7, 0),
            (8, VIEW),
            (9, MENU),
            (10, LEFT_STICK),
            (11, RIGHT_STICK),
        ] {
            assert_eq!(
                convert(&JOYINFOEX {
                    dwButtons: 1 << bit,
                    dwPOV: 65535,
                    ..Default::default()
                })
                .buttons,
                expected
            );
        }
        for (i, expected) in [
            UP,
            UP | RIGHT,
            RIGHT,
            DOWN | RIGHT,
            DOWN,
            DOWN | LEFT,
            LEFT,
            UP | LEFT,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                convert(&JOYINFOEX {
                    dwPOV: i as u32 * 4500,
                    ..Default::default()
                })
                .buttons,
                expected
            );
        }
    }

    #[test]
    fn physical_usb_samples_preserve_independent_triggers_and_sticks() {
        let mut report = JOYINFOEX {
            dwXpos: 32509,
            dwYpos: 33023,
            dwZpos: 34047,
            dwRpos: 33791,
            dwVpos: 64255,
            dwUpos: 0,
            dwButtons: 64,
            dwPOV: 65535,
            ..Default::default()
        };
        let left = convert(&report);
        assert!(
            (left.left_trigger - 64255.0 / 65535.0).abs() < 0.00001 && left.right_trigger == 0.0
        );
        report.dwUpos = 65535;
        report.dwVpos = 0;
        report.dwButtons = 128;
        let right = convert(&report);
        assert_eq!((right.left_trigger, right.right_trigger), (0.0, 1.0));
        report.dwZpos = 65535;
        report.dwRpos = 0;
        let motion = convert(&report);
        assert_eq!((motion.right_x, motion.right_y), (1., 1.));
        assert_eq!((motion.left_x, motion.left_y), (left.left_x, left.left_y));
        let first = JOYINFOEX {
            dwXpos: 32767,
            dwYpos: 32767,
            dwZpos: 32767,
            dwRpos: 32767,
            dwUpos: 32767,
            dwVpos: 32767,
            dwPOV: 65535,
            ..Default::default()
        };
        assert_eq!(convert(&first), PadState::default());
    }

    #[test]
    fn hardware_signal_filters_absence_and_accepts_hotplug_after_refresh() {
        let path = std::env::temp_dir().join(format!("victory-sony-{}.flag", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut gate = HardwarePresence::new(path.clone());
        let now = Instant::now();
        let pressed = PadState {
            buttons: A,
            ..Default::default()
        };
        let mut device = Device {
            device: Some(3),
            state: Some(pressed),
            next_poll: Some(now + Duration::from_secs(10)),
        };
        assert_eq!(device.read(None), Some(pressed));
        assert_eq!(device.read(Some(&mut gate)), None);
        assert!(device.device.is_none() && device.state.is_none() && device.next_poll.is_none());

        std::fs::write(&path, b"1\n").unwrap();
        assert!(!gate.connected(now + Duration::from_millis(20)));
        assert!(gate.connected(now + Duration::from_secs(1)));
        std::fs::write(&path, b"0").unwrap();
        assert!(!gate.connected(now + Duration::from_secs(2)));
        std::fs::write(&path, b"invalid").unwrap();
        assert!(!gate.connected(now + Duration::from_secs(3)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn blocked_winmm_detection_preserves_keyboard_polling_and_expires_held_pad() {
        use std::sync::mpsc::channel;
        let (started, entered) = channel();
        let (release, gate) = channel();
        let mut worker = Worker::start(move || {
            started.send(()).unwrap();
            gate.recv().ok().flatten()
        })
        .unwrap();
        assert!(worker.poll(Instant::now()).is_none());
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        // The simulated WinMM driver is still blocked in its first call.
        for _ in 0..100 {
            assert!(worker.poll(Instant::now()).is_none());
        }
        let pressed = PadState {
            buttons: A,
            ..Default::default()
        };
        release.send(Some(pressed)).unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let now = Instant::now();
        assert_eq!(worker.poll(now), Some(pressed));
        // A blocked later report must release the held button.
        assert_eq!(worker.poll(now + Duration::from_secs(1)), None);
        drop(worker);
        drop(release);
    }

    #[test]
    #[ignore = "requires a connected DualSense exposed via Wine WinMM"]
    fn connected_wine_dualsense() {
        let mut reader = Reader::default();
        let timeout = Instant::now() + Duration::from_secs(3);
        let first = loop {
            if let Some(pad) = reader.read() {
                break pad;
            }
            assert!(Instant::now() < timeout, "DualSense missing");
            std::thread::sleep(Duration::from_millis(8));
        };
        eprintln!("connected DualSense: {first:?}");
    }
}
