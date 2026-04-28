use crate::geotiff::CogMetadata;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::OnceCell;

type ChunkMap = HashMap<u64, Arc<OnceCell<Arc<Vec<u8>>>>>;

pub struct SharedChunkCache {
    chunks: RwLock<ChunkMap>,
}

impl Default for SharedChunkCache {
    fn default() -> Self {
        Self {
            chunks: RwLock::new(HashMap::new()),
        }
    }
}

impl SharedChunkCache {
    pub async fn get_or_fetch<F, Fut>(
        &self,
        chunk_index: u64,
        fetch_fn: F,
    ) -> Result<Arc<Vec<u8>>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
    {
        let cell = {
            let mut chunks = self.chunks.write().unwrap();
            chunks
                .entry(chunk_index)
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };

        match cell
            .get_or_try_init(|| async { fetch_fn().await.map(Arc::new) })
            .await
        {
            Ok(data) => Ok(data.clone()),
            Err(e) => Err(e.clone()),
        }
    }
}

/// Internal entry for the COG cache.
#[derive(Clone)]
pub struct CacheEntry {
    pub content_length: u64,
    pub metadata: Option<CogMetadata>,
    pub shared_cache: Arc<SharedChunkCache>,
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

    pub fn get_or_insert_empty(&self, url: &str, length: u64) -> CacheEntry {
        // Do we have it?
        if let Some(entry) = self.get(url) {
            return entry;
        }

        let new_entry = CacheEntry {
            content_length: length,
            metadata: None,
            shared_cache: Arc::new(SharedChunkCache::default()),
        };

        if let Ok(mut lock) = self.entries.write() {
            lock.entry(url.to_string()).or_insert(new_entry).clone()
        } else {
            new_entry
        }
    }

    pub fn insert_metadata(&self, url: String, metadata: CogMetadata) {
        if let Ok(mut lock) = self.entries.write() {
            if let Some(entry) = lock.get_mut(&url) {
                entry.metadata = Some(metadata);
            }
        }
    }

    pub fn insert(&self, url: String, entry: CacheEntry) {
        if let Ok(mut lock) = self.entries.write() {
            lock.insert(url, entry);
        }
    }
}
