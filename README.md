# Just-Tile

Just-Tile is a high-performance, single-binary tile server written in pure Rust. It dynamically serves map tiles from Cloud Optimized GeoTIFFs (COGs) stored on Amazon S3 (or any HTTP server).

**This is entirely Vibe coded, I don't know rust, use at your own risk**

## Features

- **Pure Rust Implementation**: No C-based dependencies (like GDAL).
- **Asynchronous & Concurrent**: Uses `tokio` and `reqwest` for non-blocking I/O.
- **Parallel Fetching**: Plans tile extraction and fetches required GeoTIFF chunks concurrently to minimize latency.
- **Efficient Caching**: Thread-safe metadata caching avoids redundant S3 `HEAD` and header requests.
- **On-the-fly Resampling**: Smooth bilinear interpolation for high-quality tiles at any zoom level.
- **Zero-Dependency Deployment**: Can be compiled to a static binary and run in a `FROM scratch` Docker image.

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable)

### Running Locally

```bash
cargo run
```
The server listens on `http://0.0.0.0:3000`.

### API Usage

Fetch a tile by providing Z, X, Y coordinates and a URL to a COG:

```bash
GET /{z}/{x}/{y}?url={cog_url}
```

**Example:**
```bash
curl "http://localhost:3000/11/988/660?url=https://e84-earth-search-sentinel-data.s3.us-west-2.amazonaws.com/sentinel-2-c1-l2a/29/U/PV/2026/3/S2A_T29UPV_20260314T113337_L2A/TCI.tif" -o tile.png
```

## Deployment with Docker

Build the optimized, scratch-based image:

```bash
docker build -t just-tile .
docker run -p 3000:3000 just-tile
```

## Authentication (AWS S3)

Just-Tile gracefully falls back to unauthenticated, unsigned requests if no credentials are provided. However, it fully supports the standard AWS SDK credential chain:

### 1. Default Environment Variables
You can pass standard AWS credentials to sign requests to S3 on-the-fly via AWS Signature V4.
```bash
AWS_ACCESS_KEY_ID=your_key AWS_SECRET_ACCESS_KEY=your_secret cargo run
```

### 2. Multi-Endpoint Configuration (Mapping File)
If you require multiple credentials corresponding to different buckets/endpoints (like an internal Minio instance + AWS S3), you can supply a JSON mapping file using standard AWS Profile names from `~/.aws/credentials`:
```bash
S3_ENDPOINT_MAPPING=endpoints.json cargo run
```
**`endpoints.json` example:**
```json
{
  "internal-minio.acme.com": "minio_local",
  "s3.us-west-2.amazonaws.com": "default"
}
```

### 3. API Option Overrides
If you define multiple AWS Profiles in standard `~/.aws/credentials`, clients can optionally dictate the authentication profile used for any request:
```bash
GET /{z}/{x}/{y}?url=s3://my-secret-bucket/data.tif&aws_profile=dev_bucket_admin
```

### Running Auth with Docker
When using Docker, you can pass credentials cleanly via environment variables or mount your local `.aws` directory for profiles:
```bash
docker run -p 3000:3000 \
  -e AWS_ACCESS_KEY_ID=xxx \
  -e AWS_SECRET_ACCESS_KEY=yyy \
  -e AWS_REGION=us-west-2 \
  just-tile
```
Or mount standard profiles configurations:
```bash
docker run -p 3000:3000 -v ~/.aws:/root/.aws:ro just-tile
```


## Performance Architecture

- **Planning Phase**: Extracts GeoKey and IFD metadata (cached).
- **Execution Phase**:
  1. Calculates required TIFF tiles for the given Web Mercator extent.
  2. Prefetches all required 1MB chunks in parallel.
  3. Decodes and stitches source pixels.
  4. Resamples to 256x256 target RGBA image.
