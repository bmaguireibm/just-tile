use futures::future::join_all;
use reqwest::Client;
use std::cmp;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom};
use tokio::runtime::Handle;

const CHUNK_SIZE: u64 = 1024 * 1024; // 1 MB chunks

pub struct HttpRangeReader {
    url: String,
    content_length: u64,
    position: u64,
    cache: HashMap<u64, Vec<u8>>,
    client: Client,
}

impl HttpRangeReader {
    pub async fn new(url: &str, client: Client) -> std::result::Result<Self, String> {
        let resp = client
            .head(url)
            .send()
            .await
            .map_err(|e| format!("HEAD request failed: {}", e))?;

        let content_length = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("Missing or invalid content-length header")?;

        Ok(Self {
            url: url.to_string(),
            content_length,
            position: 0,
            cache: HashMap::new(),
            client,
        })
    }

    /// Creates a reader with a known content length and shared client, avoiding a HEAD request.
    pub fn new_with_details(url: &str, content_length: u64, client: Client) -> Self {
        Self {
            url: url.to_string(),
            content_length,
            position: 0,
            cache: HashMap::new(),
            client,
        }
    }

    /// Prefetches multiple chunks in parallel and stores them in the cache.
    pub async fn prefetch_chunks(&mut self, chunk_indices: Vec<u64>) -> Result<()> {
        let mut futures = Vec::new();
        for idx in chunk_indices {
            if !self.cache.contains_key(&idx) {
                let url = self.url.clone();
                let client = self.client.clone();
                let total_len = self.content_length;
                futures.push(async move {
                    let start = idx * CHUNK_SIZE;
                    let end = cmp::min(start + CHUNK_SIZE - 1, total_len - 1);
                    let range = format!("bytes={}-{}", start, end);
                    let resp = client
                        .get(&url)
                        .header(reqwest::header::RANGE, &range)
                        .send()
                        .await
                        .map_err(|e| Error::other(format!("Prefetch send error: {}", e)))?;
                    if !resp.status().is_success() {
                        return Err(Error::other(format!("HTTP Status: {}", resp.status())));
                    }
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| Error::other(format!("Prefetch bytes error: {}", e)))?;
                    Ok((idx, bytes.to_vec()))
                });
            }
        }

        let results = join_all(futures).await;
        for res in results {
            let (idx, data) = res.map_err(|e: Error| e)?;
            self.cache.insert(idx, data);
        }
        Ok(())
    }

    /// Returns the content length of the remote file.
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    fn fetch_chunk(&mut self, chunk_index: u64) -> Result<()> {
        let url = self.url.clone();
        let client = self.client.clone();
        let total_len = self.content_length;

        let data = tokio::task::block_in_place(|| {
            Handle::current().block_on(async move {
                let start = chunk_index * CHUNK_SIZE;
                let end = cmp::min(start + CHUNK_SIZE - 1, total_len - 1);
                let range = format!("bytes={}-{}", start, end);
                println!("Fetching HTTP Range: {}", range);
                let resp = client
                    .get(&url)
                    .header(reqwest::header::RANGE, &range)
                    .send()
                    .await
                    .map_err(|e| {
                        Error::other(format!("HTTP Error on chunk {}: {}", chunk_index, e))
                    })?;
                if !resp.status().is_success() {
                    return Err(Error::other(format!("HTTP Status: {}", resp.status())));
                }
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| Error::other(format!("Fetch bytes error: {}", e)))?;
                Ok(bytes.to_vec())
            })
        })?;

        self.cache.insert(chunk_index, data);
        Ok(())
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.position >= self.content_length {
            return Ok(0);
        }

        let chunk_index = self.position / CHUNK_SIZE;
        let offset_in_chunk = (self.position % CHUNK_SIZE) as usize;

        if !self.cache.contains_key(&chunk_index) {
            self.fetch_chunk(chunk_index)?;
        }

        let chunk_data = self.cache.get(&chunk_index).unwrap();
        let bytes_available = chunk_data.len().saturating_sub(offset_in_chunk);

        if bytes_available == 0 {
            return Ok(0);
        }

        let bytes_to_copy = cmp::min(buf.len(), bytes_available);
        buf[..bytes_to_copy]
            .copy_from_slice(&chunk_data[offset_in_chunk..offset_in_chunk + bytes_to_copy]);

        self.position += bytes_to_copy as u64;
        Ok(bytes_to_copy)
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(p) => self.content_length as i64 + p,
            SeekFrom::Current(p) => self.position as i64 + p,
        };

        if new_pos < 0 || new_pos > self.content_length as i64 {
            return Err(Error::new(ErrorKind::InvalidInput, "Invalid seek position"));
        }

        self.position = new_pos as u64;
        Ok(self.position)
    }
}
