use crate::s3_auth::S3AuthManager;
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
    auth_manager: Option<S3AuthManager>,
    aws_profile: Option<String>,
}

impl HttpRangeReader {
    pub async fn new(
        url: &str,
        client: Client,
        auth_manager: Option<S3AuthManager>,
        aws_profile: Option<String>,
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
            cache: HashMap::new(),
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
    ) -> Self {
        Self {
            url: url.to_string(),
            content_length,
            position: 0,
            cache: HashMap::new(),
            client,
            auth_manager,
            aws_profile,
        }
    }

    /// Returns the content length of the remote file.
    pub fn content_length(&self) -> u64 {
        self.content_length
    }

    fn fetch_chunk(&mut self, chunk_index: u64) -> Result<()> {
        let url = self.url.clone();
        let client = self.client.clone();
        let total_len = self.content_length;
        let auth_manager = self.auth_manager.clone();
        let aws_profile = self.aws_profile.clone();

        let start = chunk_index * CHUNK_SIZE;
        let end = cmp::min(start + CHUNK_SIZE - 1, total_len - 1);
        let range = format!("bytes={}-{}", start, end);
        println!("Fetching HTTP Range: {}", range);

        let mut attempts = 0;
        let data = loop {
            attempts += 1;
            let range_clone = range.clone();
            let url_clone = url.clone();
            let client_clone = client.clone();
            let auth_clone = auth_manager.clone();
            let profile_clone = aws_profile.clone();
            
            let response = Handle::current().block_on(async {
                let mut builder = client_clone
                    .get(&url_clone)
                    .header(reqwest::header::RANGE, &range_clone);
                
                if let Some(auth) = auth_clone {
                    if let Ok(signed_builder) = auth.sign(builder.try_clone().unwrap(), &url_clone, profile_clone.as_ref()).await {
                        builder = signed_builder;
                    }
                }
                
                builder.send().await
            });

            match response {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        if attempts < 3 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            continue;
                        }
                        return Err(Error::other(format!("HTTP Status: {}", resp.status())));
                    }
                    let bytes = Handle::current().block_on(async { resp.bytes().await });
                    match bytes {
                        Ok(b) => break b.to_vec(),
                        Err(e) => {
                            if attempts < 3 {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                continue;
                            }
                            return Err(Error::other(format!("Fetch bytes error: {}", e)));
                        }
                    }
                }
                Err(e) => {
                    if attempts < 3 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                    return Err(Error::other(format!("HTTP Error on chunk {}: {}", chunk_index, e)));
                }
            }
        };

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
