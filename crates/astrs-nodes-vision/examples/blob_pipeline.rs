//! An end-to-end, offline vision pipeline built entirely from this crate's
//! **plain functions and types** — no [`astrs_operator_api::Operator`], no
//! manifest, no daemon (see [`astrs_nodes_vision::ops::operators`] for the
//! operator-registry half, briefly exercised in step 9 below).
//!
//! Part A (steps 1-8) is a small "find the bright blobs" pipeline: it
//! paints a synthetic scene, round-trips it through this crate's own PNG
//! codec, then runs it through colour conversion, Gaussian blur, Otsu
//! thresholding, morphological opening, connected-component labeling, and
//! bounding-box drawing — the sequence a real camera-to-detections node
//! would run, minus the camera. Part B (step 10) is a short, separate
//! pinhole-camera undistort demonstration.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p astrs-nodes-vision --example blob_pipeline
//! ```

use astrs_nodes_vision::camera::{Distortion, Interpolation, Intrinsics, undistort};
use astrs_nodes_vision::color::to_gray;
use astrs_nodes_vision::components::{Connectivity, label_components};
use astrs_nodes_vision::draw::draw_rect;
use astrs_nodes_vision::morphology::{StructuringElement, open};
use astrs_nodes_vision::png::{FilterStrategy, decode, encode};
use astrs_nodes_vision::resize::resize_bilinear;
use astrs_nodes_vision::sobel::sobel_magnitude;
use astrs_nodes_vision::threshold::{ThresholdMode, threshold_otsu};
use astrs_nodes_vision::{ImageBuffer, PixelFormat};

/// The synthetic scene's width, in pixels.
const SCENE_WIDTH: u32 = 40;

/// The synthetic scene's height, in pixels.
const SCENE_HEIGHT: u32 = 30;

/// The two bright "blobs" painted onto an otherwise-dark scene, as
/// `(top_left, bottom_right)` corners — spaced apart, and away from the
/// frame edge, so [`Connectivity::Four`] keeps them as two separate
/// components and [`morphology::open`](astrs_nodes_vision::morphology::open)
/// has no border effects to fight.
const BLOBS: [((i64, i64), (i64, i64)); 2] = [((3, 3), (10, 9)), ((22, 15), (33, 24))];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scene = paint_synthetic_scene()?;
    println!(
        "1. painted a {SCENE_WIDTH}x{SCENE_HEIGHT} RGB scene with {} blobs",
        BLOBS.len()
    );

    let png_bytes = encode(&scene, FilterStrategy::MinimumSum)?;
    println!("2. encoded to PNG: {} bytes", png_bytes.len());

    let decoded = decode(&png_bytes)?;
    assert_eq!(decoded, scene, "PNG round-trip must be lossless");
    println!("3. decoded back from PNG (byte-for-byte identical to the source)");

    let grey = to_gray(&decoded)?;
    println!(
        "4. converted to {} (from {})",
        grey.format(),
        decoded.format()
    );

    let edges = sobel_magnitude(&grey)?;
    let max_gradient = edges.data().iter().copied().max().unwrap_or(0);
    println!("5. Sobel gradient magnitude computed (peak edge strength: {max_gradient}/255)");

    let (binary, level) = threshold_otsu(&grey, ThresholdMode::Normal)?;
    // This scene's background is exactly grey level 0 and its blobs are
    // exactly grey level 240 -- a clean two-value histogram, so every level
    // in 0..240 ties on Otsu's between-class variance, and `otsu_level`'s
    // documented smallest-wins tie-break picks the boundary itself, 0. The
    // `>` comparison in `threshold_fixed` still separates the two grey
    // levels perfectly (0 is never `> 0`; 240 always is) -- see step 8.
    println!("6. Otsu thresholding chose level {level}");

    let cleaned = open(&binary, StructuringElement::Square(1))?;
    println!("7. morphological opening (3x3 square) applied to clean up speckle noise");

    let (labels, stats) = label_components(&cleaned, Connectivity::Four)?;
    println!(
        "8. connected-component labeling found {} component(s):",
        stats.len()
    );
    for component in &stats {
        println!(
            "   label {:>2}: area={:>4}px  bbox=({}, {}) - ({}, {})  centroid=({:.1}, {:.1})",
            component.label,
            component.area,
            component.min_x,
            component.min_y,
            component.max_x,
            component.max_y,
            component.centroid_x,
            component.centroid_y,
        );
    }
    assert_eq!(
        stats.len(),
        BLOBS.len(),
        "should recover exactly the painted blobs"
    );
    let _ = labels; // the label plane itself isn't needed past this point in this example

    let mut annotated = decoded.clone();
    for component in &stats {
        draw_rect(
            &mut annotated,
            (i64::from(component.min_x), i64::from(component.min_y)),
            (i64::from(component.max_x), i64::from(component.max_y)),
            &[255, 0, 0], // red outline, in the scene's own RGB8 channel order
            false,
        )?;
    }
    let upscaled = resize_bilinear(&annotated, SCENE_WIDTH * 4, SCENE_HEIGHT * 4)?;
    let annotated_png = encode(&upscaled, FilterStrategy::MinimumSum)?;
    println!(
        "9. drew a bounding box per component, upscaled 4x to {}x{}, re-encoded: {} bytes",
        upscaled.width(),
        upscaled.height(),
        annotated_png.len()
    );

    let registry = astrs_nodes_vision::operators()?;
    let mut names: Vec<&str> = registry.names().collect();
    names.sort_unstable();
    println!(
        "10. this crate's operator registry has {} entries: {}",
        registry.len(),
        names.join(", ")
    );
    assert!(registry.contains("ThresholdOperator"));

    demonstrate_undistort()?;

    Ok(())
}

/// Paints [`BLOBS`] as filled white rectangles on an otherwise-black
/// [`PixelFormat::Rgb8`] canvas.
fn paint_synthetic_scene() -> astrs_nodes_vision::Result<ImageBuffer> {
    let mut scene = ImageBuffer::zeroed(PixelFormat::Rgb8, SCENE_WIDTH, SCENE_HEIGHT)?;
    for (top_left, bottom_right) in BLOBS {
        draw_rect(&mut scene, top_left, bottom_right, &[240, 240, 240], true)?;
    }
    Ok(scene)
}

/// Part B: builds a pinhole camera model with a touch of barrel
/// distortion and shows [`undistort_map`](astrs_nodes_vision::camera::undistort_map)'s
/// per-pixel result directly — the part of this crate's pipeline that has
/// nothing to do with [`BLOBS`] at all.
fn demonstrate_undistort() -> Result<(), Box<dyn std::error::Error>> {
    let width = 20u32;
    let height = 20u32;
    let intrinsics = Intrinsics::new(30.0, 30.0, f64::from(width) / 2.0, f64::from(height) / 2.0);
    let distortion = Distortion::new(0.15, 0.0, 0.0, 0.0, 0.0); // mild barrel distortion

    let map = astrs_nodes_vision::camera::undistort_map(width, height, intrinsics, distortion)?;
    // The centre pixel sits exactly on the principal point, which never
    // moves under any distortion (see `Distortion::distort_normalized`'s
    // own doc) -- a fixed point to contrast against a pixel that does move.
    let (center_x, center_y) = (width / 2, height / 2);
    let center_source = map.source_at(center_x, center_y).unwrap_or((0.0, 0.0));
    let corner_source = map.source_at(0, 0).unwrap_or((0.0, 0.0));
    println!(
        "11. undistort map (k1=0.15 barrel distortion) for a {width}x{height} frame: \
         centre pixel ({center_x}, {center_y}) samples source ({:.2}, {:.2}) [unmoved, as expected]; \
         corner pixel (0, 0) samples source ({:.2}, {:.2}) [pulled outward, past the frame edge]",
        center_source.0, center_source.1, corner_source.0, corner_source.1
    );

    // Applying that map is exactly what `undistort` (the convenience
    // wrapper over `undistort_map` + `remap`) does; a corner marker's
    // rectified brightness drops to black precisely because the source
    // coordinate just printed falls outside the original frame.
    let mut frame = ImageBuffer::zeroed(PixelFormat::Mono8, width, height)?;
    frame.put_pixel(0, 0, &[255])?;
    let rectified = undistort(&frame, intrinsics, distortion, Interpolation::Bilinear)?;
    assert_eq!(
        rectified.pixel(0, 0),
        Some(&[0u8][..]),
        "the corner source falls outside the frame"
    );
    println!(
        "    (confirmed: `undistort` renders that corner black, matching the out-of-frame source above)"
    );
    Ok(())
}
