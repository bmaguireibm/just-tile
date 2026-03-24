use std::io::{Read, Seek};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;
use image::{DynamicImage, RgbaImage, RgbImage, Rgba, Rgb, imageops};
use proj4rs::proj::Proj;
use proj4rs::transform::transform;

const MAX_EXTENT: f64 = 20037508.342789244;

pub fn extract_tile_from_cog<R: Read + Seek>(
    mut reader: Decoder<R>,
    z: u32,
    x: u32,
    y: u32,
) -> Result<DynamicImage, String> {
    let (original_width, original_height) = reader.dimensions().map_err(|e| format!("Dimensions error: {:?}", e))?;

    let res = (MAX_EXTENT * 2.0) / (1u32 << z) as f64;
    let minx = -MAX_EXTENT + (x as f64) * res;
    let miny = MAX_EXTENT - ((y + 1) as f64) * res;
    let maxx = -MAX_EXTENT + ((x + 1) as f64) * res;
    let maxy = MAX_EXTENT - (y as f64) * res;

    // Fetch GeoTIFF Tags (Read from IFD 0 before we traverse)
    let tiepoint = reader.get_tag_f64_vec(Tag::Unknown(33922)).map_err(|e| format!("Missing ModelTiepointTag: {:?}", e))?;
    let pixel_scale = reader.get_tag_f64_vec(Tag::Unknown(33550)).unwrap_or(vec![1.0, 1.0, 1.0]);
    let geo_keys = reader.get_tag_u16_vec(Tag::Unknown(34735)).map_err(|e| format!("Missing GeoKeyDirectoryTag: {:?}", e))?;

    let mut crs_epsg = 0;
    for chunk in geo_keys.chunks(4).skip(1) {
        if chunk[0] == 3072 {
            crs_epsg = chunk[3];
            break;
        }
    }

    if crs_epsg < 32601 || crs_epsg > 32760 {
        return Err(format!("Unsupported EPSG code: {}", crs_epsg));
    }

    let is_south = crs_epsg >= 32700;
    let zone = if is_south { crs_epsg - 32700 } else { crs_epsg - 32600 };

    let proj_source_str = format!("+proj=utm +zone={} {}+datum=WGS84 +units=m +no_defs", zone, if is_south { "+south " } else { "" });
    let proj_target_str = "+proj=merc +a=6378137 +b=6378137 +lat_ts=0.0 +lon_0=0.0 +x_0=0.0 +y_0=0 +k=1.0 +units=m +nadgrids=@null +wktext +no_defs";

    let proj_source = Proj::from_proj_string(&proj_source_str).map_err(|e| format!("Proj4rs error src: {:?}", e))?;
    let proj_target = Proj::from_proj_string(proj_target_str).map_err(|e| format!("Proj4rs error target: {:?}", e))?;

    let mut points = vec![
        (minx, miny, 0.0f64),
        (maxx, miny, 0.0f64),
        (minx, maxy, 0.0f64),
        (maxx, maxy, 0.0f64),
    ];

    transform(&proj_target, &proj_source, &mut points[..])
        .map_err(|e| format!("Transform error: {:?}", e))?;

    let proj_minx = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let proj_maxx = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let proj_miny = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let proj_maxy = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);

    let tie_x = tiepoint[3];
    let tie_y = tiepoint[4];
    let scale_x = pixel_scale[0];
    let scale_y = pixel_scale[1];

    // Pixel bounds at IFD 0 resolution
    let mut px_min = ((proj_minx - tie_x) / scale_x).floor() as i64;
    let mut px_max = ((proj_maxx - tie_x) / scale_x).ceil() as i64;
    let mut py_min = ((tie_y - proj_maxy) / scale_y).floor() as i64;
    let mut py_max = ((tie_y - proj_miny) / scale_y).ceil() as i64;

    let crop_w_0 = (px_max - px_min).max(0) as f64;
    
    // Traverse overviews to find best IFD
    let mut current_width = original_width;
    let mut current_height = original_height;

    if crop_w_0 > 0.0 {
        // Keep shrinking IFDs until the selected crop region represents ~512 pixels
        // This guarantees we only download sparse chunks of low-res overlays instead of massive 10m arrays.
        while crop_w_0 * (current_width as f64 / original_width as f64) > 512.0 {
            if reader.more_images() {
                if reader.next_image().is_ok() {
                    if let Ok((w, h)) = reader.dimensions() {
                        current_width = w;
                        current_height = h;
                        continue;
                    }
                }
            }
            break;
        }
    }

    // Scale bounds to current IFD resolution
    let scale_factor = current_width as f64 / original_width as f64;
    px_min = (px_min as f64 * scale_factor).floor() as i64;
    px_max = (px_max as f64 * scale_factor).ceil() as i64;
    py_min = (py_min as f64 * scale_factor).floor() as i64;
    py_max = (py_max as f64 * scale_factor).ceil() as i64;

    let chunk_dims = reader.chunk_dimensions();
    let tile_w = chunk_dims.0;
    let tile_h = chunk_dims.1;

    if px_max <= 0 || py_max <= 0 || px_min >= current_width as i64 || py_min >= current_height as i64 {
        return Ok(DynamicImage::ImageRgba8(RgbaImage::new(256, 256)));
    }

    let crop_x0 = px_min.max(0) as u32;
    let crop_y0 = py_min.max(0) as u32;
    let crop_x1 = px_max.min(current_width as i64) as u32;
    let crop_y1 = py_max.min(current_height as i64) as u32;

    let crop_w = crop_x1.saturating_sub(crop_x0);
    let crop_h = crop_y1.saturating_sub(crop_y0);

    if crop_w == 0 || crop_h == 0 {
        return Ok(DynamicImage::ImageRgba8(RgbaImage::new(256, 256)));
    }

    let tiles_x_count = (current_width + tile_w - 1) / tile_w;
    let start_col = crop_x0 / tile_w;
    let end_col = (crop_x0 + crop_w - 1) / tile_w;
    let start_row = crop_y0 / tile_h;
    let end_row = (crop_y0 + crop_h - 1) / tile_h;

    let stitched_w = (end_col - start_col + 1) * tile_w;
    let stitched_h = (end_row - start_row + 1) * tile_h;

    let is_rgba = matches!(reader.colortype().map_err(|e| format!("{:?}",e))?, tiff::ColorType::RGBA(_));

    let mut stitched_rgba = RgbaImage::new(stitched_w, stitched_h);
    let mut stitched_rgb = RgbImage::new(stitched_w, stitched_h);

    for row in start_row..=end_row {
        for col in start_col..=end_col {
            let chunk_idx = row * tiles_x_count + col;
            let result = reader.read_chunk(chunk_idx);
            if result.is_err() { continue; } // Handle missing chunks safely for sparse COGs

            let dest_x = (col - start_col) * tile_w;
            let dest_y = (row - start_row) * tile_h;

            if let Ok(DecodingResult::U8(pixels)) = result {
                let channels = if is_rgba { 4 } else { 3 };
                
                for py in 0..tile_h {
                    for px in 0..tile_w {
                        let idx = ((py * tile_w + px) * channels) as usize;
                        if idx + channels as usize <= pixels.len() {
                            if is_rgba {
                                let pixel = Rgba([pixels[idx], pixels[idx+1], pixels[idx+2], if channels==4{pixels[idx+3]}else{255}]);
                                stitched_rgba.put_pixel(dest_x + px, dest_y + py, pixel);
                            } else {
                                let pixel = Rgb([pixels[idx], pixels[idx+1], pixels[idx+2]]);
                                stitched_rgb.put_pixel(dest_x + px, dest_y + py, pixel);
                            }
                        }
                    }
                }
            }
        }
    }

    let stitched_image = if is_rgba { DynamicImage::ImageRgba8(stitched_rgba) } else { DynamicImage::ImageRgb8(stitched_rgb) };
    let offset_x = crop_x0 - (start_col * tile_w);
    let offset_y = crop_y0 - (start_row * tile_h);

    let cropped = imageops::crop_imm(&stitched_image, offset_x, offset_y, crop_w, crop_h).to_image();
    let final_tile = imageops::resize(&cropped, 256, 256, image::imageops::FilterType::Triangle);

    Ok(DynamicImage::ImageRgba8(final_tile))
}
