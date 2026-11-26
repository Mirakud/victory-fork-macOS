//! CPU records for mesh commands on lists whose relay fence is slow.
//! The caller retains the native command list and its PSOs for the flight.

use std::collections::BTreeMap;
use std::sync::OnceLock;

const MAX_RUNS: usize = 1024;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("VICTORY_TRACE_STALLED_MESH").is_some_and(|v| v == "1"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mesh {
    Direct {
        x: u32,
    },
    Indirect {
        arguments: u64,
        max: u32,
        count: Option<u64>,
    },
}

impl std::fmt::Display for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct { x } => write!(f, "DispatchMesh({x},1,1)"),
            Self::Indirect {
                arguments,
                max,
                count,
            } => {
                write!(f, "ExecuteIndirect(mesh,args={arguments:#x},max={max}")?;
                if let Some(count) = count {
                    write!(f, ",count={count:#x}")?;
                }
                write!(f, ")")
            }
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct Records {
    shaders: BTreeMap<usize, String>,
    runs: Vec<(usize, Mesh, u64)>,
    calls: u64,
    omitted: u64,
}

impl Records {
    pub(super) fn bind(&mut self, pso: usize, shaders: &str) {
        if self.shaders.len() < MAX_RUNS {
            self.shaders
                .entry(pso)
                .or_insert_with(|| shaders.to_owned());
        }
    }

    pub(super) fn record(&mut self, pso: usize, command: Mesh) {
        self.calls += 1;
        if self.omitted == 0 {
            if let Some((last_pso, last_command, count)) = self.runs.last_mut()
                && (*last_pso, *last_command) == (pso, command)
            {
                *count += 1;
                return;
            }
            if self.runs.len() < MAX_RUNS {
                self.runs.push((pso, command, 1));
                return;
            }
        }
        self.omitted += 1;
    }
}

pub(super) struct NativeList {
    id: usize,
    role: &'static str,
    host: Option<usize>,
    records: Option<std::sync::Arc<Records>>,
}

impl NativeList {
    pub(super) fn new(
        id: usize,
        role: &'static str,
        host: Option<usize>,
        records: Option<std::sync::Arc<Records>>,
    ) -> Self {
        Self {
            id,
            role,
            host,
            records,
        }
    }

    #[cfg(windows)]
    pub(super) fn log(&self, submission: u64, fence: u64, list_index: usize) {
        let records = self.records.as_deref();
        tracing::warn!(target: "victory_gpu::host", submission, fence, list_index,
            list = format_args!("{:#x}", self.id), role = self.role,
            host_list = format_args!("{:#x}", self.host.unwrap_or(0)),
            calls = records.map_or(0, |r| r.calls), omitted = records.map_or(0, |r| r.omitted),
            "stalled relay mesh list");
        let Some(records) = records else { return };
        for (pso, shaders) in &records.shaders {
            tracing::warn!(target: "victory_gpu::host", submission, list_index,
                pso = format_args!("{pso:#x}"), %shaders, "stalled relay mesh pipeline bound");
        }
        let mut first = 1;
        for (pso, command, count) in &records.runs {
            tracing::warn!(target: "victory_gpu::host", submission, list_index, first, count,
                pso = format_args!("{pso:#x}"), %command, "stalled relay mesh command recorded");
            first += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_require_matching_pso_kind_and_arguments() {
        let mut r = Records::default();
        let direct = Mesh::Direct { x: 12 };
        let indirect = Mesh::Indirect {
            arguments: 42,
            max: 7,
            count: None,
        };
        for (pso, cmd) in [
            (5, direct),
            (5, direct),
            (8, direct),
            (8, indirect),
            (8, direct),
        ] {
            r.record(pso, cmd);
        }
        assert_eq!(r.calls, 5);
        assert_eq!(
            r.runs,
            [
                (5, direct, 2),
                (8, direct, 1),
                (8, indirect, 1),
                (8, direct, 1)
            ]
        );
        assert_eq!(r.omitted, 0);
    }

    #[test]
    fn submitted_lists_keep_their_shaders_and_dimensions_after_reuse() {
        use std::sync::Arc;
        let mut live = Arc::<Records>::default();
        Arc::make_mut(&mut live).bind(4, "[mesh old, pixel old]");
        Arc::make_mut(&mut live).record(4, Mesh::Direct { x: 3 });
        let submitted = NativeList::new(0x200, "body", Some(0x400), Some(live.clone()));
        Arc::make_mut(&mut live).record(4, Mesh::Direct { x: 9 });
        drop(live);
        let records = submitted.records.as_ref().unwrap();
        assert_eq!(records.shaders[&4], "[mesh old, pixel old]");
        assert_eq!(records.runs, [(4, Mesh::Direct { x: 3 }, 1)]);
        assert_eq!(
            (submitted.id, submitted.role, submitted.host),
            (0x200, "body", Some(0x400))
        );
    }

    #[test]
    fn truncated_suffix_never_merges_into_the_retained_prefix() {
        let mut r = Records::default();
        for i in 0..MAX_RUNS + 2 {
            r.record(1, Mesh::Direct { x: i as u32 });
        }
        r.record(
            1,
            Mesh::Direct {
                x: MAX_RUNS as u32 - 1,
            },
        );
        assert_eq!(r.runs.len(), MAX_RUNS);
        assert_eq!(r.runs.last().unwrap().2, 1);
        assert_eq!(r.omitted, 3);
        assert_eq!(r.calls, MAX_RUNS as u64 + 3);
    }
}
