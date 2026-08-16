//! The list of problems shown to the user.
//!
//! Bounded and deduplicating, because the things that go wrong here go wrong
//! repeatedly: a proxy that cannot be decoded fails once per requested frame,
//! which is once per scrub tick. What the user needs is "this is failing", not
//! four hundred copies of it, and what the process needs is for a session-long
//! failure not to be a session-long allocation.

use std::collections::VecDeque;

/// Distinct messages kept. Past this the oldest is dropped — a message that has
/// not recurred while sixteen others came and went is not the one being looked
/// at.
const CAPACITY: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub message: String,
    /// How many times this message has arrived, including the first.
    pub count: u32,
}

impl Entry {
    /// What the UI shows: the message, and how often it has happened when that
    /// is more than once.
    pub fn label(&self) -> String {
        if self.count > 1 {
            format!("{} (×{})", self.message, self.count)
        } else {
            self.message.clone()
        }
    }
}

/// Newest last.
#[derive(Debug, Default)]
pub struct ErrorLog {
    entries: VecDeque<Entry>,
}

impl ErrorLog {
    /// Record a problem. A repeat of something already listed bumps its count
    /// and moves it to the end rather than adding a line: a message that is
    /// still arriving is the most current thing in the log, not the oldest.
    pub fn push(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let Some(i) = self.entries.iter().position(|e| e.message == message) {
            let mut entry = self.entries.remove(i).expect("index came from the deque");
            entry.count = entry.count.saturating_add(1);
            self.entries.push_back(entry);
            return;
        }
        self.entries.push_back(Entry { message, count: 1 });
        if self.entries.len() > CAPACITY {
            self.entries.pop_front();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The `n` most recent, newest first.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &Entry> {
        self.entries.iter().rev().take(n)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_message_is_counted_rather_than_repeated() {
        let mut log = ErrorLog::default();
        for _ in 0..500 {
            log.push("frame 12 could not be decoded");
        }
        assert_eq!(log.len(), 1);
        let entry = log.recent(1).next().unwrap();
        assert_eq!(entry.count, 500);
        assert_eq!(entry.label(), "frame 12 could not be decoded (×500)");
    }

    /// A distinct message per failed frame, for as long as the session lasts,
    /// must not add up to a list that grows forever.
    #[test]
    fn distinct_messages_are_bounded() {
        let mut log = ErrorLog::default();
        for i in 0..(CAPACITY * 10) {
            log.push(format!("frame {i} could not be decoded"));
        }
        assert_eq!(log.len(), CAPACITY);
        // The newest survived and the oldest did not.
        assert_eq!(
            log.recent(1).next().unwrap().message,
            format!("frame {} could not be decoded", CAPACITY * 10 - 1)
        );
        assert!(!log.iter().any(|e| e.message.contains("frame 0 ")));
    }

    #[test]
    fn a_recurrence_moves_a_message_back_to_the_front() {
        let mut log = ErrorLog::default();
        log.push("proxy is unreadable");
        log.push("audio extraction failed");
        assert_eq!(
            log.recent(1).next().unwrap().message,
            "audio extraction failed"
        );

        log.push("proxy is unreadable");
        let recent: Vec<&str> = log.recent(2).map(|e| e.message.as_str()).collect();
        assert_eq!(recent, ["proxy is unreadable", "audio extraction failed"]);
        assert_eq!(log.len(), 2, "still two problems, not three");
    }

    /// A recurrence must not let a message outlive the cap for free — it holds
    /// its place, but sixteen newer distinct problems still push it out.
    #[test]
    fn a_counted_message_is_still_subject_to_the_cap() {
        let mut log = ErrorLog::default();
        log.push("the old one");
        log.push("the old one");
        for i in 0..CAPACITY {
            log.push(format!("problem {i}"));
        }
        assert_eq!(log.len(), CAPACITY);
        assert!(!log.iter().any(|e| e.message == "the old one"));
    }

    #[test]
    fn dismissing_empties_it() {
        let mut log = ErrorLog::default();
        log.push("something");
        assert!(!log.is_empty());
        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.recent(4).count(), 0);
    }

    #[test]
    fn a_single_occurrence_is_not_labelled_with_a_count() {
        let mut log = ErrorLog::default();
        log.push("nothing to export — the timeline is empty");
        assert_eq!(
            log.recent(1).next().unwrap().label(),
            "nothing to export — the timeline is empty"
        );
    }
}
