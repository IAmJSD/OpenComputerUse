//! The elements of the last tree a session read, by the ids the tree gave
//! them, so element actions can find them again.

use anyhow::{anyhow, Result};

use ocu_core::Rect;

#[derive(Clone, Debug, Default)]
pub struct Element {
    /// In points, relative to the screen.
    pub frame: Rect,
    /// Where the backend can find the element again: the helper's node
    /// path (Android) or the element's index in document order (iOS).
    pub path: Vec<usize>,
    pub editable: bool,
    pub scrollable: bool,
    /// How many characters the element's value had, for clearing a field
    /// by deleting them.
    pub value_len: usize,
}

#[derive(Default)]
pub struct Elements(Vec<Element>);

impl Elements {
    /// Registers an element and returns its id: "e12".
    pub fn add(&mut self, e: Element) -> String {
        self.0.push(e);
        format!("e{}", self.0.len() - 1)
    }

    pub fn get(&self, id: &str) -> Result<&Element> {
        id.trim()
            .strip_prefix('e')
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| self.0.get(n))
            .ok_or_else(|| {
                anyhow!("no element {id} in the last tree; read the tree again (get_ui_tree)")
            })
    }
}
