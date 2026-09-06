# astrs-nodes-vision

Ready-made AstRS vision nodes and operators: image codecs, resize, colour
conversion, filtering, and camera geometry.

Camera frames arrive in whatever format the driver produces and are wanted
in whatever format, size, and state the consumer expects. This crate ships
that plumbing once, over an owned pixel buffer (`ImageBuffer`) built on
`astrs-data`'s columnar `Image` payloads:

- **Colour** (`color`): conversion between mono8/rgb8/bgr8/rgba8/yuyv/uyvy,
  pivoting through RGB8.
- **Geometry** (`resize`, `draw`): nearest-neighbour and bilinear resizing;
  line/rectangle/circle drawing (no text — see the module doc).
- **Filtering** (`blur`, `sobel`): separable Gaussian blur; Sobel gradients
  and gradient magnitude.
- **Segmentation** (`threshold`, `morphology`, `components`): fixed and
  Otsu thresholding; erode/dilate/open/close; connected-component labeling
  with per-component area/bbox/centroid.
- **Camera** (`camera`): pinhole intrinsics, plumb-bob (Brown-Conrady)
  distortion, and precomputed undistort maps.
- **Codec** (`png`): a from-scratch PNG encoder/decoder (`IHDR`/`PLTE`/
  `IDAT`/`IEND`, all five scanline filters) over `oxiarc-deflate` — no
  `libpng`, no `flate2`, no `image`-crate C codecs anywhere in the graph.
  JPEG is explicitly out of scope; carry one through unopened as a
  `std/media/v1/CompressedImage` instead.

Every op above is a plain, directly callable function; where it reduces to
a handful of config values it is *also* an `astrs_operator_api::Operator`,
all collected by `ops::operators()` for a runtime host to register by name.

Format contracts are uniform throughout: single-channel ops (`threshold`,
`sobel`, `morphology`, `components`) require `PixelFormat::Mono8`;
geometric/filtering ops (`resize`, `blur`, `draw`, `color`'s own
conversions) accept any fully-sampled format, refused only for the two
packed 4:2:2 formats (`Yuyv`/`Uyvy`) — convert with `color` first.

See `examples/blob_pipeline.rs` (`cargo run -p astrs-nodes-vision --example
blob_pipeline`) for an end-to-end walkthrough: paint a synthetic scene,
round-trip it through the PNG codec, blur/threshold/clean/label it, draw a
box around each detected blob, and (separately) undistort a frame through a
pinhole-plus-plumb-bob camera model.

## Example

```rust
use astrs_nodes_vision::color::to_gray;
use astrs_nodes_vision::threshold::{ThresholdMode, threshold_otsu};
use astrs_nodes_vision::{ImageBuffer, PixelFormat};

// A 4x1 RGB strip: two dark pixels, two bright ones.
let strip = ImageBuffer::new(
    PixelFormat::Rgb8,
    4,
    1,
    vec![10, 10, 10, /**/ 20, 20, 20, /**/ 220, 220, 220, /**/ 230, 230, 230],
)?;

let grey = to_gray(&strip)?;
let (binary, level) = threshold_otsu(&grey, ThresholdMode::Normal)?;
assert!((20..220).contains(&level));
assert_eq!(binary.data(), &[0, 0, 255, 255]);
# Ok::<(), astrs_nodes_vision::VisionError>(())
```

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
