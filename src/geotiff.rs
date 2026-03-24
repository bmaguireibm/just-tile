use std::io::{Read, Seek};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;
use image::{DynamicImage, RgbaImage, RgbImage, Rgba, Rgb};
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

    // Fetch GeoTIFF Tags
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

    // We want to transform the 4 corners of the Mercator tile to UTM
    let mut corners = vec![
        (minx, maxy, 0.0f64), // Top-Left
        (maxx, maxy, 0.0f64), // Top-Right
        (minx, miny, 0.0f64), // Bottom-Left
        (maxx, miny, 0.0f64), // Bottom-Right
    ];

    transform(&proj_target, &proj_source, &mut corners[..])
        .map_err(|e| format!("Transform error: {:?}", e))?;

    let tie_x = tiepoint[3];
    let tie_y = tiepoint[4];
    let scale_x = pixel_scale[0];
    let scale_y = pixel_scale[1];

    // Convert corners to IFD0 pixel coordinates (floats)
    let px_corners: Vec<(f64, f64)> = corners.iter().map(|(ux, uy, _)| {
        ((ux - tie_x) / scale_x, (tie_y - uy) / scale_y)
    }).collect();

    let pminx = px_corners.iter().map(|(x, _)| *x).fold(f64::INFINITY, f64::min);
    let pmaxx = px_corners.iter().map(|(x, _)| *x).fold(f64::NEG_INFINITY, f64::max);
    let _pminy = px_corners.iter().map(|(_, y)| *y).fold(f64::INFINITY, f64::min);
    let _pmaxy = px_corners.iter().map(|(_, y)| *y).fold(f64::NEG_INFINITY, f64::max);

    // Bounding width at IFD 0
    let crop_w_0 = (pmaxx - pminx).max(0.0);
    
    // Select IFD
    let mut current_width = original_width;
    let _current_height = original_height;

    // Use a slightly larger threshold for IFD selection (e.g. 512)
    while crop_w_0 * (current_width as f64 / original_width as f64) > 512.0 {
        if reader.more_images() {
            if reader.next_image().is_ok() {
                if let Ok((w, _)) = reader.dimensions() {
                    current_width = w;
                    continue;
                }
            }
        }
        break;
    }

    let scale_factor = current_width as f64 / original_width as f64;
    
    // Corner coordinates in current IFD pixels
    let ifd_corners: Vec<(f64, f64)> = px_corners.iter().map(|(px, py)| {
        (px * scale_factor, py * scale_factor)
    }).collect();

    let iminx = ifd_corners.iter().map(|(x, _)| *x).fold(f64::INFINITY, f64::min).floor() as i64;
    let imaxx = ifd_corners.iter().map(|(x, _)| *x).fold(f64::NEG_INFINITY, f64::max).ceil() as i64;
    let iminy = ifd_corners.iter().map(|(_, y)| *y).fold(f64::INFINITY, f64::min).floor() as i64;
    let imaxy = ifd_corners.iter().map(|(_, y)| *y).fold(f64::NEG_INFINITY, f64::max).ceil() as i64;

    let chunk_dims = reader.chunk_dimensions();
    let tw = chunk_dims.0;
    let th = chunk_dims.1;

    let ifd_w = current_width;
    let ifd_h = (original_height as f64 * scale_factor) as u32; // Approx, but we got dimensions earlier potentially

    let start_col = (iminx.max(0) as u32) / tw;
    let end_col = ((imaxx.max(0) as u32).min(ifd_w - 1)) / tw;
    let start_row = (iminy.max(0) as u32) / th;
    let end_row = ((imaxy.max(0) as u32).min(ifd_h - 1)) / th;

    let buf_w = (end_col - start_col + 1) * tw;
    let buf_h = (end_row - start_row + 1) * th;

    let is_rgba = matches!(reader.colortype().map_err(|e| format!("{:?}",e))?, tiff::ColorType::RGBA(_));
    let mut buf_rgba = RgbaImage::new(buf_w, buf_h);
    let mut buf_rgb = RgbImage::new(buf_w, buf_h);

    let tiles_x_count = (ifd_w + tw - 1) / tw;

    for row in start_row..=end_row {
        for col in start_col..=end_col {
            let chunk_idx = row * tiles_x_count + col;
            if let Ok(DecodingResult::U8(pixels)) = reader.read_chunk(chunk_idx) {
                let channels = if is_rgba { 4 } else { 3 };
                let dx = (col - start_col) * tw;
                let dy = (row - start_row) * th;
                for py in 0..th {
                    for px in 0..tw {
                        let idx = ((py * tw + px) * channels) as usize;
                        if idx + channels as usize <= pixels.len() {
                            if is_rgba {
                                buf_rgba.put_pixel(dx + px, dy + py, Rgba([pixels[idx], pixels[idx+1], pixels[idx+2], if channels==4{pixels[idx+3]}else{255}]));
                            } else {
                                buf_rgb.put_pixel(dx + px, dy + py, Rgb([pixels[idx], pixels[idx+1], pixels[idx+2]]));
                            }
                        }
                    }
                }
            }
        }
    }

    // Now sample the target 256x256 tile by interpolating between corners
    let mut target = RgbaImage::new(256, 256);
    let tl = ifd_corners[0];
    let tr = ifd_corners[1];
    let bl = ifd_corners[2];
    let br = ifd_corners[3];

    let bx0 = (start_col * tw) as f64;
    let by0 = (start_row * th) as f64;

    for ty in 0..256 {
        for tx in 0..256 {
            let u = tx as f64 / 255.0;
            let v = ty as f64 / 255.0;

            // Bilinear interpolation between corners
            let sx = (1.0 - u) * (1.0 - v) * tl.0 + u * (1.0 - v) * tr.0 + (1.0 - u) * v * bl.0 + u * v * br.0;
            let sy = (1.0 - u) * (1.0 - v) * tl.1 + u * (1.0 - v) * tr.1 + (1.0 - u) * v * bl.1 + u * v * br.1;

            // Local coordinates in our stitched buffer
            let lx = sx - bx0;
            let ly = sy - by0;

            if lx >= 0.0 && lx < (buf_w as f64 - 1.0) && ly >= 0.0 && ly < (buf_h as f64 - 1.0) {
                // Bilinear sample from buffer
                let xf = lx.floor() as u32;
                let yf = ly.floor() as u32;
                let xc = xf + 1;
                let yc = yf + 1;
                let fx = lx - xf as f64;
                let fy = ly - yf as f64;

                let get_pix = |x: u32, y: u32| -> [f64; 4] {
                    if is_rgba {
                        let p = buf_rgba.get_pixel(x, y);
                        [p[0] as f64, p[1] as f64, p[2] as f64, p[3] as f64]
                    } else {
                        let p = buf_rgb.get_pixel(x, y);
                        [p[0] as f64, p[1] as f64, p[2] as f64, 255.0]
                    }
                };

                let p00 = get_pix(xf, yf);
                let p10 = get_pix(xc, yf);
                let p01 = get_pix(xf, yc);
                let p11 = get_pix(xc, yc);

                let mut final_rgba = [0u8; 4];
                for i in 0..4 {
                    let val = (1.0 - fx) * (1.0 - fy) * p00[i]
                            + fx * (1.0 - fy) * p10[i]
                            + (1.0 - fx) * fy * p01[i]
                            + fx * fy * p11[i];
                    final_rgba[i] = val.round().clamp(0.0, 255.0) as u8;
                }
                target.put_pixel(tx, ty, Rgba(final_rgba));
            }
        }
    }

    Ok(DynamicImage::ImageRgba8(target))
}
