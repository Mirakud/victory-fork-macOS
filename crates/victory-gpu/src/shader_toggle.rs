//! Run-local A/B control for graphics commands. Decisions are made when recording
//! native commands. Counts are published when a list is submitted, including replays.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct GraphicsId {
    key: u128,
    pair: Option<(u64, u64)>,
}

impl GraphicsId {
    pub(crate) fn new(key: &str, first: Option<&str>, pixel: Option<&str>) -> Option<Self> {
        let key = hex(key, 32)?;
        let pair = first
            .zip(pixel)
            .and_then(|(a, b)| Some((hex(a, 16)? as u64, hex(b, 16)? as u64)));
        Some(Self { key, pair })
    }
}

fn hex(s: &str, n: usize) -> Option<u128> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u128::from_str_radix(s, 16).ok())?
}

#[derive(Default, Clone, PartialEq, Eq, Debug)]
enum Targets {
    #[default]
    None,
    Key(BTreeSet<u128>),
    Pair(BTreeSet<(u64, u64)>),
}
#[derive(Default, Clone, Debug, PartialEq, Eq)]
struct Selection {
    revision: u64,
    solo: bool,
    targets: Targets,
}

impl Selection {
    fn parse(s: &[u8]) -> Result<Self, &'static str> {
        let mut fields = std::str::from_utf8(s)
            .map_err(|_| "invalid encoding")?
            .split_ascii_whitespace();
        let revision: u64 = fields
            .next()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .ok_or("invalid revision")?;
        let mode = fields.next().ok_or("missing mode")?;
        if mode == "all" && fields.next().is_none() {
            return Ok(Self {
                revision,
                ..Self::default()
            });
        }
        let solo = match mode {
            "solo" => true,
            "hide" => false,
            _ => return Err("invalid mode"),
        };
        let kind = fields.next().ok_or("missing kind")?;
        let list = fields.next().ok_or("missing target")?;
        if fields.next().is_some() || list.split(',').count() > 32 {
            return Err("excess targets or fields");
        }
        let targets = match kind {
            "key" => Targets::Key(
                list.split(',')
                    .map(|k| hex(k, 32).ok_or("invalid pipeline key"))
                    .collect::<Result<_, _>>()?,
            ),
            "pair" => Targets::Pair(
                list.split(',')
                    .map(|p| {
                        let (a, b) = p.split_once('/').ok_or("invalid shader pair")?;
                        Ok((
                            hex(a, 16).ok_or("invalid shader pair")? as u64,
                            hex(b, 16).ok_or("invalid shader pair")? as u64,
                        ))
                    })
                    .collect::<Result<_, &'static str>>()?,
            ),
            _ => return Err("invalid kind"),
        };
        Ok(Self {
            revision,
            solo,
            targets,
        })
    }

    fn permits(&self, id: GraphicsId) -> bool {
        let hit = match &self.targets {
            Targets::None => return true,
            Targets::Key(keys) => keys.contains(&id.key),
            Targets::Pair(pairs) => id.pair.is_some_and(|pair| pairs.contains(&pair)),
        };
        hit == self.solo
    }

    fn name(&self) -> &'static str {
        if self.targets == Targets::None {
            "all"
        } else if self.solo {
            "solo"
        } else {
            "hide"
        }
    }

    fn target_text(&self) -> (&'static str, String) {
        match &self.targets {
            Targets::None => ("none", String::new()),
            Targets::Key(keys) => (
                "key",
                keys.iter()
                    .map(|k| format!("{k:032x}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Targets::Pair(pairs) => (
                "pair",
                pairs
                    .iter()
                    .map(|(a, b)| format!("{a:016x}/{b:016x}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        }
    }
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Counts {
    drawn: u64,
    skipped: u64,
}

impl Counts {
    pub(crate) fn record(&mut self, drawn: bool) {
        if drawn {
            self.drawn += 1;
        } else {
            self.skipped += 1;
        }
    }
    fn add(&mut self, other: Self) {
        self.drawn += other.drawn;
        self.skipped += other.skipped;
    }
}

pub(crate) type ListCounts = BTreeMap<GraphicsId, Counts>;

/// Owned by the list's draw state until that particular native list closes.
#[derive(Default)]
pub(crate) struct Recording(ListCounts);

impl Recording {
    pub(crate) fn record(&mut self, id: GraphicsId) -> bool {
        let allowed = permits(id);
        self.0.entry(id).or_default().record(allowed);
        allowed
    }

    pub(crate) fn take(&mut self) -> Option<ListCounts> {
        (!self.0.is_empty()).then(|| std::mem::take(&mut self.0))
    }
}

#[derive(Default)]
struct Report {
    total: Counts,
    frame: Counts,
}

#[derive(PartialEq, Eq, Debug)]
enum Request {
    Missing,
    Command(Vec<u8>),
    Error(&'static str),
}

#[derive(Default)]
struct Session {
    control: Selection,
    previous: Option<Request>,
    error: &'static str,
    timelines: BTreeMap<GraphicsId, Report>,
}

impl Session {
    fn apply(&mut self, request: Request) -> bool {
        if self.previous.as_ref() == Some(&request) {
            return false;
        }
        let parsed = match &request {
            Request::Missing => Ok(Selection::default()),
            Request::Error(reason) => Err(*reason),
            Request::Command(bytes) => Selection::parse(bytes),
        };
        self.previous = Some(request);
        let outcome = parsed.and_then(|candidate| {
            let known = match &candidate.targets {
                Targets::None => true,
                Targets::Key(keys) => keys.iter().all(|key| {
                    self.timelines
                        .iter()
                        .any(|(id, r)| id.key == *key && r.total.drawn > 0)
                }),
                Targets::Pair(pairs) => pairs.iter().all(|p| {
                    self.timelines
                        .iter()
                        .any(|(id, r)| id.pair == Some(*p) && r.total.drawn > 0)
                }),
            };
            if known {
                Ok(candidate)
            } else {
                Err("unknown target")
            }
        });
        match outcome {
            Ok(selection) => {
                self.control = selection;
                self.error = "";
            }
            Err(reason) => self.error = reason,
        }
        true
    }

    fn submit(&mut self, list: &ListCounts) {
        for (&id, &count) in list {
            let row = self.timelines.entry(id).or_default();
            row.frame.add(count);
            row.total.add(count);
        }
    }

    fn json(&self, frame: u64) -> String {
        let mut out = String::new();
        let (kind, target) = self.control.target_text();
        write!(out,r#"{{"version":1,"frame":{frame},"revision":{},"mode":"{}","kind":"{kind}","target":"{target}","error":"{}","pipelines":["#,self.control.revision,self.control.name(),self.error).unwrap();
        for (i, (id, report)) in self.timelines.iter().enumerate() {
            if i != 0 {
                out.push(',');
            }
            let pair = id
                .pair
                .map_or(String::new(), |(a, b)| format!("{a:016x}/{b:016x}"));
            write!(out,r#"{{"key":"{:032x}","pair":"{pair}","drawn":{},"skipped":{},"frame_drawn":{},"frame_skipped":{}}}"#,id.key,report.total.drawn,report.total.skipped,report.frame.drawn,report.frame.skipped).unwrap();
        }
        out.push_str("]}\n");
        out
    }
}

struct Runtime {
    control: PathBuf,
    filtering: AtomicBool,
    session: Mutex<Session>,
    publication: Mutex<(u64, Option<Instant>, bool)>,
}

static RUNTIME: LazyLock<Option<Runtime>> = LazyLock::new(|| {
    std::env::var_os("VICTORY_SHADER_CONTROL_FILE")
        .filter(|s| !s.is_empty())
        .map(|path| Runtime {
            control: PathBuf::from(path),
            filtering: AtomicBool::new(false),
            session: Mutex::default(),
            publication: Mutex::default(),
        })
});

pub(crate) fn enabled() -> bool {
    RUNTIME.is_some()
}

pub(crate) fn permits(id: GraphicsId) -> bool {
    RUNTIME.as_ref().is_none_or(|r| {
        !r.filtering.load(Ordering::Acquire)
            || r.session.lock().unwrap_or_else(|e| e.into_inner()).control.permits(id)
    })
}

pub(crate) fn submit(list: &ListCounts) {
    if list.is_empty() {
        return;
    }
    if let Some(r) = RUNTIME.as_ref() {
        r.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .submit(list);
    }
}

fn request(path: &Path) -> Request {
    let mut bytes = Vec::new();
    match fs::File::open(path).and_then(|f| f.take(4097).read_to_end(&mut bytes)) {
        Ok(_) if bytes.len() > 4096 => Request::Error("command too long"),
        Ok(_) => Request::Command(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Request::Missing,
        Err(_) => Request::Error("control unreadable"),
    }
}

fn publish(path: &Path, text: &str) -> io::Result<()> {
    let temp = path.with_file_name("shader-inventory.json.tmp");
    fs::write(&temp, text)?;
    fs::rename(temp, path.with_file_name("shader-inventory.json"))
}

pub(crate) fn frame(frame: u64) {
    let Some(r) = RUNTIME.as_ref() else {
        return;
    };
    let mut publication = r.publication.lock().unwrap_or_else(|e| e.into_inner());
    if frame <= publication.0 {
        return;
    }
    publication.0 = frame;
    let command = request(&r.control);
    let mut session = r.session.lock().unwrap_or_else(|e| e.into_inner());
    let changed = session.apply(command);
    if changed {
        r.filtering.store(session.control.targets != Targets::None, Ordering::Release);
    }
    let mut counts = Counts::default();
    for row in session.timelines.values() {
        counts.add(row.frame);
    }
    #[cfg(windows)]
    tracing::info!(target:"victory_gpu::shader_toggle", frame,revision=session.control.revision,
        mode=session.control.name(), drawn=counts.drawn, skipped=counts.skipped,error=session.error,"graphics A/B frame");
    let due = changed
        || publication
            .1
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(250));
    let data = due.then(|| session.json(frame));
    for row in session.timelines.values_mut() {
        row.frame = Counts::default();
    }
    drop(session);
    if let Some(data) = data {
        let result = publish(&r.control, &data);
        #[cfg(windows)]
        if result.is_err() && !publication.2 {
            tracing::warn!("could not publish graphics A/B inventory");
        }
        publication.2 = result.is_err();
        publication.1 = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: GraphicsId = GraphicsId {
        key: 20,
        pair: Some((7, 13)),
    };
    const B: GraphicsId = GraphicsId {
        key: 21,
        pair: Some((7, 13)),
    };
    const C: GraphicsId = GraphicsId {
        key: 22,
        pair: None,
    };
    fn cmd(t: impl Into<String>) -> Request {
        Request::Command(t.into().into_bytes())
    }

    #[test]
    fn validates_whole_command_and_supports_depth_only_keys() {
        assert_eq!(
            GraphicsId::new(&format!("{:032x}", C.key), None, None),
            Some(C)
        );
        for bad in [
            "0 all",
            "1",
            "1 all key",
            "2 solo key 1",
            "2 hide pair 0000000000000001/",
            "1 hide key 00000000000000000000000000000001,",
            "1 draw key x",
        ] {
            assert!(Selection::parse(bad.as_bytes()).is_err(), "{bad}");
        }
        let keys = vec!["00000000000000000000000000000001"; 33].join(",");
        assert!(Selection::parse(format!("9 solo key {keys}").as_bytes()).is_err());
    }

    #[test]
    fn hide_and_solo_match_multiple_keys_and_the_whole_pair() {
        let keys = format!("{:032x},{:032x}", A.key, C.key);
        for (verb, want) in [
            ("hide", [false, true, false]),
            ("solo", [true, false, true]),
        ] {
            let rule = Selection::parse(format!("2 {verb} key {keys}").as_bytes()).unwrap();
            assert_eq!([A, B, C].map(|id| rule.permits(id)), want);
        }
        let pair = Selection::parse(b"3 solo pair 0000000000000007/000000000000000D").unwrap();
        assert_eq!([A, B, C].map(|id| pair.permits(id)), [true, true, false]);
    }

    #[test]
    fn inventory_counts_only_submitted_lists_and_keeps_replays() {
        let mut session = Session::default();
        let mut list = ListCounts::default();
        list.entry(A).or_default().record(true);
        list.entry(B).or_default().record(true);
        let hide = format!("1 hide key {:032x}", A.key);
        assert!(session.apply(cmd(&hide)));
        assert_eq!(session.error, "unknown target");
        session.submit(&list);
        assert!(!session.apply(cmd(&hide)));
        assert!(session.control.permits(A));
        session.apply(cmd(format!("2 hide key {:032x}", A.key)));
        assert!(!session.control.permits(A));
        let mut filtered = ListCounts::default();
        filtered.entry(A).or_default().record(false);
        session.submit(&filtered);
        session.submit(&list);
        let image = session.json(33);
        assert!(image.contains(r#""drawn":2,"skipped":1,"frame_drawn":2,"frame_skipped":1"#));
        assert!(image.contains(r#""pair":"0000000000000007/000000000000000d""#));
        session.apply(cmd(format!("3 hide key {:032x},{:032x}", B.key, 999)));
        assert_eq!(session.control.revision, 2);
        assert!(!session.control.permits(A) && session.control.permits(B));
        session.apply(cmd("4 invalid"));
        assert_eq!(session.control.revision, 2);
        session.apply(Request::Missing);
        assert!(session.control.permits(A));
        assert_eq!(session.error, "");
    }

    #[test]
    fn closing_a_split_list_detaches_counts_for_each_part_and_its_replays() {
        let mut recording = Recording::default();
        assert!(recording.record(A));
        let first = recording.take().unwrap();
        assert!(recording.take().is_none());
        assert!(recording.record(B));
        let second = recording.take().unwrap();
        let mut session = Session::default();
        session.submit(&first);
        session.submit(&second);
        session.submit(&first);
        assert_eq!(session.timelines[&A].total.drawn, 2);
        assert_eq!(session.timelines[&B].total.drawn, 1);
    }

    #[test]
    fn polls_bounded_files_and_replaces_valid_snapshots() {
        let dir =
            std::env::temp_dir().join(format!("victory-shader-control-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shader-control.txt");
        assert_eq!(request(&file), Request::Missing);
        fs::write(&file, b"1 all\n").unwrap();
        assert_eq!(request(&file), Request::Command(b"1 all\n".to_vec()));
        fs::write(&file, vec![b' '; 4097]).unwrap();
        assert_eq!(request(&file), Request::Error("command too long"));
        let state = Session::default();
        publish(&file, &state.json(1)).unwrap();
        publish(&file, &state.json(2)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("shader-inventory.json")).unwrap(),
            state.json(2)
        );
        assert!(!dir.join("shader-inventory.json.tmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
