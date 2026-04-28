use image::ImageFormat;
use just_tile::{geotiff, http_reader};
use std::io::Cursor;
use tiff::decoder::Decoder;

#[tokio::test(flavor = "multi_thread")]
async fn test_element84_cog() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let url = "https://e84-earth-search-sentinel-data.s3.us-west-2.amazonaws.com/sentinel-2-c1-l2a/29/U/PV/2026/3/S2A_T29UPV_20260314T113337_L2A/TCI.tif";
    let (z, x, y) = (11, 988, 660);

    println!("Testing against Dublin S3 COG...");
    let client = reqwest::Client::new();
    let cache = std::sync::Arc::new(just_tile::cache::SharedChunkCache::default());
    let mut reader = http_reader::HttpRangeReader::new(url, client, None, None, cache)
        .await
        .expect("Failed to create reader");
    let image = tokio::task::spawn_blocking(move || {
        let mut decoder = Decoder::new(&mut reader).expect("Failed to create decoder");
        let plan = geotiff::plan_tile_extraction(&mut decoder, z, x, y).expect("Failed to plan");

        let chunk_indices = geotiff::get_required_cache_chunks(&mut decoder, &plan).unwrap();
        drop(decoder);
        reader.cache_chunks_concurrently(&chunk_indices).unwrap();

        use std::io::Seek;
        reader.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut decoder = Decoder::new(&mut reader).unwrap();
        for _ in 0..plan.ifd_index {
            decoder.next_image().unwrap();
        }

        geotiff::extract_tile_from_cog(decoder, plan).expect("Failed to extract")
    })
    .await
    .expect("Task panicked");

    assert_eq!(image.width(), 256);
    assert_eq!(image.height(), 256);

    // Load reference tile for comparison
    let ref_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Dublin.png");
    let ref_image = image::open(ref_path).expect("Failed to load reference tile");

    // Convert both to raw bytes for comparison if we want exact match,
    // but since resampling can be tricky with floating point, let's at least check dimensions and some pixel values or MSE.
    // However, the user asked to "match the values to be equal to this too".

    let mut gen_bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut gen_bytes), ImageFormat::Png)
        .unwrap();

    let mut ref_bytes = Vec::new();
    ref_image
        .write_to(&mut Cursor::new(&mut ref_bytes), ImageFormat::Png)
        .unwrap();

    // If they are exactly the same, this will pass.
    // Note: image::open might decode and re-encode, so comparing raw file bytes might be safer if we want bit-perfect.
    let _ref_file_bytes = std::fs::read(ref_path).expect("Failed to read reference file");

    // Check if the generated image matches the reference image.
    let gen_raw = image.to_rgba8().into_raw();
    let ref_raw = ref_image.to_rgba8().into_raw();

    if gen_raw != ref_raw {
        let mut mismatch_count = 0;
        for i in 0..gen_raw.len() {
            if gen_raw[i] != ref_raw[i] {
                if mismatch_count < 10 {
                    println!(
                        "Mismatch at byte {}: gen={}, ref={}",
                        i, gen_raw[i], ref_raw[i]
                    );
                }
                mismatch_count += 1;
            }
        }
        println!(
            "Total mismatched bytes: {} / {}",
            mismatch_count,
            gen_raw.len()
        );
        panic!(
            "Pixels do not match reference tile ({} bytes differ)",
            mismatch_count
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_border_zoom_10() {
    let url = "https://e84-earth-search-sentinel-data.s3.us-west-2.amazonaws.com/sentinel-2-c1-l2a/29/U/PV/2026/3/S2A_T29UPV_20260314T113337_L2A/TCI.tif";
    let client = reqwest::Client::new();
    let cache = std::sync::Arc::new(just_tile::cache::SharedChunkCache::default());
    let mut reader = http_reader::HttpRangeReader::new(url, client.clone(), None, None, cache)
        .await
        .expect("Failed to create reader 1");

    // Evaluate 10/494/332 (which the user reports is skewed)
    let image_result = tokio::task::spawn_blocking(move || {
        let mut decoder = Decoder::new(&mut reader).expect("Failed to create decoder");
        let plan =
            geotiff::plan_tile_extraction(&mut decoder, 10, 494, 332).expect("Failed to plan");

        let chunk_indices = geotiff::get_required_cache_chunks(&mut decoder, &plan).unwrap();
        drop(decoder);
        reader.cache_chunks_concurrently(&chunk_indices).unwrap();

        use std::io::Seek;
        reader.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut decoder = Decoder::new(&mut reader).unwrap();
        for _ in 0..plan.ifd_index {
            decoder.next_image().unwrap();
        }

        geotiff::extract_tile_from_cog(decoder, plan)
    })
    .await
    .unwrap();

    let image = image_result.expect("Tile extraction failed");
    assert_eq!(image.width(), 256);
    assert_eq!(image.height(), 256);

    // Save outputs or analyze raw pixels to assert it's densely filled, preventing black skews
    let gen_raw = image.to_rgba8().into_raw();
    let bright_pixels = gen_raw.iter().filter(|&&p| p > 10).count();

    // Ensure > 20% of the image output isn't absolute black or heavily skewed transparency
    assert!(
        bright_pixels > 50000,
        "Image contains anomalous amounts of black padded noise, which is expected by the skew bug"
    );
}
