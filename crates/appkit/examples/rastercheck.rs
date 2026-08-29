//! Decode an image the way `NSImage` does, and check the TIFF round trip.
//!
//! `-TIFFRepresentation` is a round trip through `appkit::raster` - the guest
//! asks for a TIFF and hands it straight back to `NSBitmapImageRep` - so the
//! encoder and the decoder have to agree. This is that check against a real
//! file, which is how Deltarune's `splash.png` was found to be a BMP.
fn main() {
    let path = std::env::args().nth(1).expect("path");
    let bytes = std::fs::read(&path).expect("readable");
    match appkit::raster::decode(&bytes) {
        Some(b) => {
            println!("decoded {:?}", b);
            let tiff = appkit::raster::encode_tiff(&b);
            let back = appkit::raster::decode(&tiff).expect("round trip");
            println!("round trip {:?} equal={}", back, back == b);
        }
        None => println!("not decodable"),
    }
}
