use image::{DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};
use proj4rs::proj::Proj;
use proj4rs::transform::transform;
use std::io::{Read, Seek};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

/// Maximum extent of the Web Mercator projection in meters.
const MAX_EXTENT: f64 = 20037508.342789244;

/// Metadata extracted from a GeoTIFF header.
#[derive(Debug, Clone)]
pub struct CogMetadata {
    pub width: u32,
    pub height: u32,
    pub tie_x: f64,
    pub tie_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub proj_source_str: String,
}

/// Bounding box in a specific CRS.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
}

/// Extracts metadata from the current IFD of a GeoTIFF.
pub fn get_cog_metadata<R: Read + Seek>(reader: &mut Decoder<R>) -> Result<CogMetadata, String> {
    let (width, height) = reader
        .dimensions()
        .map_err(|e| format!("Dimensions error: {:?}", e))?;

    let tiepoint = reader
        .get_tag_f64_vec(Tag::Unknown(33922))
        .map_err(|e| format!("Missing ModelTiepointTag: {:?}", e))?;
    let pixel_scale = reader
        .get_tag_f64_vec(Tag::Unknown(33550))
        .unwrap_or(vec![1.0, 1.0, 1.0]);
    let geo_keys = reader
        .get_tag_u16_vec(Tag::Unknown(34735))
        .map_err(|e| format!("Missing GeoKeyDirectoryTag: {:?}", e))?;

    let mut crs_epsg = 0;
    for chunk in geo_keys.chunks(4).skip(1) {
        if chunk[0] == 3072 {
            crs_epsg = chunk[3];
            break;
        }
    }

    if !(32601..=32760).contains(&crs_epsg) {
        return Err(format!("Unsupported EPSG code: {}", crs_epsg));
    }

    let is_south = crs_epsg >= 32700;
    let zone = if is_south {
        crs_epsg - 32700
    } else {
        crs_epsg - 32600
    };

    let proj_source_str = format!(
        "+proj=utm +zone={} {}+datum=WGS84 +units=m +no_defs",
        zone,
        if is_south { "+south " } else { "" }
    );

    Ok(CogMetadata {
        width,
        height,
        tie_x: tiepoint[3],
        tie_y: tiepoint[4],
        scale_x: pixel_scale[0],
        scale_y: pixel_scale[1],
        proj_source_str,
    })
}

/// Calculates the Web Mercator extents for a given ZXY tile.
pub fn calculate_mercator_bounds(z: u32, x: u32, y: u32) -> Bounds {
    let res = (MAX_EXTENT * 2.0) / (1u32 << z) as f64;
    Bounds {
        minx: -MAX_EXTENT + (x as f64) * res,
        miny: MAX_EXTENT - ((y + 1) as f64) * res,
        maxx: -MAX_EXTENT + ((x + 1) as f64) * res,
        maxy: MAX_EXTENT - (y as f64) * res,
    }
}

/// Plan for extracting a tile, containing all necessary coordinates and metadata.
pub struct TileExtractionPlan {
    pub ifd_index: u32,
    pub scale_factor: f64,
    pub ifd_corners: [(f64, f64); 4],
    pub start_col: u32,
    pub end_col: u32,
    pub start_row: u32,
    pub end_row: u32,
    pub tw: u32,
    pub th: u32,
    pub current_width: u32,
    pub current_height: u32,
    pub is_rgba: bool,
}

/// Determines the IFD, bounds, and chunks needed for a tile extraction.
pub fn plan_tile_extraction<R: Read + Seek>(
    reader: &mut Decoder<R>,
    z: u32,
    x: u32,
    y: u32,
) -> Result<TileExtractionPlan, String> {
    let meta0 = get_cog_metadata(reader)?;
    let bounds_merc = calculate_mercator_bounds(z, x, y);

    let proj_target_str = "+proj=merc +a=6378137 +b=6378137 +lat_ts=0.0 +lon_0=0.0 +x_0=0.0 +y_0=0 +k=1.0 +units=m +nadgrids=@null +wktext +no_defs";
    let proj_source = Proj::from_proj_string(&meta0.proj_source_str)
        .map_err(|e| format!("Proj4rs error src: {:?}", e))?;
    let proj_target = Proj::from_proj_string(proj_target_str)
        .map_err(|e| format!("Proj4rs error target: {:?}", e))?;

    let mut corners = [
        (bounds_merc.minx, bounds_merc.maxy, 0.0f64),
        (bounds_merc.maxx, bounds_merc.maxy, 0.0f64),
        (bounds_merc.minx, bounds_merc.miny, 0.0f64),
        (bounds_merc.maxx, bounds_merc.miny, 0.0f64),
    ];

    transform(&proj_target, &proj_source, &mut corners[..])
        .map_err(|e| format!("Transform error: {:?}", e))?;

    let px_corners: Vec<(f64, f64)> = corners
        .iter()
        .map(|(ux, uy, _)| {
            (
                (ux - meta0.tie_x) / meta0.scale_x,
                (meta0.tie_y - uy) / meta0.scale_y,
            )
        })
        .collect();

    let pminx = px_corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::INFINITY, f64::min);
    let pmaxx = px_corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::NEG_INFINITY, f64::max);
    let crop_w_0 = (pmaxx - pminx).max(0.0);

    let mut current_width = meta0.width;
    let mut current_height = meta0.height;
    let mut ifd_index = 0;

    while crop_w_0 * (current_width as f64 / meta0.width as f64) > 512.0 {
        if reader.more_images() && reader.next_image().is_ok() {
            let (w, h) = reader.dimensions().map_err(|e| format!("{:?}", e))?;
            current_width = w;
            current_height = h;
            ifd_index += 1;
        } else {
            break;
        }
    }

    let scale_factor = current_width as f64 / meta0.width as f64;
    let ifd_corners_vec: Vec<(f64, f64)> = px_corners
        .iter()
        .map(|(px, py)| (px * scale_factor, py * scale_factor))
        .collect();
    let ifd_corners = [
        ifd_corners_vec[0],
        ifd_corners_vec[1],
        ifd_corners_vec[2],
        ifd_corners_vec[3],
    ];

    let iminx = ifd_corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::INFINITY, f64::min)
        .floor() as i64;
    let imaxx = ifd_corners
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil() as i64;
    let iminy = ifd_corners
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::INFINITY, f64::min)
        .floor() as i64;
    let imaxy = ifd_corners
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil() as i64;

    let (tw, th) = reader.chunk_dimensions();
    let is_rgba = matches!(
        reader.colortype().map_err(|e| format!("{:?}", e))?,
        tiff::ColorType::RGBA(_)
    );

    Ok(TileExtractionPlan {
        ifd_index,
        scale_factor,
        ifd_corners,
        start_col: ((iminx.max(0) as u32).min(current_width.saturating_sub(1))) / tw,
        end_col: ((imaxx.max(0) as u32).min(current_width.saturating_sub(1))) / tw,
        start_row: ((iminy.max(0) as u32).min(current_height.saturating_sub(1))) / th,
        end_row: ((imaxy.max(0) as u32).min(current_height.saturating_sub(1))) / th,
        tw,
        th,
        current_width,
        current_height,
        is_rgba,
    })
}

/// Extracts a 256x256 map tile from a Cloud Optimized GeoTIFF (COG), given a pre-computed plan.
///
/// This function performs two main steps:
/// 1. **Data Loading**: Fetches and stitches the required TIFF chunks into a local buffer.
/// 2. **Resampling**: Maps and interpolates the pixels from the buffer onto the 256x256 target tile
///    using bilinear filtering.
pub fn extract_tile_from_cog<R: Read + Seek>(
    mut reader: Decoder<R>,
    plan: TileExtractionPlan,
) -> Result<DynamicImage, String> {
    // 1. Prepare Buffer: Calculate the dimensions of the area we need to fetch from the COG.
    // This is the bounding box of all required TIFF chunks.
    let buf_w = (plan.end_col - plan.start_col + 1) * plan.tw;
    let buf_h = (plan.end_row - plan.start_row + 1) * plan.th;

    let mut buf_rgba = RgbaImage::new(buf_w, buf_h);
    let mut buf_rgb = RgbImage::new(buf_w, buf_h);

    let tiles_x_count = plan.current_width.div_ceil(plan.tw);
    let tiles_y_count = plan.current_height.div_ceil(plan.th);

    // 2. Fetch Data: Loop through all required chunks and stitch them into the buffer.
    for row in plan.start_row..=plan.end_row {
        for col in plan.start_col..=plan.end_col {
            let chunk_idx = row * tiles_x_count + col;

            // Fetch the desired chunk, returning an error up the chain if decoding fails
            let pixels = match reader.read_chunk(chunk_idx) {
                Ok(DecodingResult::U8(px)) => px,
                Err(tiff::TiffError::FormatError(tiff::TiffFormatError::InconsistentSizesEncountered)) => {
                    // Occurs prominently on GDAL/S3 empty padding chunks due to jpeg misalignments
                    continue;
                }
                Err(e) => return Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
                _ => return Err(format!("Unexpected memory format from chunk {}", chunk_idx)),
            };

            let channels = if plan.is_rgba { 4 } else { 3 };
            let dx = (col - plan.start_col) * plan.tw;
            let dy = (row - plan.start_row) * plan.th;

            let is_right_edge = col == tiles_x_count - 1;
            let is_bottom_edge = row == tiles_y_count - 1;

            let actual_w = if is_right_edge && !plan.current_width.is_multiple_of(plan.tw) {
                (plan.current_width % plan.tw) as usize
            } else {
                plan.tw as usize
            };

            let actual_h = if is_bottom_edge && !plan.current_height.is_multiple_of(plan.th) {
                (plan.current_height % plan.th) as usize
            } else {
                plan.th as usize
            };

            let mut stride_w = actual_w;
            let expected_len = actual_w * actual_h * channels;
            
            if pixels.len() > expected_len {
                let w16 = actual_w.div_ceil(16) * 16;
                let h16 = actual_h.div_ceil(16) * 16;
                if w16 * h16 * channels == pixels.len() {
                    stride_w = w16;
                } else {
                    let w8 = actual_w.div_ceil(8) * 8;
                    let h8 = actual_h.div_ceil(8) * 8;
                    if w8 * h8 * channels == pixels.len() {
                        stride_w = w8;
                    } else if pixels.len() % (actual_h * channels) == 0 {
                        stride_w = pixels.len() / (actual_h * channels);
                    }
                }
            }

            // Copy pixels from the TIFF chunk into our local buffer
            for py in 0..plan.th {
                for px in 0..plan.tw {
                    if px as usize >= actual_w || py as usize >= actual_h {
                        continue; // Truncated edge tiles pad out to undefined bounds
                    }
                    let idx = (py as usize * stride_w + px as usize) * channels;
                    if idx + channels <= pixels.len() {
                        if plan.is_rgba {
                            buf_rgba.put_pixel(
                                dx + px,
                                dy + py,
                                Rgba([
                                    pixels[idx],
                                    pixels[idx + 1],
                                    pixels[idx + 2],
                                    if channels == 4 { pixels[idx + 3] } else { 255 },
                                ]),
                            );
                        } else {
                            buf_rgb.put_pixel(
                                dx + px,
                                dy + py,
                                Rgb([pixels[idx], pixels[idx + 1], pixels[idx + 2]]),
                            );
                        }
                    }
                }
            }
        }
    }

    // 3. Resampling: Perform bilinear interpolation to generate the final 256x256 tile.
    let target = resample_to_tile(&plan, &buf_rgba, &buf_rgb, buf_w, buf_h);

    Ok(DynamicImage::ImageRgba8(target))
}

/// Helper function to resample the fetched buffered data into a 256x256 target tile.
/// Uses bilinear interpolation for smooth visual results.
fn resample_to_tile(
    plan: &TileExtractionPlan,
    buf_rgba: &RgbaImage,
    buf_rgb: &RgbImage,
    buf_w: u32,
    buf_h: u32,
) -> RgbaImage {
    let mut target = RgbaImage::new(256, 256);

    // Corner coordinates in the IFD's pixel space
    let (tl, tr, bl, br) = (
        plan.ifd_corners[0],
        plan.ifd_corners[1],
        plan.ifd_corners[2],
        plan.ifd_corners[3],
    );

    // Origin of the buffer in the IFD's pixel space
    let bx0 = (plan.start_col * plan.tw) as f64;
    let by0 = (plan.start_row * plan.th) as f64;

    for ty in 0..256 {
        for tx in 0..256 {
            // Normalized tile coordinates (0.0 to 1.0)
            let u = tx as f64 / 255.0;
            let v = ty as f64 / 255.0;

            // Bilinear interpolation of the four corners to find the exact source pixel coordinate (sx, sy)
            let sx = (1.0 - u) * (1.0 - v) * tl.0
                + u * (1.0 - v) * tr.0
                + (1.0 - u) * v * bl.0
                + u * v * br.0;
            let sy = (1.0 - u) * (1.0 - v) * tl.1
                + u * (1.0 - v) * tr.1
                + (1.0 - u) * v * bl.1
                + u * v * br.1;

            // Local coordinates within our fetched buffer
            let lx = sx - bx0;
            let ly = sy - by0;

            // Check if we are within the bounds of our fetched buffer
            if lx >= 0.0 && lx < (buf_w as f64 - 1.0) && ly >= 0.0 && ly < (buf_h as f64 - 1.0) {
                let xf = lx.floor() as u32;
                let yf = ly.floor() as u32;
                let fx = lx - xf as f64;
                let fy = ly - yf as f64;

                // Bilinear sampling from the 4 surrounding pixels
                let get_pix = |x: u32, y: u32| -> [f64; 4] {
                    if plan.is_rgba {
                        let p = buf_rgba.get_pixel(x, y);
                        [p[0] as f64, p[1] as f64, p[2] as f64, p[3] as f64]
                    } else {
                        let p = buf_rgb.get_pixel(x, y);
                        [p[0] as f64, p[1] as f64, p[2] as f64, 255.0]
                    }
                };

                let p00 = get_pix(xf, yf);
                let p10 = get_pix(xf + 1, yf);
                let p01 = get_pix(xf, yf + 1);
                let p11 = get_pix(xf + 1, yf + 1);

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
    target
}
