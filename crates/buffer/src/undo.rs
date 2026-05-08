//! Undo/redo history for the input buffer and vim motions.

use crate::attachment::AttachmentId;

#[derive(Clone)]
pub struct UndoEntry {
    pub buf: String,
    pub cpos: usize,
    pub attachments: Vec<AttachmentId>,
}

impl UndoEntry {
    pub fn snapshot(buf: &str, cpos: usize, attachments: &[AttachmentId]) -> Self {
        Self {
            buf: buf.to_string(),
            cpos,
            attachments: attachments.to_vec(),
        }
    }
}

pub struct UndoHistory {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    cap: Option<usize>,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Clone for UndoHistory {
    fn clone(&self) -> Self {
        Self {
            undo: self.undo.clone(),
            redo: self.redo.clone(),
            cap: self.cap,
        }
    }
}

impl UndoHistory {
    pub fn new(cap: Option<usize>) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            cap,
        }
    }

    /// Push a snapshot, clearing redo.
    pub fn save(&mut self, entry: UndoEntry) {
        self.redo.clear();
        self.undo.push(entry);
        if let Some(cap) = self.cap {
            while self.undo.len() > cap {
                self.undo.remove(0);
            }
        }
    }

    /// Pop the most recent snapshot onto redo, return it.
    pub fn undo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.undo.pop()?;
        self.redo.push(current);
        Some(entry)
    }

    /// Pop the most recent redo onto undo, return it.
    pub fn redo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.redo.pop()?;
        self.undo.push(current);
        Some(entry)
    }
}
