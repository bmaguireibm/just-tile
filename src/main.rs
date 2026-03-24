use just_tile::{http_reader, geotiff};

use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
    routing::get,
    Router,
    http::{StatusCode, header},
};
use serde::Deserialize;
use std::io::Cursor;
use tiff::decoder::Decoder;

#[derive(Deserialize)]
struct TileQuery {
    url: String,
}

async fn health_check() -> &'static str {
    "Tile Server is running"
}

async fn get_tile(
    Path((z, x, y)): Path<(u32, u32, u32)>,
    Query(query): Query<TileQuery>,
) -> Result<Response, StatusCode> {
    
    // Convert to a spawned blocking task because TIFF processing and HTTP range reading is synchronous.
    let tile_bytes = tokio::task::spawn_blocking(move || {
        let mut reader = http_reader::HttpRangeReader::new(&query.url)?;

        let decoder = Decoder::new(&mut reader)
            .map_err(|e| format!("TIFF decode error: {:?}", e))?;

        let image = geotiff::extract_tile_from_cog(decoder, z, x, y)?;

        let mut out = Vec::new();
        image.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
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
    ).into_response())
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(health_check))
        .route("/{z}/{x}/{y}", get(get_tile));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Listening on http://0.0.0.0:3000");
    axum::serve(listener, app).await.unwrap();
}
