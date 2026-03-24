use just_tile::{http_reader, geotiff};
use tiff::decoder::Decoder;

#[test]
fn test_element84_cog() {
    let url = "https://e84-earth-search-sentinel-data.s3.us-west-2.amazonaws.com/sentinel-2-c1-l2a/29/U/PV/2026/3/S2A_T29UPV_20260314T113337_L2A/TCI.tif";
    println!("Testing against Dublin S3 COG...");
    let mut reader = http_reader::HttpRangeReader::new(url).unwrap();
    let decoder = Decoder::new(&mut reader).unwrap();
    
    // Extents for Dublin
    let image = geotiff::extract_tile_from_cog(decoder, 11, 988, 660).unwrap();
    
    assert_eq!(image.width(), 256);
    assert_eq!(image.height(), 256);
}
