use crate::s3_auth::S3AuthManager;
use reqwest::Client;
use std::cmp;

use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom};
use tokio::runtime::Handle;

pub const CHUNK_SIZE: u64 = 1024 * 1024; // 1 MB chunks

pub struct HttpRangeReader {
    url: String,
    content_length: u64,
    position: u64,
    cache: std::sync::Arc<crate::cache::SharedChunkCache>,
    client: Client,
    auth_manager: Option<S3AuthManager>,
    aws_profile: Option<String>,
}

impl HttpRangeReader {
    pub async fn new(
        url: &str,
        client: Client,
        auth_manager: Option<S3AuthManager>,
        aws_profile: Option<String>,
        cache: std::sync::Arc<crate::cache::SharedChunkCache>,
    ) -> std::result::Result<Self, String> {
        let mut builder = client.head(url);

        if let Some(auth) = &auth_manager {
            builder = auth.sign(builder, url, aws_profile.as_ref()).await?;
        }

        let resp = builder
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
            cache,
            client,
            auth_manager,
            aws_profile,
        })
    }

    /// Creates a reader with a known content length and shared client, avoiding a HEAD request.
    pub fn new_with_details(
        url: &str,
        content_length: u64,
        client: Client,
        auth_manager: Option<S3AuthManager>,
        aws_profile: Option<String>,
        cache: std::sync::Arc<crate::cache::SharedChunkCache>,
    ) -> Self {
        Self {
            url: url.to_string(),
            content_length,
            position: 0,
            cache,
            client,
            auth_manager,
            aws_profile,
        }
    }

    /// Returns the content length of the remote file.
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    pub fn cache_chunks_concurrently(
        &mut self,
        chunk_indices: &[u64],
    ) -> std::result::Result<(), String> {
        let mut missing = Vec::new();
        for &idx in chunk_indices {
            // We cannot synchronously check OnceCell easily across the boundary without blocking,
            // but we can spawn them all and OneCell deals with overlaps cleanly natively!
            missing.push(idx);
        }
        if missing.is_empty() {
            return Ok(());
        }

        let url = self.url.clone();
        let auth_manager = self.auth_manager.clone();
        let aws_profile = self.aws_profile.clone();
        let total_len = self.content_length;
        let client_clone = self.client.clone();
        let shared_cache = self.cache.clone();

        let t_start = std::time::Instant::now();

        let results = Handle::current().block_on(async {
            let mut futures = Vec::new();
            for idx in missing {
                let url_c = url.clone();
                let client_c = client_clone.clone();
                let auth_c = auth_manager.clone();
                let profile_c = aws_profile.clone();
                let cache_c = shared_cache.clone();

                futures.push(tokio::spawn(async move {
                    cache_c
                        .get_or_fetch(idx, || async move {
                            let start = idx * CHUNK_SIZE;
                            let end = cmp::min(start + CHUNK_SIZE - 1, total_len - 1);
                            let range = format!("bytes={}-{}", start, end);

                            let mut attempts = 0;
                            loop {
                                attempts += 1;
                                let mut builder =
                                    client_c.get(&url_c).header(reqwest::header::RANGE, &range);
                                if let Some(auth) = &auth_c {
                                    if let Ok(signed_builder) = auth
                                        .sign(
                                            builder.try_clone().unwrap(),
                                            &url_c,
                                            profile_c.as_ref(),
                                        )
                                        .await
                                    {
                                        builder = signed_builder;
                                    }
                                }
                                match builder.send().await {
                                    Ok(resp) if resp.status().is_success() => {
                                        match resp.bytes().await {
                                            Ok(b) => return Ok(b.to_vec()),
                                            Err(_) if attempts < 3 => {
                                                tokio::time::sleep(
                                                    std::time::Duration::from_millis(100),
                                                )
                                                .await
                                            }
                                            Err(e) => {
                                                return Err(format!("Fetch bytes error: {}", e))
                                            }
                                        }
                                    }
                                    Ok(resp) => {
                                        if attempts < 3 {
                                            tokio::time::sleep(std::time::Duration::from_millis(
                                                100,
                                            ))
                                            .await;
                                        } else {
                                            return Err(format!("HTTP Status {}", resp.status()));
                                        }
                                    }
                                    Err(e) => {
                                        if attempts < 3 {
                                            tokio::time::sleep(std::time::Duration::from_millis(
                                                100,
                                            ))
                                            .await;
                                        } else {
                                            return Err(format!("HTTP Error chunk {}: {}", idx, e));
                                        }
                                    }
                                }
                            }
                        })
                        .await
                }));
            }
            futures::future::join_all(futures).await
        });

        for res in results {
            if let Err(e) = res {
                return Err(format!("Task spawn error: {}", e));
            }
        }

        tracing::info!("Concurrent pre-fetch resolved in {:?}", t_start.elapsed());
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

        let url = self.url.clone();
        let client = self.client.clone();
        let total_len = self.content_length;
        let auth_manager = self.auth_manager.clone();
        let aws_profile = self.aws_profile.clone();

        let chunk_data = Handle::current()
            .block_on(async {
                self.cache
                    .get_or_fetch(chunk_index, || async move {
                        let start = chunk_index * CHUNK_SIZE;
                        let end = cmp::min(start + CHUNK_SIZE - 1, total_len - 1);
                        let range = format!("bytes={}-{}", start, end);
                        let fetch_start = std::time::Instant::now();
                        tracing::debug!("Lazy fetching missing chunk {}", chunk_index);

                        let mut attempts = 0;
                        loop {
                            attempts += 1;
                            let mut builder =
                                client.get(&url).header(reqwest::header::RANGE, &range);
                            if let Some(auth) = &auth_manager {
                                if let Ok(signed_builder) = auth
                                    .sign(builder.try_clone().unwrap(), &url, aws_profile.as_ref())
                                    .await
                                {
                                    builder = signed_builder;
                                }
                            }
                            match builder.send().await {
                                Ok(resp) if resp.status().is_success() => {
                                    match resp.bytes().await {
                                        Ok(b) => {
                                            tracing::info!(
                                                "Lazy Chunk {} fetched and cached in {:?}",
                                                chunk_index,
                                                fetch_start.elapsed()
                                            );
                                            return Ok(b.to_vec());
                                        }
                                        Err(_) if attempts < 3 => {
                                            tokio::time::sleep(std::time::Duration::from_millis(
                                                100,
                                            ))
                                            .await
                                        }
                                        Err(e) => return Err(format!("Fetch bytes error: {}", e)),
                                    }
                                }
                                Ok(resp) => {
                                    if attempts < 3 {
                                        tokio::time::sleep(std::time::Duration::from_millis(100))
                                            .await;
                                    } else {
                                        return Err(format!("HTTP Status {}", resp.status()));
                                    }
                                }
                                Err(e) => {
                                    if attempts < 3 {
                                        tokio::time::sleep(std::time::Duration::from_millis(100))
                                            .await;
                                    } else {
                                        return Err(format!(
                                            "HTTP Error chunk {}: {}",
                                            chunk_index, e
                                        ));
                                    }
                                }
                            }
                        }
                    })
                    .await
            })
            .map_err(Error::other)?;

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
