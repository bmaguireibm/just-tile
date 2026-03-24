use std::cmp;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Result, Seek, SeekFrom};

const CHUNK_SIZE: u64 = 1024 * 1024; // 1 MB chunks

pub struct HttpRangeReader {
    url: String,
    content_length: u64,
    position: u64,
    cache: HashMap<u64, Vec<u8>>,
}

impl HttpRangeReader {
    pub fn new(url: &str) -> std::result::Result<Self, String> {
        let resp = ureq::head(url)
            .call()
            .map_err(|e| format!("HEAD request failed: {}", e))?;
        let length_str = resp
            .header("content-length")
            .ok_or("Missing content-length header")?;
        let content_length: u64 = length_str
            .parse()
            .map_err(|e| format!("Bad content length parsing: {}", e))?;

        Ok(Self {
            url: url.to_string(),
            content_length,
            position: 0,
            cache: HashMap::new(),
        })
    }

    fn fetch_chunk(&mut self, chunk_index: u64) -> Result<()> {
        let start = chunk_index * CHUNK_SIZE;
        let end = cmp::min(start + CHUNK_SIZE - 1, self.content_length - 1);

        let range_header = format!("bytes={}-{}", start, end);
        println!("Fetching HTTP Range: {}", range_header);

        let resp = match ureq::get(&self.url).set("Range", &range_header).call() {
            Ok(r) => r,
            Err(e) => return Err(Error::other(format!("HTTP Error: {}", e))),
        };

        if resp.status() != 200 && resp.status() != 206 {
            return Err(Error::other(format!("HTTP Status: {}", resp.status())));
        }

        let mut reader = resp.into_reader();
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;

        self.cache.insert(chunk_index, buffer);
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

#[cfg(test)]
mod tests {
    // A real mock testing would require an HTTP mocking framework,
    // but we can add basic unit tests or integration tests later.
}
