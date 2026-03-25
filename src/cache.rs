use crate::geotiff::CogMetadata;
use std::collections::HashMap;
use std::sync::RwLock;

/// Internal entry for the COG cache.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub content_length: u64,
    pub metadata: CogMetadata,
}

/// A simple thread-safe cache for COG headers and metadata.
pub struct CogCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
}

impl Default for CogCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CogCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, url: &str) -> Option<CacheEntry> {
        let lock = self.entries.read().ok()?;
        lock.get(url).cloned()
    }

    pub fn insert(&self, url: String, entry: CacheEntry) {
        if let Ok(mut lock) = self.entries.write() {
            lock.insert(url, entry);
        }
    }
}
