//! GPU-side dispatch arguments for a requested render-target capture.
use std::io::Write;
use std::sync::{LazyLock, Mutex};

const LIMIT: u32 = 512;
static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    ["VICTORY_CAPTURE_MESH_ARGS", "VICTORY_CAPTURE_TARGET_COMMANDS"]
        .iter().all(|key| std::env::var_os(key).is_some_and(|v| v == "1"))
});
static IDS: Mutex<(u64, u32)> = Mutex::new((0, 0));

#[derive(Clone, Copy)]
pub(crate) struct Command {
    pub frame: u64,
    pub id: u32,
}

pub(crate) fn requested(frame: u64) -> bool {
    *ENABLED && super::folder().is_some() && super::target_frame(frame)
}

pub(crate) fn reserve(frame: u64) -> Option<Command> {
    let mut state = IDS.lock().unwrap_or_else(|e| e.into_inner());
    if frame < state.0 {
        return None;
    }
    if frame != state.0 {
        *state = (frame, 0);
    }
    if state.1 == LIMIT {
        return None;
    }
    state.1 += 1;
    Some(Command { frame, id: state.1 })
}

pub(crate) struct Source {
    pub command: Command,
    pub guest: u64,
    pub source_gpu: u64,
    pub shadow: u64,
    pub bounded_gpu: u64,
    pub pipeline: String,
    pub count_guest: Option<u64>,
}

impl Source {
    pub(crate) fn write(&self, list: usize, xyz: [u32; 3]) -> std::io::Result<()> {
        let Some(folder) = super::folder() else { return Ok(()) };
        static OUTPUT: Mutex<()> = Mutex::new(());
        let _output = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
        let path = folder.join(format!("frame-{}-mesh-args.tsv", self.command.frame));
        let mut file = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
        if file.metadata()?.len() == 0 {
            writeln!(file, "id\tlist\tguest_args\tshadow\tsource_gpu\tbounded_gpu\tcount_guest\tbounded_x\tbounded_y\tbounded_z\tpipeline")?;
        }
        let count = self.count_guest.map_or_else(|| "-".to_owned(), |v| format!("{v:#x}"));
        writeln!(file, "{}\t{list:#x}\t{:#x}\t{}\t{:#x}\t{:#x}\t{count}\t{}\t{}\t{}\t{}",
            self.command.id, self.guest, self.shadow, self.source_gpu, self.bounded_gpu, xyz[0], xyz[1], xyz[2], self.pipeline)
    }
}
