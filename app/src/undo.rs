//! Undo/redo over whole-project snapshots. The edit model is small — a few
//! segments, a crop rect, a handful of audio settings — so snapshotting is
//! simpler than diffing and impossible to get subtly wrong.

use clip_core::Project;

const MAX_DEPTH: usize = 100;

#[derive(Default)]
pub struct UndoStack {
    past: Vec<Project>,
    future: Vec<Project>,
}

impl UndoStack {
    pub fn push(&mut self, before: Project) {
        self.past.push(before);
        if self.past.len() > MAX_DEPTH {
            self.past.remove(0);
        }
        self.future.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    pub fn undo(&mut self, current: Project) -> Option<Project> {
        let prev = self.past.pop()?;
        self.future.push(current);
        Some(prev)
    }

    pub fn redo(&mut self, current: Project) -> Option<Project> {
        let next = self.future.pop()?;
        self.past.push(current);
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
        assert_eq!(stack.past.len(), MAX_DEPTH);
    }
}
