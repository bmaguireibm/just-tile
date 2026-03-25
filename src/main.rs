use just_tile::{geotiff, http_reader, cache};

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use std::io::Cursor;
use tiff::decoder::Decoder;
use std::sync::Arc;

#[derive(Deserialize)]
struct TileQuery {
    url: String,
}

struct AppState {
    cache: cache::CogCache,
    client: reqwest::Client,
}

async fn health_check() -> &'static str {
    "Tile Server is running"
}

async fn get_tile(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u32, u32, u32)>,
    Query(query): Query<TileQuery>,
) -> Result<Response, StatusCode> {
    // Attempt to use cached metadata
    let cached_entry = state.cache.get(&query.url);

    let mut reader = if let Some(ref entry) = cached_entry {
        http_reader::HttpRangeReader::new_with_details(
            &query.url,
            entry.content_length,
            state.client.clone(),
        )
    } else {
        http_reader::HttpRangeReader::new(&query.url, state.client.clone())
            .await
            .map_err(|e| {
                println!("Reader init error: {}", e);
                StatusCode::BAD_REQUEST
            })?
    };

    // Phase 1: Planning (Determine which chunks we need)
    // We need a Decoder to plan, but planning is fast and doesn't read much except headers.
    let plan = {
        let mut decoder = Decoder::new(&mut reader)
            .map_err(|e| {
                println!("TIFF plan error: {:?}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        geotiff::plan_tile_extraction(&mut decoder, z, x, y).map_err(|e| {
            println!("Planning error: {}", e);
            StatusCode::BAD_REQUEST
        })?
    };

    // Phase 2: Parallel Pre-fetching
    let tiles_x_count = plan.current_width.div_ceil(plan.tw);
    let mut chunk_indices = Vec::new();
    for row in plan.start_row..=plan.end_row {
        for col in plan.start_col..=plan.end_col {
            chunk_indices.push((row * tiles_x_count + col) as u64);
        }
    }

    // Also pre-fetch the very beginning of the file if not already there (for headers)
    // and potentially other metadata chunks. But the plan already moved the reader.
    reader.prefetch_chunks(chunk_indices).await.map_err(|e| {
        println!("Prefetch error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Phase 3: Execution (Blocking TIFF decoding and resampling)
    let tile_bytes = tokio::task::spawn_blocking(move || {
        use std::io::Seek;
        reader.seek(std::io::SeekFrom::Start(0))
            .map_err(|e| format!("Reader seek error: {}", e))?;

        let mut decoder = Decoder::new(&mut reader)
            .map_err(|e| format!("TIFF decoder init error: {:?}", e))?;

        // Cache the metadata if this was the first run (extract from IFD0)
        if cached_entry.is_none() {
            if let Ok(metadata) = geotiff::get_cog_metadata(&mut decoder) {
                state.cache.insert(query.url.clone(), cache::CacheEntry {
                    content_length: reader.content_length(),
                    metadata,
                });
            }
            // Reset decoder after metadata extraction to ensure we start next_image traversal from IFD0
            reader.seek(std::io::SeekFrom::Start(0))
                .map_err(|e| format!("Reader re-seek error: {}", e))?;
            decoder = Decoder::new(&mut reader)
                .map_err(|e| format!("TIFF decoder re-init error: {:?}", e))?;
        }

        // Advance to the optimal IFD
        for _ in 0..plan.ifd_index {
            decoder.next_image().map_err(|e| format!("TIFF IFD traversal error: {:?}", e))?;
        }

        let image = geotiff::extract_tile_from_cog(decoder, plan)?;

        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| format!("PNG encode error: {:?}", e))?;

        Ok::<Vec<u8>, String>(out)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|err| {
        println!("Error processing tile: {}", err);
        StatusCode::BAD_REQUEST
    })?;

    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        tile_bytes,
    )
        .into_response())
}

#[tokio::main]
async fn main() {
    let state = Arc::new(AppState {
        cache: cache::CogCache::new(),
        client: reqwest::Client::new(),
    });

    let app = Router::new()
        .route("/", get(health_check))
        .route("/{z}/{x}/{y}", get(get_tile))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
