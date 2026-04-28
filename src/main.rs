use just_tile::{cache, geotiff, http_reader};

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use std::io::Cursor;
use std::sync::Arc;
use tiff::decoder::Decoder;

#[derive(Deserialize, Debug)]
struct TileQuery {
    url: String,
    aws_profile: Option<String>,
}

#[derive(Default, serde::Serialize, Clone)]
pub struct Metrics {
    pub total_requests: u64,
    pub total_errors: u64,
    pub cumulative_load_time_ms: u64,
    pub cumulative_resample_time_ms: u64,
    pub cumulative_encode_time_ms: u64,
}

struct AppState {
    cache: cache::CogCache,
    client: reqwest::Client,
    s3_auth: just_tile::s3_auth::S3AuthManager,
    metrics: std::sync::RwLock<Metrics>,
}

async fn health_check() -> &'static str {
    "Tile Server is running"
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> axum::Json<Metrics> {
    axum::Json(state.metrics.read().unwrap().clone())
}

#[tracing::instrument(skip(state, query), fields(url=%query.url, z=%z, x=%x, y=%y))]
async fn get_tile(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u32, u32, u32)>,
    Query(query): Query<TileQuery>,
) -> Result<Response, StatusCode> {
    let tile_start = std::time::Instant::now();
    // Attempt to use cached metadata
    let cached_entry = state.cache.get(&query.url);
    let (content_length, shared_cache) = if let Some(ref entry) = cached_entry {
        (entry.content_length, entry.shared_cache.clone())
    } else {
        let mut builder = state.client.head(&query.url);
        if let Ok(signed_builder) = state
            .s3_auth
            .sign(
                builder
                    .try_clone()
                    .expect("Failed to clone request builder"),
                &query.url,
                query.aws_profile.as_ref(),
            )
            .await
        {
            builder = signed_builder;
        }
        let resp = builder.send().await.map_err(|e| {
            println!("HEAD error: {}", e);
            StatusCode::BAD_REQUEST
        })?;
        let length = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let entry = state.cache.get_or_insert_empty(&query.url, length);
        (entry.content_length, entry.shared_cache.clone())
    };

    let mut reader = http_reader::HttpRangeReader::new_with_details(
        &query.url,
        content_length,
        state.client.clone(),
        Some(state.s3_auth.clone()),
        query.aws_profile.clone(),
        shared_cache,
    );

    // Execution (Blocking TIFF decoding and resampling)
    let state_worker = state.clone();
    let tile_bytes = tokio::task::spawn_blocking(move || {
        let _span = tracing::info_span!("spawn_blocking_worker").entered();
        let block_start = std::time::Instant::now();
        use std::io::Seek;
        reader
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|e| format!("Reader seek error: {}", e))?;

        let mut decoder =
            Decoder::new(&mut reader).map_err(|e| format!("TIFF decoder init error: {:?}", e))?;

        // Cache the metadata if this was the first run (extract from IFD0)
        let needs_metadata = cached_entry.as_ref().is_none_or(|e| e.metadata.is_none());
        if needs_metadata {
            if let Ok(metadata) = geotiff::get_cog_metadata(&mut decoder) {
                state_worker
                    .cache
                    .insert_metadata(query.url.clone(), metadata);
            }
            // Reset decoder after metadata extraction to ensure we start next_image traversal from IFD0
            reader
                .seek(std::io::SeekFrom::Start(0))
                .map_err(|e| format!("Reader re-seek error: {}", e))?;
            decoder = Decoder::new(&mut reader)
                .map_err(|e| format!("TIFF decoder re-init error: {:?}", e))?;
        }

        let plan = geotiff::plan_tile_extraction(&mut decoder, z, x, y)
            .map_err(|e| format!("Planning error: {}", e))?;

        let chunk_indices = geotiff::get_required_cache_chunks(&mut decoder, &plan)
            .map_err(|e| format!("Prefetch calculation error: {}", e))?;

        drop(decoder);
        let prefetch_start = std::time::Instant::now();
        reader
            .cache_chunks_concurrently(&chunk_indices)
            .map_err(|e| format!("Prefetch error: {}", e))?;
        let load_ms = prefetch_start.elapsed().as_millis() as u64;

        reader
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|e| format!("Reader re-seek error: {}", e))?;

        let mut decoder = Decoder::new(&mut reader)
            .map_err(|e| format!("TIFF decoder re-init error: {:?}", e))?;

        for _ in 0..plan.ifd_index {
            decoder
                .next_image()
                .map_err(|e| format!("IFD traversal error: {}", e))?;
        }

        let decode_start = std::time::Instant::now();
        let image = geotiff::extract_tile_from_cog(decoder, plan)?;
        let resample_ms = decode_start.elapsed().as_millis() as u64;

        let png_start = std::time::Instant::now();
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| format!("PNG encode error: {}", e))?;

        let encode_ms = png_start.elapsed().as_millis() as u64;
        tracing::info!("Worker completed in {:?}", block_start.elapsed());

        Ok::<(Vec<u8>, u64, u64, u64), String>((out, load_ms, resample_ms, encode_ms))
    })
    .await;

    let state_err1 = state.clone();
    let state_err2 = state.clone();

    let tile_bytes = tile_bytes
        .map_err(|_| {
            if let Ok(mut metrics) = state_err1.metrics.write() {
                metrics.total_errors += 1;
            }
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map_err(|err| {
            println!("Error processing tile: {}", err);
            if let Ok(mut metrics) = state_err2.metrics.write() {
                metrics.total_errors += 1;
            }
            StatusCode::BAD_REQUEST
        })?;

    if let Ok(mut metrics) = state.metrics.write() {
        metrics.total_requests += 1;
        metrics.cumulative_load_time_ms += tile_bytes.1;
        metrics.cumulative_resample_time_ms += tile_bytes.2;
        metrics.cumulative_encode_time_ms += tile_bytes.3;
    }

    tracing::info!("Total get_tile request took {:?}", tile_start.elapsed());
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        tile_bytes.0,
    )
        .into_response())
}

#[tokio::main]
async fn main() {
    let env_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "error".into());

    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    tracing::info!("Starting tile server...");
    let mapping_file = std::env::var("S3_ENDPOINT_MAPPING").ok();

    let state = Arc::new(AppState {
        cache: cache::CogCache::new(),
        client: reqwest::Client::new(),
        s3_auth: just_tile::s3_auth::S3AuthManager::new(mapping_file.as_deref()).await,
        metrics: std::sync::RwLock::new(Metrics::default()),
    });

    let app = Router::new()
        .route("/", get(health_check))
        .route("/metrics", get(metrics_handler))
        .route("/{z}/{x}/{y}", get(get_tile))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
