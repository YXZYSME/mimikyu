use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::driver::BoundingBox;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefEntry {
    pub ref_id: String,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub frame_id: Option<String>,
    pub backend_node_id: Option<i64>,
    pub bounds: Option<BoundingBox>,
    pub css_fallback: Option<String>,
    pub disabled: bool,
    pub focused: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
}

impl RefEntry {
    pub fn center(&self) -> Option<(f64, f64)> {
        self.bounds.filter(|b| b.is_visible()).map(|b| b.center())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RefMap {
    entries: BTreeMap<usize, RefEntry>,
    #[serde(default)]
    extra: BTreeMap<String, RefEntry>,
}

impl RefMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn normalize_ref_id(raw: &str) -> String {
        let trimmed = raw.trim();
        if let Some(stripped) = trimmed.strip_prefix('@') {
            format!("@{stripped}")
        } else {
            format!("@{trimmed}")
        }
    }

    fn parse_numeric_key(normalized: &str) -> Option<usize> {
        normalized
            .strip_prefix("@e")
            .and_then(|rest| rest.parse::<usize>().ok())
    }

    pub fn insert(&mut self, mut entry: RefEntry) {
        let key = Self::normalize_ref_id(&entry.ref_id);
        entry.ref_id = key.clone();
        if let Some(num) = Self::parse_numeric_key(&key) {
            self.entries.insert(num, entry);
        } else {
            self.extra.insert(key, entry);
        }
    }

    pub fn get(&self, ref_id: &str) -> Option<&RefEntry> {
        let key = Self::normalize_ref_id(ref_id);
        if let Some(num) = Self::parse_numeric_key(&key) {
            self.entries.get(&num)
        } else {
            self.extra.get(&key)
        }
    }

    pub fn get_mut(&mut self, ref_id: &str) -> Option<&mut RefEntry> {
        let key = Self::normalize_ref_id(ref_id);
        if let Some(num) = Self::parse_numeric_key(&key) {
            self.entries.get_mut(&num)
        } else {
            self.extra.get_mut(&key)
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len() + self.extra.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.extra.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.extra.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &RefEntry)> {
        self.entries
            .values()
            .chain(self.extra.values())
            .map(|e| (&e.ref_id, e))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetLocator<'a> {
    Ref(&'a str),
    Selector(&'a str),
}

impl<'a> TargetLocator<'a> {
    pub fn parse(raw: &'a str) -> Self {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("@e")
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return Self::Ref(trimmed);
        }
        Self::Selector(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_locator_parses_refs_and_css_selectors() {
        assert_eq!(TargetLocator::parse("@e1"), TargetLocator::Ref("@e1"));
        assert_eq!(TargetLocator::parse("  @e42 "), TargetLocator::Ref("@e42"));
        assert_eq!(
            TargetLocator::parse("#submit"),
            TargetLocator::Selector("#submit")
        );
        assert_eq!(
            TargetLocator::parse("@email"),
            TargetLocator::Selector("@email")
        );
    }

    #[test]
    fn ref_map_lookup_accepts_with_or_without_at_prefix() {
        let mut map = RefMap::new();
        map.insert(RefEntry {
            ref_id: "@e1".to_string(),
            role: "button".to_string(),
            name: Some("Save".to_string()),
            value: None,
            frame_id: None,
            backend_node_id: Some(99),
            bounds: Some(BoundingBox {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 40.0,
            }),
            css_fallback: Some("#save".to_string()),
            disabled: false,
            focused: false,
            checked: None,
            expanded: None,
            selected: None,
        });
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("@e1").unwrap().backend_node_id, Some(99));
        assert_eq!(map.get("e1").unwrap().center(), Some((60.0, 40.0)));
    }
}
