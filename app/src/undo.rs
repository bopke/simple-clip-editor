//! Undo/redo over whole-project snapshots. The edit model is small — a few
//! segments, a crop rect, a handful of audio settings — so snapshotting is
//! simpler than diffing and impossible to get subtly wrong.

use clip_core::Project;
use std::collections::VecDeque;

const MAX_DEPTH: usize = 100;

/// Both stacks push and pop at the back; `past` also drops from the front once
/// it is full, which is the only reason this is a deque rather than a `Vec`.
#[derive(Default)]
pub struct UndoStack {
    past: VecDeque<Project>,
    future: VecDeque<Project>,
}

impl UndoStack {
    pub fn push(&mut self, before: Project) {
        self.past.push_back(before);
        if self.past.len() > MAX_DEPTH {
            self.past.pop_front();
        }
        self.future.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// Number of undo steps available. Exposed so tests can assert that an
    /// edit produced exactly one entry.
    pub fn depth(&self) -> usize {
        self.past.len()
    }

    pub fn undo(&mut self, current: Project) -> Option<Project> {
        let prev = self.past.pop_back()?;
        self.future.push_back(current);
        Some(prev)
    }

    pub fn redo(&mut self, current: Project) -> Option<Project> {
        let next = self.future.pop_back()?;
        self.past.push_back(current);
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clip_core::{FrameIdx, Segment, SourceInfo};

    fn project(end: u64) -> Project {
        let mut p = Project::new(SourceInfo::default());
        p.segments = vec![Segment {
            start: FrameIdx(0),
            end: FrameIdx(end),
        }];
        p
    }

    #[test]
    fn undo_then_redo_returns_the_same_state() {
        let mut stack = UndoStack::default();
        let a = project(100);
        let b = project(200);
        stack.push(a.clone());
        assert!(stack.can_undo());
        let restored = stack.undo(b.clone()).unwrap();
        assert_eq!(restored.segments[0].end, FrameIdx(100));
        assert!(stack.can_redo());
        let again = stack.redo(restored).unwrap();
        assert_eq!(again.segments[0].end, FrameIdx(200));
    }

    #[test]
    fn a_new_edit_clears_the_redo_branch() {
        let mut stack = UndoStack::default();
        stack.push(project(1));
        let _ = stack.undo(project(2));
        assert!(stack.can_redo());
        stack.push(project(3));
        assert!(!stack.can_redo());
    }

    #[test]
    fn depth_is_bounded() {
        let mut stack = UndoStack::default();
        for i in 0..(MAX_DEPTH + 20) {
            stack.push(project(i as u64));
        }
        assert_eq!(stack.depth(), MAX_DEPTH);
        // The *oldest* entries are the ones dropped, so the 20 overflowing
        // pushes cost states 0..20 and the newest is still on top.
        assert_eq!(stack.past.front().unwrap().segments[0].end, FrameIdx(20));
        assert_eq!(
            stack.past.back().unwrap().segments[0].end,
            FrameIdx((MAX_DEPTH + 19) as u64)
        );
    }
}
