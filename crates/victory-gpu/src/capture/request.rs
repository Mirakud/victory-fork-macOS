//! One-shot target capture, armed at a presentation boundary.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct TargetRequest {
    path: PathBuf,
    frame: AtomicU64,
}

impl TargetRequest {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path, frame: AtomicU64::new(0) }
    }

    pub(super) fn selects(&self, frame: u64) -> bool {
        frame != 0 && self.frame.load(Ordering::Acquire) == frame
    }

    /// The immediately following frame can already be recording on other threads.
    /// Take the first full frame after it. Keep a second request on disk while busy.
    pub(super) fn after_present(&self, finished: u64, presented: impl FnOnce() -> u64) -> Option<u64> {
        let selected = self.frame.load(Ordering::Acquire);
        if selected != 0 && selected != finished {
            return None;
        }
        if self.frame.compare_exchange(selected, u64::MAX, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        let next = std::fs::remove_file(&self.path).ok().map(|()| presented() + 2);
        self.frame.store(next.unwrap_or(0), Ordering::Release);
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_select_one_full_frame_and_queue_while_busy() {
        let path = std::env::temp_dir().join(format!("victory-target-request-{}", std::process::id()));
        let request = TargetRequest::new(path.clone());
        let _ = std::fs::remove_file(&path);
        assert_eq!(request.after_present(73, || 73), None);
        std::fs::write(&path, b"").unwrap();
        assert_eq!(request.after_present(74, || 75), Some(77));
        assert!(!path.exists());
        assert!(!request.selects(76));
        assert!(request.selects(77));
        std::fs::write(&path, b"").unwrap();
        assert_eq!(request.after_present(76, || 76), None);
        assert!(path.exists());
        assert_eq!(request.after_present(77, || 77), Some(79));
        assert!(!request.selects(77));
        assert!(request.selects(79));
        assert_eq!(request.after_present(79, || 79), None);
        assert!(!request.selects(79));
        assert!(!path.exists());
    }
}
