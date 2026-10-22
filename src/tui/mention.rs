//! The composer's `@` ghost autocomplete.
//!
//! While the text before the cursor ends in an `@` that starts a word (the
//! start of the input, or after whitespace or `(`) followed by handle
//! characters, and the cursor is at the end of the input or before
//! whitespace, the composer draws the rest of the best matching handle from
//! the caret on in fog: the ghost. It is never part of the input, so sending
//! never sends it. `Tab` puts it in at the cursor with a space after it
//! (unless whitespace follows already), `↑` `↓` pick another match and `Esc`
//! puts it away for that word (the next `@` brings it back); the keys are in
//! [`crate::tui::keys`]. Every `@` that can show a ghost is one
//! [`crate::model::parse_mentions`] reads as a mention.
//!
//! The candidates, best first: the sidebar's agents (active, then idle, each
//! most recently seen first), the other authors of the loaded posts (newest
//! post first), then `all`; never `me`. A handle matches when it starts with
//! what is typed after the `@`, in any case.

use std::cmp::Reverse;

use crate::model::{is_handle_char, validate_handle};
use crate::tui::state::{App, Focus};

/// What the ghost offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ghost {
    /// The whole handle.
    pub handle: String,
    /// What is left of it to type: drawn after the caret, inserted by `Tab`.
    pub rest: String,
    /// Which of the matches it is (0 is the best).
    pub index: usize,
    /// How many handles match what is typed.
    pub count: usize,
}

/// What the keys did to the `@handle` being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// Byte offset of its `@` in the input: which word this is.
    pub at: usize,
    /// What was typed after the `@` when the keys last looked.
    pub prefix: String,
    /// The match `↑` `↓` moved to; `None` is the best one.
    pub pick: Option<String>,
    /// `Esc` put the ghost away for this word.
    pub dismissed: bool,
}

/// The `@handle` `input` ends in (the composer's text up to the cursor), if
/// its `@` starts a word: the `@`'s byte offset, and the handle characters
/// typed after it (maybe none).
pub fn trigger(input: &str) -> Option<(usize, &str)> {
    let start = input.trim_end_matches(is_handle_char).len();
    if !input[..start].ends_with('@') {
        return None;
    }
    let at = start - 1;
    input[..at]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || c == '(')
        .then(|| (at, &input[start..]))
}

impl App {
    /// The ghost the composer shows, if any: only while it has the keys.
    pub fn ghost(&self) -> Option<Ghost> {
        if self.focus != Focus::Composer || self.dialog.is_some() || self.filter_editing {
            return None;
        }
        let cursor = self.cursor();
        let next = self.input[cursor..].chars().next();
        if next.is_some_and(|c| !c.is_whitespace()) {
            return None;
        }
        let (at, prefix) = trigger(&self.input[..cursor])?;
        let state = self.mention.as_ref().filter(|m| m.at == at);
        if state.is_some_and(|m| m.dismissed) {
            return None;
        }
        let matches = self.mention_matches(prefix);
        let index = state
            .filter(|m| m.prefix == prefix)
            .and_then(|m| m.pick.as_deref())
            .and_then(|pick| matches.iter().position(|h| h == pick))
            .unwrap_or(0);
        let handle = matches.get(index)?.clone();
        Some(Ghost {
            // Handles are ASCII, and the prefix is a case-insensitive start
            // of this one, so it ends on a character boundary.
            rest: handle[prefix.len()..].to_string(),
            handle,
            index,
            count: matches.len(),
        })
    }

    /// Every handle the ghost can offer, best first.
    pub fn mention_candidates(&self) -> Vec<String> {
        let mut agents: Vec<_> = self.agents.iter().collect();
        agents.sort_by_key(|a| (!a.active, Reverse(a.last_seen_ms)));
        let agents = agents.into_iter().map(|a| a.handle.as_str());
        let authors = self.posts.iter().rev().map(|p| p.author.as_str());
        let mut handles: Vec<String> = Vec::new();
        for handle in agents.chain(authors).chain(["all"]) {
            let known = handle.eq_ignore_ascii_case(&self.me)
                || handles.iter().any(|h| h.eq_ignore_ascii_case(handle));
            if !known && validate_handle(handle).is_ok() {
                handles.push(handle.to_string());
            }
        }
        handles
    }

    /// The candidates that start with `prefix`, in any case.
    fn mention_matches(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        self.mention_candidates()
            .into_iter()
            .filter(|h| h.to_ascii_lowercase().starts_with(&prefix))
            .collect()
    }

    /// `↓` (forward) and `↑`: the next or previous match, round the ends.
    pub(crate) fn pick_mention(&mut self, ghost: &Ghost, forward: bool) {
        let Some((at, prefix)) = trigger(self.before_cursor()) else {
            return;
        };
        let prefix = prefix.to_string();
        let count = ghost.count.max(1);
        let next = if forward {
            (ghost.index + 1) % count
        } else {
            (ghost.index + count - 1) % count
        };
        let pick = self.mention_matches(&prefix).get(next).cloned();
        self.mention_for(at, &prefix).pick = pick;
    }

    /// `Esc`: no ghost for this word.
    pub(crate) fn dismiss_mention(&mut self) {
        if let Some((at, prefix)) = trigger(self.before_cursor()) {
            let prefix = prefix.to_string();
            self.mention_for(at, &prefix).dismissed = true;
        }
    }

    /// Keeps the keys' state in step with the input, after every message:
    /// more typed in the word starts again from the best match (still put
    /// away if it was); a word that ended, or a new `@`, starts afresh.
    pub(crate) fn sync_mention(&mut self) {
        match trigger(self.before_cursor()) {
            Some((at, prefix)) if self.mention.is_some() => {
                let prefix = prefix.to_string();
                self.mention_for(at, &prefix);
            }
            _ => self.mention = None,
        }
    }

    /// The composer's text up to the cursor: where a mention is typed.
    fn before_cursor(&self) -> &str {
        &self.input[..self.cursor()]
    }

    /// The keys' state for the word whose `@` is at `at`, with `prefix`
    /// typed after it.
    fn mention_for(&mut self, at: usize, prefix: &str) -> &mut Mention {
        let mut mention = self
            .mention
            .take()
            .filter(|m| m.at == at)
            .unwrap_or_else(|| Mention {
                at,
                prefix: prefix.to_string(),
                pick: None,
                dismissed: false,
            });
        if mention.prefix != prefix {
            mention.prefix = prefix.to_string();
            mention.pick = None;
        }
        self.mention.insert(mention)
    }
}
