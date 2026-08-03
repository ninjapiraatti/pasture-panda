use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use image::{
    codecs::jpeg::JpegEncoder, metadata::Orientation, ImageDecoder, ImageFormat, ImageReader,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct ConversionResult {
    success: bool,
    input_path: String,
    output_path: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchConversionResult {
    total: usize,
    succeeded: usize,
    failed: usize,
    results: Vec<ConversionResult>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImageInfo {
    path: String,
    name: String,
    width: u32,
    height: u32,
    format: String,
    size_bytes: u64,
}

/// A crop region expressed as percentages of the source image, so one rectangle can apply
/// to a batch of differently sized images.
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub struct CropRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl CropRect {
    /// A crop covering the whole frame is a no-op worth skipping.
    fn is_full_frame(&self) -> bool {
        self.x <= 0.0 && self.y <= 0.0 && self.width >= 100.0 && self.height >= 100.0
    }

    /// Resolves to pixel bounds inside a `w` x `h` image.
    ///
    /// Everything is clamped to stay in bounds and to keep at least one pixel, because the
    /// rectangle comes from the UI and `crop_imm` panics on out-of-range values.
    fn to_pixels(self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        if w == 0 || h == 0 {
            return None;
        }

        let frac = |v: f32| f32::from(v.is_finite()) * v.clamp(0.0, 100.0) / 100.0;
        let x = (frac(self.x) * w as f32).round() as u32;
        let y = (frac(self.y) * h as f32).round() as u32;
        let cw = (frac(self.width) * w as f32).round() as u32;
        let ch = (frac(self.height) * h as f32).round() as u32;

        // Leave room for at least one pixel of image after the offset.
        let x = x.min(w - 1);
        let y = y.min(h - 1);
        let cw = cw.max(1).min(w - x);
        let ch = ch.max(1).min(h - y);

        Some((x, y, cw, ch))
    }
}

/// Target size for the output.
///
/// `preserve_aspect` is the default: the image is fitted inside whichever bounds are given.
/// With it off, each axis is set independently and the image is stretched — "free resizing".
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub struct ResizeOptions {
    width: Option<u32>,
    height: Option<u32>,
    preserve_aspect: bool,
    no_upscale: bool,
}

impl ResizeOptions {
    /// Target dimensions for a `w` x `h` input, or `None` when this is a no-op.
    fn target_for(&self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w == 0 || h == 0 || (self.width.is_none() && self.height.is_none()) {
            return None;
        }

        let (tw, th) = if self.preserve_aspect {
            // One scale factor for both axes: the tightest of the given bounds.
            let scale = match (self.width, self.height) {
                (Some(bw), None) => f64::from(bw) / f64::from(w),
                (None, Some(bh)) => f64::from(bh) / f64::from(h),
                (Some(bw), Some(bh)) => {
                    (f64::from(bw) / f64::from(w)).min(f64::from(bh) / f64::from(h))
                }
                (None, None) => unreachable!("guarded above"),
            };
            let scale = if self.no_upscale { scale.min(1.0) } else { scale };
            (
                ((f64::from(w) * scale).round() as u32).max(1),
                ((f64::from(h) * scale).round() as u32).max(1),
            )
        } else {
            // Free resize: an axis left blank keeps the source's own size.
            let mut tw = self.width.unwrap_or(w).max(1);
            let mut th = self.height.unwrap_or(h).max(1);
            if self.no_upscale {
                tw = tw.min(w);
                th = th.min(h);
            }
            (tw, th)
        };

        if (tw, th) == (w, h) {
            None
        } else {
            Some((tw, th))
        }
    }
}

/// The dimensions an input of `w` x `h` will end up with. Pure maths, no decoding, so the UI
/// can call this on every keystroke — and so there is exactly one implementation of the rules
/// rather than one in Rust and a drifting copy in TypeScript.
fn planned_dimensions(
    w: u32,
    h: u32,
    crop: Option<&CropRect>,
    resize: Option<&ResizeOptions>,
) -> (u32, u32) {
    let (mut w, mut h) = (w, h);

    if let Some(crop) = crop {
        if !crop.is_full_frame() {
            if let Some((_, _, cw, ch)) = crop.to_pixels(w, h) {
                (w, h) = (cw, ch);
            }
        }
    }

    if let Some(resize) = resize {
        if let Some((tw, th)) = resize.target_for(w, h) {
            (w, h) = (tw, th);
        }
    }

    (w, h)
}

/// Applies crop then resize. Order matters: cropping first means the resize bounds describe
/// the visible region, which is what someone adjusting both would expect.
fn apply_transforms(
    mut img: image::DynamicImage,
    crop: Option<&CropRect>,
    resize: Option<&ResizeOptions>,
) -> image::DynamicImage {
    if let Some(crop) = crop {
        if !crop.is_full_frame() {
            if let Some((x, y, cw, ch)) = crop.to_pixels(img.width(), img.height()) {
                img = img.crop_imm(x, y, cw, ch);
            }
        }
    }

    if let Some(resize) = resize {
        if let Some((tw, th)) = resize.target_for(img.width(), img.height()) {
            // Lanczos3 is the best of the crate's filters for downscaling photographs, which
            // is what this is overwhelmingly used for.
            img = img.resize_exact(tw, th, image::imageops::FilterType::Lanczos3);
        }
    }

    img
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConversionOptions {
    format: String,
    quality: u8,
    output_mode: OutputMode,
    output_folder: Option<String>,
    #[serde(default)]
    crop: Option<CropRect>,
    #[serde(default)]
    resize: Option<ResizeOptions>,
    /// Carry Exif/ICC from the source into the output where the container allows it.
    /// Defaults to true so that stripping metadata is always a deliberate choice.
    #[serde(default = "default_preserve_metadata")]
    preserve_metadata: bool,
}

fn default_preserve_metadata() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    SameFolder,
    CustomFolder,
    ReplaceOriginal,
}

/// True for the orientations that transpose the image, and so swap width and height.
fn orientation_swaps_axes(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

fn read_image_info(path: &str) -> Result<ImageInfo, String> {
    // Only the header is needed for width/height. Fully decoding here made adding a few
    // hundred files needlessly slow and held every decoded buffer in memory at once.
    let mut decoder = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| format!("Failed to open image: {}", e))?
        .into_decoder()
        .map_err(|e| format!("Failed to read image: {}", e))?;

    let (raw_width, raw_height) = decoder.dimensions();

    // Conversion applies EXIF orientation, so report the dimensions the output will
    // actually have rather than the stored ones.
    let orientation = decoder
        .orientation()
        .unwrap_or(Orientation::NoTransforms);
    let (width, height) = if orientation_swaps_axes(orientation) {
        (raw_height, raw_width)
    } else {
        (raw_width, raw_height)
    };

    let metadata = fs::metadata(path).map_err(|e| format!("Failed to read metadata: {}", e))?;

    let path_obj = Path::new(path);
    let name = path_obj
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let format = path_obj
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("unknown")
        .to_uppercase();

    Ok(ImageInfo {
        path: path.to_string(),
        name,
        width,
        height,
        format,
        size_bytes: metadata.len(),
    })
}

#[tauri::command]
async fn get_image_info(path: String) -> Result<ImageInfo, String> {
    tauri::async_runtime::spawn_blocking(move || read_image_info(&path))
        .await
        .map_err(|e| format!("Failed to read image info: {}", e))?
}

#[tauri::command]
async fn get_images_info(paths: Vec<String>) -> Vec<Result<ImageInfo, String>> {
    tauri::async_runtime::spawn_blocking(move || {
        paths.par_iter().map(|p| read_image_info(p)).collect()
    })
    .await
    .unwrap_or_default()
}

/// Resolves the destination for one input.
///
/// `reserved` accumulates every destination handed out so far in this batch. Because
/// conversion runs in parallel, `output_path.exists()` alone is no longer enough: two
/// inputs planned at the same time would both see a free path and then race for it.
fn get_output_path(
    input_path: &str,
    options: &ConversionOptions,
    reserved: &mut HashSet<PathBuf>,
) -> Result<String, String> {
    let input = Path::new(input_path);
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("Invalid input filename")?;

    let extension = match options.format.to_lowercase().as_str() {
        "jpeg" | "jpg" => "jpg",
        "png" => "png",
        "webp" => "webp",
        "avif" => "avif",
        "gif" => "gif",
        "bmp" => "bmp",
        "tiff" => "tiff",
        other => return Err(format!("Unsupported output format: {}", other)),
    };

    let output_dir = match &options.output_mode {
        OutputMode::ReplaceOriginal | OutputMode::SameFolder => input
            .parent()
            .ok_or("Cannot determine parent directory")?
            .to_path_buf(),
        OutputMode::CustomFolder => {
            let folder = options
                .output_folder
                .as_ref()
                .ok_or("Custom folder not specified")?;
            Path::new(folder).to_path_buf()
        }
    };

    let replacing = matches!(options.output_mode, OutputMode::ReplaceOriginal);

    // For replace mode with same format, we'll overwrite
    let same_format_replace = replacing && {
        let input_ext = input
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let input_ext_normalized = if input_ext == "jpeg" { "jpg" } else { &input_ext };
        input_ext_normalized == extension
    };

    let mut output_path = if same_format_replace {
        input.to_path_buf()
    } else {
        output_dir.join(format!("{}.{}", stem, extension))
    };

    if replacing {
        // Replace mode has no renaming to fall back on, so two inputs landing on the
        // same destination would interleave their writes. Fail the second one instead.
        if reserved.contains(&output_path) {
            return Err(format!(
                "Another file in this batch already writes to {}",
                output_path.display()
            ));
        }
    } else {
        // Avoid overwriting anything on disk, or anything else in this batch.
        let mut counter = 1;
        while output_path.exists() || reserved.contains(&output_path) {
            output_path = output_dir.join(format!("{}_{}.{}", stem, counter, extension));
            counter += 1;
        }
    }

    let resolved = output_path
        .to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "Invalid output path".to_string())?;

    reserved.insert(output_path);
    Ok(resolved)
}

/// Reserves a destination for every input up front, single-threaded, so the parallel
/// conversion pass never has to consult the filesystem to pick a name.
fn plan_output_paths(paths: &[String], options: &ConversionOptions) -> Vec<Result<String, String>> {
    let mut reserved = HashSet::new();
    paths
        .iter()
        .map(|p| get_output_path(p, options, &mut reserved))
        .collect()
}

/// Raw metadata blocks lifted off the source image, to be re-attached to the output.
///
/// Both are opaque byte blobs: `exif` is a TIFF-structured Exif block (starting with the
/// `II*\0`/`MM\0*` byte-order marker, no `Exif\0\0` prefix) and `icc` is an ICC profile.
#[derive(Debug, Default, Clone)]
struct SourceMetadata {
    exif: Option<Vec<u8>>,
    icc: Option<Vec<u8>>,
}

impl SourceMetadata {
    fn is_empty(&self) -> bool {
        self.exif.is_none() && self.icc.is_none()
    }
}

fn decode_oriented(input_path: &str) -> Result<(image::DynamicImage, SourceMetadata), String> {
    let mut decoder = ImageReader::open(input_path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;

    // Read orientation and metadata before the decoder is consumed. The `image` crate does
    // not apply orientation automatically, which is why unrotated iPhone photos used to
    // come out sideways.
    let orientation = decoder
        .orientation()
        .unwrap_or(Orientation::NoTransforms);

    // Metadata is best-effort: a source that cannot report it should still convert.
    let mut exif = decoder.exif_metadata().ok().flatten();
    let icc = decoder.icc_profile().ok().flatten();

    // Critical: the rotation is baked into the pixels below, so the Exif orientation tag
    // has to be reset to "no transforms". Carrying the original tag forward would make
    // every Exif-aware viewer rotate the image a second time.
    if let Some(chunk) = exif.as_mut() {
        let _ = Orientation::remove_from_exif_chunk(chunk);
    }

    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok((img, SourceMetadata { exif, icc }))
}

/// Re-attaches `meta` to already-encoded image bytes.
///
/// Only the JPEG, PNG and WebP containers can carry Exif/ICC here; anything else is
/// returned unchanged. Callers that need to know whether metadata survived should consult
/// [`format_carries_metadata`].
fn attach_metadata(encoded: Vec<u8>, meta: &SourceMetadata) -> Vec<u8> {
    use img_parts::{ImageEXIF, ImageICC};

    if meta.is_empty() {
        return encoded;
    }

    // Bytes is reference-counted, so this clone is cheap and lets us fall back to the
    // original buffer when the container is not one img-parts understands.
    let bytes = img_parts::Bytes::from(encoded);

    let mut image = match img_parts::DynImage::from_bytes(bytes.clone()) {
        Ok(Some(image)) => image,
        // Unsupported container, or bytes img-parts could not parse. Either way the
        // encoded image itself is fine — it just travels without metadata.
        Ok(None) | Err(_) => return bytes.to_vec(),
    };

    if let Some(exif) = &meta.exif {
        image.set_exif(Some(img_parts::Bytes::from(exif.clone())));
    }
    if let Some(icc) = &meta.icc {
        image.set_icc_profile(Some(img_parts::Bytes::from(icc.clone())));
    }

    if let img_parts::DynImage::Png(png) = &mut image {
        move_png_exif_before_idat(png);
    }

    image.encoder().bytes().to_vec()
}

/// Moves the `eXIf` chunk ahead of the first `IDAT`.
///
/// `img-parts` appends `eXIf` just before `IEND`. The PNG spec allows it either side of the
/// image data, but decoders commonly only surface ancillary chunks they meet before `IDAT`
/// — the `image` crate included — so metadata written after it reads back as absent.
fn move_png_exif_before_idat(png: &mut img_parts::png::Png) {
    const CHUNK_IDAT: [u8; 4] = *b"IDAT";
    const CHUNK_EXIF: [u8; 4] = *b"eXIf";

    let chunks = png.chunks_mut();

    let Some(exif_at) = chunks.iter().position(|c| c.kind() == CHUNK_EXIF) else {
        return;
    };
    let Some(idat_at) = chunks.iter().position(|c| c.kind() == CHUNK_IDAT) else {
        return;
    };
    if exif_at < idat_at {
        return;
    }

    let exif = chunks.remove(exif_at);
    chunks.insert(idat_at, exif);
}

/// Whether an output format can carry Exif/ICC metadata through this app.
///
/// This tracks what `img-parts` supports, not what the file format permits: TIFF and AVIF
/// both allow Exif in principle, but nothing here writes it.
fn format_carries_metadata(format: &str) -> bool {
    matches!(
        format.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "webp"
    )
}

/// libwebp's hard limit on either dimension.
const WEBP_MAX_DIMENSION: u32 = 16383;

/// Lossy WebP via libwebp. The `image` crate only offers a lossless WebP encoder, which
/// made JPEG -> WebP conversions *larger* than their input — the opposite of the point.
///
/// `quality` is the UI slider, 1-100. 100 switches to lossless, matching how `cwebp`
/// treats the top of the range.
fn encode_webp(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let (width, height) = (img.width(), img.height());
    if width > WEBP_MAX_DIMENSION || height > WEBP_MAX_DIMENSION {
        return Err(format!(
            "WebP cannot store {}x{}: maximum dimension is {}",
            width, height, WEBP_MAX_DIMENSION
        ));
    }

    // A quality of 100 means lossless, matching how cwebp treats the top of the range.
    let run = |encoder: webp::Encoder| {
        if quality >= 100 {
            encoder.encode_lossless()
        } else {
            encoder.encode(f32::from(quality))
        }
    };

    // from_rgb/from_rgba rather than from_image: the latter rejects 16-bit inputs, which
    // are ordinary in PNG and TIFF sources. Dropping the alpha channel when there isn't
    // one keeps the encoded file smaller.
    let encoded = if img.color().has_alpha() {
        let buffer = img.to_rgba8();
        run(webp::Encoder::from_rgba(buffer.as_raw(), width, height))
    } else {
        let buffer = img.to_rgb8();
        run(webp::Encoder::from_rgb(buffer.as_raw(), width, height))
    };

    // libwebp signals failure by returning an empty buffer rather than an error.
    if encoded.is_empty() {
        return Err("WebP encoding failed".to_string());
    }

    Ok(encoded.to_vec())
}

/// Encodes into memory rather than straight to disk, because metadata is spliced into the
/// encoded container afterwards. Costs one encoded image per worker thread, which is small
/// next to the decoded buffer already being held.
fn encode_to_bytes(
    img: &image::DynamicImage,
    options: &ConversionOptions,
) -> Result<Vec<u8>, String> {
    let format = options.format.to_lowercase();
    let img = normalise_for_format(img, &format);
    let img = img.as_ref();

    let write_with = |format: ImageFormat| {
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, format).map_err(|e| e.to_string())?;
        Ok(buf.into_inner())
    };

    match format.as_str() {
        "jpg" | "jpeg" => {
            let mut buf = Vec::new();
            let encoder = JpegEncoder::new_with_quality(&mut buf, options.quality);
            img.write_with_encoder(encoder).map_err(|e| e.to_string())?;
            Ok(buf)
        }
        "webp" => encode_webp(img, options.quality),
        "png" => write_with(ImageFormat::Png),
        "avif" => write_with(ImageFormat::Avif),
        "gif" => write_with(ImageFormat::Gif),
        "bmp" => write_with(ImageFormat::Bmp),
        "tiff" => write_with(ImageFormat::Tiff),
        other => Err(format!("Unsupported format: {}", other)),
    }
}

/// Converts to a colour type the target encoder will actually accept.
///
/// The crate's encoders each support a different subset and fail at encode time with an opaque
/// message rather than converting — a greyscale PNG converted to GIF died with "the encoder or
/// decoder for Gif does not support the color type `L8`". Converting up front turns that class
/// of failure into a working conversion.
fn normalise_for_format<'a>(
    img: &'a image::DynamicImage,
    format: &str,
) -> std::borrow::Cow<'a, image::DynamicImage> {
    use image::ColorType::{La16, La8, L8, Rgb8, Rgba16, Rgba8};

    let color = img.color();
    let acceptable = match format {
        // Broad support, including 16-bit and greyscale.
        "png" | "tiff" => true,
        // encode_webp picks RGB or RGBA itself.
        "webp" => true,
        "jpg" | "jpeg" => matches!(color, L8 | Rgb8 | Rgba8),
        "bmp" => matches!(color, L8 | Rgb8 | Rgba8),
        "gif" | "avif" => matches!(color, Rgb8 | Rgba8),
        _ => true,
    };

    if acceptable {
        return std::borrow::Cow::Borrowed(img);
    }

    // Keep alpha where the source had it and the format can express it; JPEG cannot.
    let keeps_alpha =
        matches!(color, Rgba8 | Rgba16 | La8 | La16) && !matches!(format, "jpg" | "jpeg");
    std::borrow::Cow::Owned(if keeps_alpha {
        image::DynamicImage::ImageRgba8(img.to_rgba8())
    } else {
        image::DynamicImage::ImageRgb8(img.to_rgb8())
    })
}

/// Temporary path used to stage an encode next to its final destination, so the rename
/// that publishes it stays within one volume and is therefore atomic.
fn temp_path_for(output_path: &Path, input_path: &str) -> PathBuf {
    let dir = output_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = output_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");

    // Inputs are unique within a batch and each maps to one destination, so hashing the
    // input keeps concurrent conversions from colliding on the staging file.
    let mut hasher = DefaultHasher::new();
    input_path.hash(&mut hasher);
    dir.join(format!(".{}.{:016x}.tmp", stem, hasher.finish()))
}

fn convert_single_image(
    input_path: &str,
    output_path: String,
    options: &ConversionOptions,
) -> ConversionResult {
    let failure = |error: String| ConversionResult {
        success: false,
        input_path: input_path.to_string(),
        output_path: None,
        error: Some(error),
    };

    let (img, metadata) = match decode_oriented(input_path) {
        Ok(decoded) => decoded,
        Err(e) => return failure(format!("Failed to open image: {}", e)),
    };

    let img = apply_transforms(img, options.crop.as_ref(), options.resize.as_ref());

    let mut encoded = match encode_to_bytes(&img, options) {
        Ok(bytes) => bytes,
        Err(e) => return failure(format!("Failed to save image: {}", e)),
    };

    if options.preserve_metadata {
        encoded = attach_metadata(encoded, &metadata);
    }

    // Always stage the encode in a temporary file and rename it into place. In replace
    // mode the destination *is* the input, so writing straight into it would truncate
    // the original before the first byte is written — a decode error, a full disk or a
    // crash mid-write would leave no copy of the source anywhere.
    let final_path = Path::new(&output_path);
    let temp = temp_path_for(final_path, input_path);

    if let Err(e) = fs::write(&temp, &encoded) {
        let _ = fs::remove_file(&temp);
        return failure(format!("Failed to save image: {}", e));
    }

    if let Err(e) = fs::rename(&temp, final_path) {
        let _ = fs::remove_file(&temp);
        return failure(format!("Failed to write {}: {}", output_path, e));
    }

    // Replace mode with a changed format leaves the original behind under its old
    // extension; drop it only now that the replacement is safely on disk.
    if matches!(options.output_mode, OutputMode::ReplaceOriginal)
        && Path::new(input_path) != final_path
    {
        let _ = fs::remove_file(input_path);
    }

    ConversionResult {
        success: true,
        input_path: input_path.to_string(),
        output_path: Some(output_path),
        error: None,
    }
}

#[tauri::command]
async fn convert_images(paths: Vec<String>, options: ConversionOptions) -> BatchConversionResult {
    // Destinations are reserved sequentially, then the decode/encode work — which is
    // the expensive part — fans out across rayon's pool inside a blocking task, so the
    // main thread stays free to keep the window responsive.
    let planned = plan_output_paths(&paths, &options);
    let inputs = paths.clone();

    let results: Vec<ConversionResult> = tauri::async_runtime::spawn_blocking(move || {
        paths
            .par_iter()
            .zip(planned)
            .map(|(input_path, planned)| match planned {
                Ok(output_path) => convert_single_image(input_path, output_path, &options),
                Err(error) => ConversionResult {
                    success: false,
                    input_path: input_path.clone(),
                    output_path: None,
                    error: Some(error),
                },
            })
            .collect()
    })
    .await
    .unwrap_or_else(|e| {
        inputs
            .into_iter()
            .map(|input_path| ConversionResult {
                success: false,
                input_path,
                output_path: None,
                error: Some(format!("Conversion task failed: {}", e)),
            })
            .collect()
    });

    let succeeded = results.iter().filter(|r| r.success).count();
    let failed = results.len() - succeeded;

    BatchConversionResult {
        total: results.len(),
        succeeded,
        failed,
        results,
    }
}

/// Longest edge of the preview image handed to the crop UI.
const THUMBNAIL_MAX_EDGE: u32 = 900;

/// Edge length of each native-resolution sample tile used for size estimation, and the grid
/// they are taken on (2x2 = four tiles, one per quadrant).
///
/// Tiles are cut at *native* resolution rather than downscaled from the whole image. That
/// distinction is the difference between a usable estimate and a useless one: downscaling
/// averages away high-frequency detail, so a shrunken copy of a photo compresses far better
/// per pixel than the original and extrapolating from it underestimates badly — measured at
/// -50% to -84% on detailed images. A native tile has the same detail character as the source,
/// so bytes-per-pixel carries across.
const SAMPLE_TILE_EDGE: u32 = 256;
const SAMPLE_GRID: u32 = 2;

/// Longest edge of the whole-image proxy kept alongside the tiles.
///
/// When the requested output fits inside this, the proxy is resized to the exact output size
/// and encoded whole — no extrapolation, so the estimate is near-exact. Extrapolating from
/// tiles is only used for outputs larger than this, where tiles stay big enough to be
/// representative. Heavy downscales were the tile approach's weak spot: a 17x reduction
/// shrinks a 256px tile to 15px, and per-pixel cost at that size stopped tracking reality,
/// overestimating by up to 89%.
const SAMPLE_PROXY_EDGE: u32 = 640;

/// An orientation-corrected preview as a PNG data URI, for the crop overlay to draw on.
///
/// Orientation matters here: the crop rectangle is expressed against what the user sees, so
/// the preview has to be rotated the same way the output will be.
#[tauri::command]
async fn get_thumbnail(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (img, _) = decode_oriented(&path)?;

        // Only shrink. `thumbnail` would otherwise enlarge a small image to fill the box,
        // producing a blurry preview and a needlessly large data URI.
        let thumb = if img.width() > THUMBNAIL_MAX_EDGE || img.height() > THUMBNAIL_MAX_EDGE {
            img.thumbnail(THUMBNAIL_MAX_EDGE, THUMBNAIL_MAX_EDGE)
        } else {
            img
        };

        let mut buf = std::io::Cursor::new(Vec::new());
        thumb
            .write_to(&mut buf, ImageFormat::Png)
            .map_err(|e| format!("Failed to encode thumbnail: {}", e))?;

        Ok(format!(
            "data:image/png;base64,{}",
            BASE64.encode(buf.into_inner())
        ))
    })
    .await
    .map_err(|e| format!("Failed to build thumbnail: {}", e))?
}

/// What is kept in memory per source image to estimate output sizes without re-decoding:
/// native-resolution tiles for large outputs, and a whole-image proxy for small ones.
struct EstimationSample {
    tiles: Vec<image::DynamicImage>,
    proxy: image::DynamicImage,
    source_width: u32,
    source_height: u32,
    /// True when the source was small enough that the proxy *is* the source, which makes any
    /// estimate derived from it exact.
    whole_image: bool,
}

/// Derives both the tiles and the proxy from one decode.
fn build_sample(img: &image::DynamicImage) -> EstimationSample {
    let (w, h) = (img.width(), img.height());

    // `thumbnail` scales to *fit* the box, which means it enlarges anything smaller. An
    // upscaled proxy then gets scaled back down for the estimate, and the double resample
    // softened the image enough to underestimate by 30%.
    let whole_image = w <= SAMPLE_PROXY_EDGE && h <= SAMPLE_PROXY_EDGE;
    let proxy = if whole_image {
        img.clone()
    } else {
        img.thumbnail(SAMPLE_PROXY_EDGE, SAMPLE_PROXY_EDGE)
    };

    // Small enough that sampling the whole thing is cheaper than slicing it up.
    if w <= SAMPLE_TILE_EDGE * SAMPLE_GRID && h <= SAMPLE_TILE_EDGE * SAMPLE_GRID {
        return EstimationSample {
            tiles: vec![img.clone()],
            proxy,
            source_width: w,
            source_height: h,
            whole_image,
        };
    }

    let tw = SAMPLE_TILE_EDGE.min(w / SAMPLE_GRID).max(1);
    let th = SAMPLE_TILE_EDGE.min(h / SAMPLE_GRID).max(1);

    let mut tiles = Vec::with_capacity((SAMPLE_GRID * SAMPLE_GRID) as usize);
    for gy in 0..SAMPLE_GRID {
        for gx in 0..SAMPLE_GRID {
            // Centre of this grid cell, then step back half a tile to centre the tile on it.
            let cx = (2 * gx + 1) * w / (2 * SAMPLE_GRID);
            let cy = (2 * gy + 1) * h / (2 * SAMPLE_GRID);
            let x = cx.saturating_sub(tw / 2).min(w - tw);
            let y = cy.saturating_sub(th / 2).min(h - th);
            tiles.push(img.crop_imm(x, y, tw, th));
        }
    }

    EstimationSample {
        tiles,
        proxy,
        source_width: w,
        source_height: h,
        whole_image,
    }
}

/// Samples are cached because decoding is the expensive part of estimating. Without this,
/// every quality-slider nudge would re-decode every original.
#[derive(Default)]
struct ProxyCache(
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<EstimationSample>>>,
);

#[derive(Debug, Serialize, Deserialize)]
pub struct SizeEstimate {
    /// Sum of estimated output sizes, in bytes.
    estimated_bytes: u64,
    /// Sum of the input file sizes, for comparison.
    source_bytes: u64,
    /// How many inputs contributed. Files that could not be read are excluded.
    counted: usize,
    /// Inputs that could not be estimated at all.
    failed: usize,
    /// False when every sample covered its whole source, i.e. the figure is near-exact.
    approximate: bool,
}

/// Estimates total output size by encoding a small proxy at the chosen settings and scaling
/// the result by the pixel ratio.
///
/// This is an estimate, not a measurement. Compression ratio does not scale perfectly with
/// pixel count — fine detail behaves differently at different scales — so treat it as
/// roughly +/-20% for photographic content, and worse for synthetic images.
#[tauri::command]
async fn estimate_output_size(
    paths: Vec<String>,
    options: ConversionOptions,
    cache: tauri::State<'_, ProxyCache>,
) -> Result<SizeEstimate, String> {
    // Reuse cached proxies, decode whatever is missing.
    let mut proxies: Vec<(String, std::sync::Arc<EstimationSample>)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    {
        let map = cache.0.lock().map_err(|_| "proxy cache poisoned")?;
        for path in &paths {
            match map.get(path) {
                Some(proxy) => proxies.push((path.clone(), proxy.clone())),
                None => missing.push(path.clone()),
            }
        }
    }

    if !missing.is_empty() {
        let decoded = tauri::async_runtime::spawn_blocking(move || {
            missing
                .par_iter()
                .map(|path| {
                    let sample = decode_oriented(path).map(|(img, _)| build_sample(&img));
                    (path.clone(), sample)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| format!("Failed to prepare estimate: {}", e))?;

        let mut map = cache.0.lock().map_err(|_| "proxy cache poisoned")?;
        for (path, sample) in decoded {
            if let Ok(sample) = sample {
                let sample = std::sync::Arc::new(sample);
                map.insert(path.clone(), sample.clone());
                proxies.push((path, sample));
            }
        }
        // Keep the cache bounded to the current selection rather than growing for the life
        // of the process.
        let keep: std::collections::HashSet<&String> = paths.iter().collect();
        map.retain(|k, _| keep.contains(k));
    }

    let source_bytes: u64 = paths
        .iter()
        .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
        .sum();
    let failed = paths.len() - proxies.len();
    let approximate = proxies.iter().any(|(_, s)| !s.whole_image);

    let estimated_bytes = tauri::async_runtime::spawn_blocking(move || {
        proxies
            .par_iter()
            .filter_map(|(_, sample)| estimate_one(sample, &options))
            .sum::<u64>()
    })
    .await
    .map_err(|e| format!("Failed to estimate size: {}", e))?;

    Ok(SizeEstimate {
        estimated_bytes,
        source_bytes,
        counted: paths.len() - failed,
        failed,
        approximate,
    })
}

/// Measures bytes-per-pixel on the sample tiles and multiplies by the real output's pixel count.
///
/// The tiles are scaled by the *same factor* the full image will undergo, so they carry the
/// detail loss that resizing causes. Encoding tiles at native scale and extrapolating from
/// there is what makes the estimate track reality; see [`SAMPLE_TILE_EDGE`].
fn estimate_one(sample: &EstimationSample, options: &ConversionOptions) -> Option<u64> {
    let (out_w, out_h) = planned_dimensions(
        sample.source_width,
        sample.source_height,
        options.crop.as_ref(),
        options.resize.as_ref(),
    );
    let out_pixels = u64::from(out_w) * u64::from(out_h);
    if out_pixels == 0 {
        return None;
    }

    // The cropped region is what gets resized, so the output's scale factor is measured
    // against the crop, not the original frame.
    let (crop_w, crop_h) = options
        .crop
        .as_ref()
        .filter(|c| !c.is_full_frame())
        .and_then(|c| c.to_pixels(sample.source_width, sample.source_height))
        .map_or(
            (sample.source_width, sample.source_height),
            |(_, _, cw, ch)| (cw, ch),
        );

    // When the output fits inside the proxy, encode the whole proxy at the exact output size.
    // No extrapolation is involved, so this is as close to measuring as estimating gets — and
    // it is exactly the heavy-downscale case that tile extrapolation handled worst.
    if out_w <= sample.proxy.width() && out_h <= sample.proxy.height() {
        let cropped = apply_transforms(sample.proxy.clone(), options.crop.as_ref(), None);
        // Resampling at identical dimensions is not a no-op — Lanczos3 would soften the image
        // and change its encoded size — so only resize when the size actually differs.
        let scaled = if (cropped.width(), cropped.height()) == (out_w, out_h) {
            cropped
        } else {
            cropped.resize_exact(out_w, out_h, image::imageops::FilterType::Lanczos3)
        };
        return encode_to_bytes(&scaled, options).ok().map(|b| b.len() as u64);
    }

    let sx = f64::from(out_w) / f64::from(crop_w.max(1));
    let sy = f64::from(out_h) / f64::from(crop_h.max(1));

    // Scale every tile by the factor the real image gets, then stitch them into one sample.
    // Stitching matters at heavy downscales: four separate 32px encodes pay four lots of
    // container overhead on almost no data, which is where the estimate went most wrong.
    let scaled: Vec<image::DynamicImage> = sample
        .tiles
        .iter()
        .map(|tile| {
            let tw = ((f64::from(tile.width()) * sx).round() as u32).max(1);
            let th = ((f64::from(tile.height()) * sy).round() as u32).max(1);
            if (tw, th) == (tile.width(), tile.height()) {
                tile.clone()
            } else {
                tile.resize_exact(tw, th, image::imageops::FilterType::Lanczos3)
            }
        })
        .collect();

    let stitched = stitch_tiles(&scaled)?;
    let pixels = u64::from(stitched.width()) * u64::from(stitched.height());
    if pixels == 0 {
        return None;
    }

    let bytes = encode_to_bytes(&stitched, options).ok()?.len() as u64;

    // Split the measurement into fixed container cost and per-pixel data cost. Without this
    // the estimate inflates on downscales, where headers and quantisation tables dominate the
    // sample's bytes and multiplying that per-pixel figure up overestimates badly.
    let overhead = container_overhead(options);
    let data_per_pixel = bytes.saturating_sub(overhead) as f64 / pixels as f64;

    Some(overhead + (data_per_pixel * out_pixels as f64) as u64)
}

/// Lays tiles out in a `SAMPLE_GRID`-wide mosaic so they can be encoded as one image.
///
/// The canvas keeps the tiles' own colour type. Normalising to RGB would wreck the estimate:
/// a greyscale source encoded as RGB costs roughly three times the bytes per pixel, which
/// showed up as a +204% overestimate on a greyscale PNG before this was fixed.
fn stitch_tiles(tiles: &[image::DynamicImage]) -> Option<image::DynamicImage> {
    let first = tiles.first()?;
    if tiles.len() == 1 {
        return Some(first.clone());
    }

    let (tw, th) = (first.width(), first.height());
    let cols = SAMPLE_GRID;
    let rows = (tiles.len() as u32).div_ceil(cols);

    let mut canvas = image::DynamicImage::new(tw * cols, th * rows, first.color());
    for (i, tile) in tiles.iter().enumerate() {
        let x = (i as u32 % cols) * tw;
        let y = (i as u32 / cols) * th;
        image::imageops::replace(&mut canvas, tile, i64::from(x), i64::from(y));
    }

    Some(canvas)
}

/// Bytes a container costs before any image data: signatures, headers, quantisation tables,
/// palettes. Measured rather than assumed, since it varies by format and quality.
fn container_overhead(options: &ConversionOptions) -> u64 {
    let flat = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        8,
        8,
        image::Rgb([128, 128, 128]),
    ));
    encode_to_bytes(&flat, options).map_or(0, |b| b.len() as u64)
}

/// Source dimensions paired with what they will become after crop and resize.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlannedSize {
    width: u32,
    height: u32,
}

/// Pure maths — no image is opened — so the UI can call this freely while someone types in
/// the resize fields or drags the crop box.
#[tauri::command]
fn plan_output_dimensions(
    sources: Vec<PlannedSize>,
    crop: Option<CropRect>,
    resize: Option<ResizeOptions>,
) -> Vec<PlannedSize> {
    sources
        .into_iter()
        .map(|source| {
            let (width, height) =
                planned_dimensions(source.width, source.height, crop.as_ref(), resize.as_ref());
            PlannedSize { width, height }
        })
        .collect()
}

#[tauri::command]
fn get_supported_input_formats() -> Vec<String> {
    vec![
        "PNG".to_string(),
        "JPEG".to_string(),
        "GIF".to_string(),
        "WebP".to_string(),
        "BMP".to_string(),
        "TIFF".to_string(),
        "ICO".to_string(),
        // AVIF is deliberately absent: the `image` crate only decodes it with the
        // non-default `avif-native` feature, which links dav1d. AVIF remains available as
        // an *output* format, which does work with default features.
    ]
}

#[tauri::command]
fn get_supported_output_formats() -> Vec<OutputFormatInfo> {
    let info = |name: &str, extension: &str, supports_quality: bool| OutputFormatInfo {
        name: name.to_string(),
        extension: extension.to_string(),
        supports_quality,
        supports_metadata: format_carries_metadata(extension),
    };

    vec![
        info("PNG", "png", false),
        info("JPEG", "jpg", true),
        info("WebP", "webp", true),
        info("AVIF", "avif", false),
        info("GIF", "gif", false),
        info("BMP", "bmp", false),
        info("TIFF", "tiff", false),
    ]
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OutputFormatInfo {
    name: String,
    extension: String,
    supports_quality: bool,
    /// Whether Exif/ICC survive conversion to this format, so the UI can say so instead of
    /// leaving the user to discover it.
    supports_metadata: bool,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(ProxyCache::default())
        .invoke_handler(tauri::generate_handler![
            get_image_info,
            get_images_info,
            convert_images,
            get_thumbnail,
            plan_output_dimensions,
            estimate_output_size,
            get_supported_input_formats,
            get_supported_output_formats
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Unique scratch directory per test. Avoids a `tempfile` dev-dependency, and being
    /// under the system temp dir keeps it on one volume so renames stay atomic.
    fn scratch_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pasture-panda-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn options(format: &str, output_mode: OutputMode) -> ConversionOptions {
        ConversionOptions {
            format: format.to_string(),
            quality: 85,
            output_mode,
            output_folder: None,
            crop: None,
            resize: None,
            preserve_metadata: true,
        }
    }

    fn fit(width: Option<u32>, height: Option<u32>) -> ResizeOptions {
        ResizeOptions {
            width,
            height,
            preserve_aspect: true,
            no_upscale: false,
        }
    }

    fn stretch(width: Option<u32>, height: Option<u32>) -> ResizeOptions {
        ResizeOptions {
            width,
            height,
            preserve_aspect: false,
            no_upscale: false,
        }
    }

    fn crop(x: f32, y: f32, width: f32, height: f32) -> CropRect {
        CropRect {
            x,
            y,
            width,
            height,
        }
    }

    /// Photographic-ish content: a smooth gradient plus pseudo-random grain from a small
    /// LCG. A regular pattern would compress losslessly to almost nothing and make any
    /// lossy-vs-lossless size comparison meaningless.
    fn noisy_image(width: u32, height: u32) -> image::RgbImage {
        let mut state: u32 = 0x1234_5678;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };

        let mut img = image::RgbImage::new(width, height);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let base_r = (x * 255 / width.max(1)) as u8;
            let base_g = (y * 255 / height.max(1)) as u8;
            *px = image::Rgb([
                base_r.wrapping_add(next() / 2),
                base_g.wrapping_add(next() / 2),
                next(),
            ]);
        }
        img
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        let img = image::DynamicImage::new_rgb8(width, height);
        img.save_with_format(path, ImageFormat::Png).expect("write png");
    }

    #[test]
    fn same_folder_keeps_stem_and_swaps_extension() {
        let mut reserved = HashSet::new();
        let out = get_output_path(
            "/photos/holiday.png",
            &options("webp", OutputMode::SameFolder),
            &mut reserved,
        )
        .unwrap();
        assert_eq!(out, "/photos/holiday.webp");
    }

    #[test]
    fn jpeg_and_jpg_both_normalise_to_jpg() {
        let mut reserved = HashSet::new();
        for format in ["jpeg", "JPEG", "jpg"] {
            reserved.clear();
            let out = get_output_path(
                "/photos/a.png",
                &options(format, OutputMode::SameFolder),
                &mut reserved,
            )
            .unwrap();
            assert_eq!(out, "/photos/a.jpg", "format {}", format);
        }
    }

    #[test]
    fn unsupported_output_format_is_rejected() {
        let mut reserved = HashSet::new();
        let err = get_output_path(
            "/photos/a.png",
            &options("heic", OutputMode::SameFolder),
            &mut reserved,
        )
        .unwrap_err();
        assert!(err.contains("Unsupported output format"), "got: {}", err);
    }

    #[test]
    fn custom_folder_redirects_output() {
        let mut reserved = HashSet::new();
        let mut opts = options("png", OutputMode::CustomFolder);
        opts.output_folder = Some("/exports".to_string());
        let out = get_output_path("/photos/deep/a.tiff", &opts, &mut reserved).unwrap();
        assert_eq!(out, "/exports/a.png");
    }

    #[test]
    fn custom_folder_without_folder_is_an_error() {
        let mut reserved = HashSet::new();
        let err = get_output_path(
            "/photos/a.png",
            &options("png", OutputMode::CustomFolder),
            &mut reserved,
        )
        .unwrap_err();
        assert!(err.contains("Custom folder not specified"), "got: {}", err);
    }

    #[test]
    fn batch_collisions_get_distinct_names() {
        let mut reserved = HashSet::new();
        let opts = options("png", OutputMode::CustomFolder);
        let mut opts = opts;
        opts.output_folder = Some("/exports".to_string());

        // Two different inputs whose stems collide in one destination folder.
        let first = get_output_path("/a/shot.jpg", &opts, &mut reserved).unwrap();
        let second = get_output_path("/b/shot.tiff", &opts, &mut reserved).unwrap();
        assert_eq!(first, "/exports/shot.png");
        assert_eq!(second, "/exports/shot_1.png");
        assert_ne!(first, second);
    }

    #[test]
    fn existing_file_on_disk_is_never_overwritten() {
        let dir = scratch_dir();
        let existing = dir.join("a.png");
        write_png(&existing, 2, 2);

        let mut reserved = HashSet::new();
        let out = get_output_path(
            dir.join("a.tiff").to_str().unwrap(),
            &options("png", OutputMode::SameFolder),
            &mut reserved,
        )
        .unwrap();

        assert_eq!(out, dir.join("a_1.png").to_str().unwrap());
        assert!(existing.exists(), "pre-existing file must survive planning");
    }

    #[test]
    fn replace_mode_same_format_targets_the_input_itself() {
        let mut reserved = HashSet::new();
        let out = get_output_path(
            "/photos/a.jpeg",
            &options("jpg", OutputMode::ReplaceOriginal),
            &mut reserved,
        )
        .unwrap();
        // .jpeg and .jpg are the same format, so this is an in-place replacement.
        assert_eq!(out, "/photos/a.jpeg");
    }

    #[test]
    fn replace_mode_rejects_two_inputs_claiming_one_destination() {
        let mut reserved = HashSet::new();
        let opts = options("jpg", OutputMode::ReplaceOriginal);

        // a.jpg replaces itself; a.png would then also want to become a.jpg.
        let first = get_output_path("/photos/a.jpg", &opts, &mut reserved).unwrap();
        assert_eq!(first, "/photos/a.jpg");

        let err = get_output_path("/photos/a.png", &opts, &mut reserved).unwrap_err();
        assert!(
            err.contains("already writes to"),
            "second claim must fail rather than race: {}",
            err
        );
    }

    #[test]
    fn plan_output_paths_reserves_across_the_whole_batch() {
        let opts = {
            let mut o = options("png", OutputMode::CustomFolder);
            o.output_folder = Some("/exports".to_string());
            o
        };
        let planned = plan_output_paths(
            &[
                "/a/x.jpg".to_string(),
                "/b/x.jpg".to_string(),
                "/c/x.jpg".to_string(),
            ],
            &opts,
        );
        let paths: Vec<String> = planned.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(
            paths,
            vec!["/exports/x.png", "/exports/x_1.png", "/exports/x_2.png"]
        );
    }

    #[test]
    fn replace_in_place_leaves_a_valid_image_and_no_temp_files() {
        let dir = scratch_dir();
        let target = dir.join("photo.png");
        write_png(&target, 4, 6);

        let opts = options("png", OutputMode::ReplaceOriginal);
        let input = target.to_str().unwrap().to_string();
        let mut reserved = HashSet::new();
        let out = get_output_path(&input, &opts, &mut reserved).unwrap();
        assert_eq!(out, input, "same-format replace should target the input");

        let result = convert_single_image(&input, out, &opts);
        assert!(result.success, "conversion failed: {:?}", result.error);

        // The replacement must be a readable image, not a truncated file.
        let (w, h) = ImageReader::open(&target)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!((w, h), (4, 6));

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left behind: {:?}", leftovers);
    }

    /// Regression test for the replace-mode data loss bug.
    ///
    /// JPEG cannot encode a dimension above 65535, so a very wide source decodes fine and
    /// then fails *during encoding* — the exact window in which the old code had already
    /// truncated the destination with `File::create`. Replace mode points the destination
    /// at the input, so that truncation destroyed the only copy of the source.
    #[test]
    fn encode_failure_after_decode_leaves_the_original_intact() {
        let dir = scratch_dir();
        let target = dir.join("wide.png");
        write_png(&target, 70_000, 1);
        let before = fs::read(&target).unwrap();

        let opts = options("jpg", OutputMode::ReplaceOriginal);
        let input = target.to_str().unwrap().to_string();

        // Replace mode changing png -> jpg writes alongside, so aim the destination at the
        // input directly to reproduce the in-place case.
        let result = convert_single_image(&input, input.clone(), &opts);

        assert!(!result.success, "oversized JPEG encode should fail");
        assert!(target.exists(), "original must still exist");
        assert_eq!(
            fs::read(&target).unwrap(),
            before,
            "original must be byte-identical after a failed conversion"
        );

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "staging files left behind: {:?}", leftovers);
    }

    #[test]
    fn unsupported_format_fails_without_touching_the_destination() {
        let dir = scratch_dir();
        let target = dir.join("photo.png");
        write_png(&target, 3, 3);
        let before = fs::read(&target).unwrap();

        let opts = options("heic", OutputMode::ReplaceOriginal);
        let input = target.to_str().unwrap().to_string();
        let result = convert_single_image(&input, input.clone(), &opts);

        assert!(!result.success, "unsupported format should fail");
        assert_eq!(fs::read(&target).unwrap(), before);
    }

    /// The point of switching to libwebp: a photographic image at slider quality should
    /// encode smaller than the same image losslessly, which the old encoder could not do.
    #[test]
    fn lossy_webp_beats_lossless_on_photographic_content() {
        let dir = scratch_dir();
        let img = image::DynamicImage::ImageRgb8(noisy_image(256, 256));

        let lossy = encode_webp(&img, 60).expect("lossy encode");
        let lossless = encode_webp(&img, 100).expect("lossless encode");

        assert!(
            lossy.len() < lossless.len(),
            "expected lossy ({} bytes) to beat lossless ({} bytes)",
            lossy.len(),
            lossless.len()
        );

        // And the result must still be a readable WebP of the right size.
        let path = dir.join("q60.webp");
        fs::write(&path, &lossy).unwrap();
        let (w, h) = ImageReader::open(&path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!((w, h), (256, 256));
    }

    #[test]
    fn webp_quality_slider_changes_output_size() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(128, 128));
        let low = encode_webp(&img, 20).unwrap();
        let high = encode_webp(&img, 95).unwrap();

        assert!(
            low.len() < high.len(),
            "quality 20 ({}) should be smaller than quality 95 ({})",
            low.len(),
            high.len()
        );
    }

    #[test]
    fn webp_preserves_alpha() {
        let dir = scratch_dir();
        let mut img = image::RgbaImage::new(32, 32);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([255, 0, 0, if x < 16 { 0 } else { 255 }]);
        }
        let encoded = encode_webp(&image::DynamicImage::ImageRgba8(img), 100).unwrap();

        let dest = dir.join("alpha.webp");
        fs::write(&dest, &encoded).unwrap();
        let decoded = image::open(&dest).expect("decode webp");
        assert!(decoded.color().has_alpha(), "alpha channel must survive");
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0)[3], 0, "left half transparent");
        assert_eq!(decoded.to_rgba8().get_pixel(31, 0)[3], 255, "right half opaque");
    }

    #[test]
    fn webp_rejects_oversized_images_with_a_clear_message() {
        let img = image::DynamicImage::new_rgb8(WEBP_MAX_DIMENSION + 1, 1);
        let err = encode_webp(&img, 80).unwrap_err();
        assert!(err.contains("maximum dimension"), "got: {}", err);
    }

    #[test]
    fn avif_is_not_advertised_as_an_input_format() {
        // Regression guard for issue 4: decoding AVIF needs the avif-native feature.
        assert!(!get_supported_input_formats().iter().any(|f| f == "AVIF"));
    }

    #[test]
    fn webp_output_reports_quality_support() {
        let webp = get_supported_output_formats()
            .into_iter()
            .find(|f| f.extension == "webp")
            .expect("webp output format");
        assert!(webp.supports_quality, "lossy WebP means the slider applies");
    }

    /// Minimal but valid Exif block: little-endian TIFF header, one IFD entry holding the
    /// orientation tag (0x0112), plus an ImageDescription (0x010E) so there is something
    /// other than orientation to preserve.
    fn exif_chunk_with_orientation(exif_orientation: u16) -> Vec<u8> {
        let mut chunk: Vec<u8> = Vec::new();
        chunk.extend_from_slice(b"II*\0"); // little-endian marker
        chunk.extend_from_slice(&8u32.to_le_bytes()); // offset of first IFD
        chunk.extend_from_slice(&2u16.to_le_bytes()); // entry count

        // Orientation: SHORT, count 1.
        chunk.extend_from_slice(&0x0112u16.to_le_bytes());
        chunk.extend_from_slice(&3u16.to_le_bytes());
        chunk.extend_from_slice(&1u32.to_le_bytes());
        chunk.extend_from_slice(&exif_orientation.to_le_bytes());
        chunk.extend_from_slice(&0u16.to_le_bytes()); // value padding

        // ImageDescription: ASCII, 4 bytes, fits inline.
        chunk.extend_from_slice(&0x010Eu16.to_le_bytes());
        chunk.extend_from_slice(&2u16.to_le_bytes());
        chunk.extend_from_slice(&4u32.to_le_bytes());
        chunk.extend_from_slice(b"ab\0\0");

        chunk.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        chunk
    }

    fn read_back_exif(path: &Path) -> Option<Vec<u8>> {
        let mut decoder = ImageReader::open(path)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        decoder.exif_metadata().ok().flatten()
    }

    #[test]
    fn exif_and_icc_are_attached_to_jpeg_output() {
        let dir = scratch_dir();
        let dest = dir.join("out.jpg");

        let img = image::DynamicImage::ImageRgb8(noisy_image(32, 32));
        let opts = options("jpg", OutputMode::SameFolder);
        let meta = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: Some(b"fake-icc-profile".to_vec()),
        };

        let encoded = attach_metadata(encode_to_bytes(&img, &opts).unwrap(), &meta);
        fs::write(&dest, &encoded).unwrap();

        let exif = read_back_exif(&dest).expect("exif should survive");
        assert!(
            exif.windows(2).any(|w| w == [0x0E, 0x01]),
            "ImageDescription tag should still be present"
        );

        let mut decoder = ImageReader::open(&dest)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        let icc = decoder.icc_profile().ok().flatten().expect("icc should survive");
        assert_eq!(icc, b"fake-icc-profile");
    }

    /// Walks the PNG chunk sequence, returning the four-byte type codes in order.
    fn png_chunk_kinds(bytes: &[u8]) -> Vec<String> {
        let mut kinds = Vec::new();
        let mut i = 8; // skip the signature
        while i + 8 <= bytes.len() {
            let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
            let kind = String::from_utf8_lossy(&bytes[i + 4..i + 8]).to_string();
            let is_end = kind == "IEND";
            kinds.push(kind);
            if is_end {
                break;
            }
            i += 12 + len as usize;
        }
        kinds
    }

    #[test]
    fn exif_survives_a_png_round_trip() {
        let dir = scratch_dir();
        let dest = dir.join("out.png");
        let img = image::DynamicImage::ImageRgb8(noisy_image(16, 16));
        let opts = options("png", OutputMode::SameFolder);
        let meta = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: None,
        };

        let encoded = attach_metadata(encode_to_bytes(&img, &opts).unwrap(), &meta);
        fs::write(&dest, &encoded).unwrap();

        assert!(read_back_exif(&dest).is_some(), "png eXIf chunk should be readable");
        assert!(image::open(&dest).is_ok(), "png should still decode");
    }

    /// `img-parts` puts `eXIf` after `IDAT`, where the `image` crate (and plenty of other
    /// decoders) never look. Ordering is the whole reason PNG metadata reads back at all.
    #[test]
    fn png_exif_chunk_precedes_the_image_data() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(16, 16));
        let opts = options("png", OutputMode::SameFolder);
        let meta = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: None,
        };

        let encoded = attach_metadata(encode_to_bytes(&img, &opts).unwrap(), &meta);
        let kinds = png_chunk_kinds(&encoded);

        let exif_at = kinds.iter().position(|k| k == "eXIf").expect("eXIf chunk");
        let idat_at = kinds.iter().position(|k| k == "IDAT").expect("IDAT chunk");
        assert!(
            exif_at < idat_at,
            "eXIf must precede IDAT, got order: {:?}",
            kinds
        );
    }

    /// The trap this whole feature could easily walk into: we rotate the pixels ourselves,
    /// so if the original orientation tag were carried through, every Exif-aware viewer
    /// would rotate the output a second time.
    #[test]
    fn preserved_exif_has_its_orientation_tag_neutralised() {
        let mut chunk = exif_chunk_with_orientation(6); // 6 = rotate 90 clockwise
        assert_eq!(
            Orientation::from_exif_chunk(&chunk),
            Some(Orientation::Rotate90),
            "fixture should start out rotated"
        );

        // Same call decode_oriented makes after reading the chunk.
        let removed = Orientation::remove_from_exif_chunk(&mut chunk);
        assert_eq!(removed, Some(Orientation::Rotate90));
        assert_eq!(
            Orientation::from_exif_chunk(&chunk),
            Some(Orientation::NoTransforms),
            "tag must be reset or viewers will double-rotate"
        );

        // The rest of the block must be untouched.
        assert!(
            chunk.windows(2).any(|w| w == [0x0E, 0x01]),
            "clearing orientation must not drop other tags"
        );
    }

    #[test]
    fn exif_survives_a_webp_round_trip() {
        let dir = scratch_dir();
        let dest = dir.join("out.webp");
        let img = image::DynamicImage::ImageRgb8(noisy_image(32, 32));
        let opts = options("webp", OutputMode::SameFolder);
        let meta = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: None,
        };

        // A plain libwebp file is a simple VP8/VP8L container; img-parts has to promote it
        // to the extended VP8X form before it can hold an EXIF chunk.
        let encoded = attach_metadata(encode_to_bytes(&img, &opts).unwrap(), &meta);
        fs::write(&dest, &encoded).unwrap();

        assert!(image::open(&dest).is_ok(), "webp should still decode");
        assert!(read_back_exif(&dest).is_some(), "webp EXIF chunk should be readable");
    }

    /// End-to-end through the real conversion entry point, which is the only test that
    /// exercises the metadata *read* side in `decode_oriented`.
    #[test]
    fn metadata_travels_from_source_file_to_converted_output() {
        let dir = scratch_dir();
        let source = dir.join("source.jpg");

        // Build a source JPEG that genuinely carries EXIF and an ICC profile.
        let img = image::DynamicImage::ImageRgb8(noisy_image(48, 48));
        let seed = options("jpg", OutputMode::SameFolder);
        let planted = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: Some(b"planted-icc-profile".to_vec()),
        };
        fs::write(
            &source,
            attach_metadata(encode_to_bytes(&img, &seed).unwrap(), &planted),
        )
        .unwrap();

        // Convert it to PNG, a different container entirely.
        let opts = options("png", OutputMode::SameFolder);
        let dest = dir.join("source.png");
        let result = convert_single_image(
            source.to_str().unwrap(),
            dest.to_str().unwrap().to_string(),
            &opts,
        );
        assert!(result.success, "conversion failed: {:?}", result.error);

        let exif = read_back_exif(&dest).expect("exif should reach the output");
        assert!(
            exif.windows(2).any(|w| w == [0x0E, 0x01]),
            "ImageDescription should have travelled across formats"
        );

        let mut decoder = ImageReader::open(&dest)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(
            decoder.icc_profile().ok().flatten().as_deref(),
            Some(b"planted-icc-profile".as_slice()),
            "icc profile should have travelled too"
        );
    }

    #[test]
    fn conversion_with_preserving_off_strips_source_metadata() {
        let dir = scratch_dir();
        let source = dir.join("source.jpg");

        let img = image::DynamicImage::ImageRgb8(noisy_image(32, 32));
        let seed = options("jpg", OutputMode::SameFolder);
        let planted = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: None,
        };
        fs::write(
            &source,
            attach_metadata(encode_to_bytes(&img, &seed).unwrap(), &planted),
        )
        .unwrap();
        assert!(read_back_exif(&source).is_some(), "source should have exif");

        let mut opts = options("jpg", OutputMode::SameFolder);
        opts.preserve_metadata = false;
        let dest = dir.join("stripped-out.jpg");
        let result = convert_single_image(
            source.to_str().unwrap(),
            dest.to_str().unwrap().to_string(),
            &opts,
        );
        assert!(result.success, "conversion failed: {:?}", result.error);
        assert!(
            read_back_exif(&dest).is_none(),
            "metadata must not appear when preserving is off"
        );
    }

    #[test]
    fn stripping_metadata_produces_a_clean_file() {
        let dir = scratch_dir();
        let dest = dir.join("stripped.jpg");
        let img = image::DynamicImage::ImageRgb8(noisy_image(16, 16));
        let opts = options("jpg", OutputMode::SameFolder);

        // preserve_metadata=false means attach_metadata is never called.
        let encoded = encode_to_bytes(&img, &opts).unwrap();
        fs::write(&dest, &encoded).unwrap();

        assert!(
            read_back_exif(&dest).is_none(),
            "no metadata should be written when preserving is off"
        );
    }

    #[test]
    fn formats_that_cannot_carry_metadata_still_encode() {
        let dir = scratch_dir();
        let img = image::DynamicImage::ImageRgb8(noisy_image(16, 16));
        let meta = SourceMetadata {
            exif: Some(exif_chunk_with_orientation(1)),
            icc: Some(b"fake-icc".to_vec()),
        };

        // BMP and GIF are not img-parts containers; attach_metadata must pass the bytes
        // through untouched rather than corrupting or dropping them.
        for format in ["bmp", "gif", "tiff"] {
            let opts = options(format, OutputMode::SameFolder);
            let plain = encode_to_bytes(&img, &opts).unwrap();
            let attached = attach_metadata(plain.clone(), &meta);
            assert_eq!(attached, plain, "{} bytes should be unchanged", format);

            let dest = dir.join(format!("out.{}", format));
            fs::write(&dest, &attached).unwrap();
            assert!(image::open(&dest).is_ok(), "{} should still be readable", format);
        }
    }

    #[test]
    fn metadata_support_is_reported_per_format() {
        let formats = get_supported_output_formats();
        let supports = |ext: &str| {
            formats
                .iter()
                .find(|f| f.extension == ext)
                .unwrap()
                .supports_metadata
        };

        for ext in ["jpg", "png", "webp"] {
            assert!(supports(ext), "{} should carry metadata", ext);
        }
        for ext in ["avif", "gif", "bmp", "tiff"] {
            assert!(!supports(ext), "{} does not carry metadata here", ext);
        }
    }

    // --- resize maths ---

    #[test]
    fn aspect_preserving_resize_fits_inside_the_given_box() {
        // 2000x1000 into a 1000x1000 box: width binds, height follows the ratio.
        assert_eq!(fit(Some(1000), Some(1000)).target_for(2000, 1000), Some((1000, 500)));
        // Tall source into the same box: now height binds.
        assert_eq!(fit(Some(1000), Some(1000)).target_for(1000, 2000), Some((500, 1000)));
    }

    #[test]
    fn a_single_bound_scales_the_other_axis() {
        assert_eq!(fit(Some(800), None).target_for(1600, 1200), Some((800, 600)));
        assert_eq!(fit(None, Some(600)).target_for(1600, 1200), Some((800, 600)));
    }

    #[test]
    fn free_resize_stretches_to_exactly_what_was_asked() {
        assert_eq!(stretch(Some(300), Some(300)).target_for(1600, 1200), Some((300, 300)));
    }

    #[test]
    fn free_resize_leaves_a_blank_axis_alone() {
        assert_eq!(stretch(Some(300), None).target_for(1600, 1200), Some((300, 1200)));
        assert_eq!(stretch(None, Some(300)).target_for(1600, 1200), Some((1600, 300)));
    }

    #[test]
    fn no_upscale_never_enlarges() {
        let mut opts = fit(Some(4000), Some(4000));
        opts.no_upscale = true;
        assert_eq!(opts.target_for(1000, 500), None, "should stay at source size");

        let mut free = stretch(Some(4000), Some(4000));
        free.no_upscale = true;
        assert_eq!(free.target_for(1000, 500), None);

        // Without the flag, enlarging is allowed.
        assert_eq!(fit(Some(4000), Some(4000)).target_for(1000, 500), Some((4000, 2000)));
    }

    #[test]
    fn a_resize_to_the_current_size_is_a_no_op() {
        assert_eq!(fit(Some(1600), Some(1200)).target_for(1600, 1200), None);
        assert_eq!(stretch(Some(1600), Some(1200)).target_for(1600, 1200), None);
    }

    #[test]
    fn resize_never_produces_a_zero_dimension() {
        // An extreme downscale of a very wide image would round the short axis to zero.
        let (w, h) = fit(Some(1), None).target_for(10_000, 3).unwrap();
        assert!(w >= 1 && h >= 1, "got {}x{}", w, h);
    }

    // --- crop maths ---

    #[test]
    fn percentage_crop_resolves_against_each_image_size() {
        // The same rectangle on two different sources — the point of storing percentages.
        assert_eq!(crop(10.0, 10.0, 80.0, 80.0).to_pixels(1000, 1000), Some((100, 100, 800, 800)));
        assert_eq!(crop(10.0, 10.0, 80.0, 80.0).to_pixels(400, 200), Some((40, 20, 320, 160)));
    }

    #[test]
    fn crop_is_clamped_into_the_image() {
        // A rectangle running off the right edge must be trimmed, not allowed to panic
        // crop_imm with out-of-range bounds.
        let (x, y, w, h) = crop(80.0, 80.0, 50.0, 50.0).to_pixels(100, 100).unwrap();
        assert!(x + w <= 100 && y + h <= 100, "got {},{} {}x{}", x, y, w, h);
    }

    #[test]
    fn degenerate_crops_still_yield_a_pixel() {
        for rect in [
            crop(0.0, 0.0, 0.0, 0.0),
            crop(100.0, 100.0, 10.0, 10.0),
            crop(-50.0, -50.0, 5.0, 5.0),
            crop(f32::NAN, 0.0, f32::NAN, 100.0),
        ] {
            let (x, y, w, h) = rect.to_pixels(50, 50).expect("should resolve");
            assert!(w >= 1 && h >= 1, "{:?} gave {}x{}", rect, w, h);
            assert!(x + w <= 50 && y + h <= 50, "{:?} escaped bounds", rect);
        }
    }

    #[test]
    fn full_frame_crop_is_recognised_as_a_no_op() {
        assert!(crop(0.0, 0.0, 100.0, 100.0).is_full_frame());
        assert!(!crop(0.0, 0.0, 99.0, 100.0).is_full_frame());
        assert!(!crop(1.0, 0.0, 100.0, 100.0).is_full_frame());
    }

    // --- the two combined ---

    #[test]
    fn crop_is_applied_before_resize() {
        // Crop 1000x1000 down to the middle 500x500, then fit into 250x250.
        let rect = crop(25.0, 25.0, 50.0, 50.0);
        let resize = fit(Some(250), Some(250));
        assert_eq!(planned_dimensions(1000, 1000, Some(&rect), Some(&resize)), (250, 250));

        // If resize ran first, a 2:1 source cropped to a square would come out non-square.
        let rect = crop(0.0, 0.0, 50.0, 100.0); // square region of a 2:1 image
        let resize = fit(Some(400), Some(400));
        assert_eq!(
            planned_dimensions(1000, 500, Some(&rect), Some(&resize)),
            (400, 400),
            "cropping first should give a square"
        );
    }

    #[test]
    fn planned_dimensions_matches_what_conversion_produces() {
        let dir = scratch_dir();
        let source = dir.join("source.png");
        write_png(&source, 800, 400);

        let rect = crop(10.0, 20.0, 60.0, 50.0);
        let resize = fit(Some(200), None);
        let expected = planned_dimensions(800, 400, Some(&rect), Some(&resize));

        let mut opts = options("png", OutputMode::SameFolder);
        opts.crop = Some(rect);
        opts.resize = Some(resize);

        let dest = dir.join("out.png");
        let result = convert_single_image(
            source.to_str().unwrap(),
            dest.to_str().unwrap().to_string(),
            &opts,
        );
        assert!(result.success, "conversion failed: {:?}", result.error);

        let actual = image::open(&dest).unwrap();
        assert_eq!(
            (actual.width(), actual.height()),
            expected,
            "planned dimensions must match the real output"
        );
    }

    #[test]
    fn cropping_selects_the_right_region() {
        // Left half red, right half blue; crop the right half and check what survives.
        let mut img = image::RgbImage::new(100, 10);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = if x < 50 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            };
        }
        let img = image::DynamicImage::ImageRgb8(img);

        let rect = crop(50.0, 0.0, 50.0, 100.0);
        let out = apply_transforms(img, Some(&rect), None);

        assert_eq!((out.width(), out.height()), (50, 10));
        assert_eq!(out.to_rgb8().get_pixel(0, 0), &image::Rgb([0, 0, 255]), "should be the blue half");
        assert_eq!(out.to_rgb8().get_pixel(49, 9), &image::Rgb([0, 0, 255]));
    }

    #[test]
    fn transforms_are_skipped_when_they_would_change_nothing() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(64, 32));
        let full = crop(0.0, 0.0, 100.0, 100.0);
        let same = fit(Some(64), Some(32));

        let out = apply_transforms(img.clone(), Some(&full), Some(&same));
        assert_eq!((out.width(), out.height()), (64, 32));
        assert_eq!(out.to_rgb8(), img.to_rgb8(), "pixels should be untouched");
    }

    #[test]
    fn plan_output_dimensions_maps_every_source() {
        let planned = plan_output_dimensions(
            vec![
                PlannedSize { width: 1000, height: 500 },
                PlannedSize { width: 400, height: 400 },
            ],
            Some(crop(0.0, 0.0, 50.0, 100.0)),
            Some(fit(Some(100), None)),
        );

        let got: Vec<(u32, u32)> = planned.iter().map(|p| (p.width, p.height)).collect();
        // 1000x500 -> crop 500x500 -> fit width 100 -> 100x100
        // 400x400  -> crop 200x400 -> fit width 100 -> 100x200
        assert_eq!(got, vec![(100, 100), (100, 200)]);
    }

    // --- the test-images corpus ---
    //
    // A mix of real photographs (one with EXIF orientation Rotate270, two with ICC profiles,
    // one PNG with alpha) and generated images covering characteristics the photos don't:
    // flat colour, hard edges, pure noise, smooth gradient, a 25:1 aspect ratio, greyscale,
    // and an image smaller than a single estimation sample tile.

    fn corpus_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test-images")
    }

    /// Every readable image in `test-images/`, sorted so failures are reproducible.
    fn corpus_paths() -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(corpus_dir()) else {
            return Vec::new();
        };

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| IMAGE_EXTENSIONS_FOR_TESTS.contains(&e.to_lowercase().as_str()))
                    .unwrap_or(false)
            })
            .collect();
        paths.sort();
        paths
    }

    const IMAGE_EXTENSIONS_FOR_TESTS: &[&str] =
        &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "ico"];

    fn name_of(path: &Path) -> String {
        path.file_name().unwrap().to_string_lossy().to_string()
    }

    #[test]
    fn corpus_is_present_and_readable() {
        let paths = corpus_paths();
        assert!(
            !paths.is_empty(),
            "no images found in {} — the corpus tests need it populated",
            corpus_dir().display()
        );

        for path in &paths {
            let info = read_image_info(path.to_str().unwrap())
                .unwrap_or_else(|e| panic!("{}: {}", name_of(path), e));
            assert!(info.width > 0 && info.height > 0, "{} has no size", info.name);
        }
    }

    /// Runs the whole pipeline over every corpus image, into every output format, and checks
    /// the result is a decodable image of exactly the planned dimensions.
    #[test]
    fn every_corpus_image_converts_to_every_format() {
        let dir = scratch_dir();

        for path in corpus_paths() {
            let name = name_of(&path);
            let source = path.to_str().unwrap();
            let info = read_image_info(source).expect("info");

            for format in ["png", "jpg", "webp", "gif", "bmp", "tiff"] {
                let mut opts = options(format, OutputMode::CustomFolder);
                opts.output_folder = Some(dir.to_str().unwrap().to_string());
                // Downscale so the test stays quick on 12-megapixel photographs.
                opts.resize = Some(fit(Some(320), Some(320)));

                let mut reserved = HashSet::new();
                let dest = get_output_path(source, &opts, &mut reserved).expect("plan path");
                let result = convert_single_image(source, dest.clone(), &opts);
                assert!(
                    result.success,
                    "{} -> {} failed: {:?}",
                    name, format, result.error
                );

                let expected = planned_dimensions(
                    info.width,
                    info.height,
                    opts.crop.as_ref(),
                    opts.resize.as_ref(),
                );
                let decoded = image::open(&dest)
                    .unwrap_or_else(|e| panic!("{} -> {} unreadable: {}", name, format, e));
                assert_eq!(
                    (decoded.width(), decoded.height()),
                    expected,
                    "{} -> {} dimensions",
                    name,
                    format
                );
            }
        }
    }

    /// Cropping and resizing every corpus image, checking the output matches the plan. Real
    /// images matter here because their odd dimensions expose rounding the synthetic ones don't.
    #[test]
    fn crop_and_resize_match_the_plan_across_the_corpus() {
        let dir = scratch_dir();

        let cases = [
            (crop(0.0, 0.0, 100.0, 100.0), fit(Some(200), None)),
            (crop(10.0, 20.0, 55.0, 45.0), fit(Some(200), Some(200))),
            (crop(33.3, 33.3, 33.4, 33.4), stretch(Some(150), Some(90))),
            (crop(0.0, 0.0, 7.0, 100.0), fit(Some(64), Some(64))),
        ];

        for path in corpus_paths() {
            let name = name_of(&path);
            let source = path.to_str().unwrap();
            let info = read_image_info(source).expect("info");

            for (i, (rect, resize)) in cases.iter().enumerate() {
                let mut opts = options("png", OutputMode::CustomFolder);
                opts.output_folder = Some(dir.to_str().unwrap().to_string());
                opts.crop = Some(*rect);
                opts.resize = Some(*resize);

                let dest = dir.join(format!("{}-case{}.png", info.width, i));
                let result = convert_single_image(
                    source,
                    dest.to_str().unwrap().to_string(),
                    &opts,
                );
                assert!(result.success, "{} case {} failed: {:?}", name, i, result.error);

                let expected =
                    planned_dimensions(info.width, info.height, Some(rect), Some(resize));
                let decoded = image::open(&dest).expect("decode");
                assert_eq!(
                    (decoded.width(), decoded.height()),
                    expected,
                    "{} case {}: plan disagreed with output",
                    name,
                    i
                );
                assert!(
                    decoded.width() >= 1 && decoded.height() >= 1,
                    "{} case {} collapsed to nothing",
                    name,
                    i
                );
            }
        }
    }

    /// Metadata preservation over real files, which is the only place genuine multi-kilobyte
    /// EXIF blocks and real ICC profiles get exercised.
    #[test]
    fn corpus_metadata_survives_where_the_format_allows() {
        let dir = scratch_dir();

        for path in corpus_paths() {
            let name = name_of(&path);
            let source = path.to_str().unwrap();

            let (_, planted) = decode_oriented(source).expect("decode");
            if planted.is_empty() {
                continue; // nothing to preserve for this input
            }

            for format in ["jpg", "png", "webp"] {
                let mut opts = options(format, OutputMode::CustomFolder);
                opts.output_folder = Some(dir.to_str().unwrap().to_string());
                opts.resize = Some(fit(Some(200), Some(200)));

                let dest = dir.join(format!("meta-{}.{}", name.replace('.', "_"), format));
                let result =
                    convert_single_image(source, dest.to_str().unwrap().to_string(), &opts);
                assert!(result.success, "{} -> {}: {:?}", name, format, result.error);

                let mut decoder = ImageReader::open(&dest)
                    .unwrap()
                    .with_guessed_format()
                    .unwrap()
                    .into_decoder()
                    .unwrap();

                if planted.exif.is_some() {
                    assert!(
                        decoder.exif_metadata().ok().flatten().is_some(),
                        "{} -> {}: exif was dropped",
                        name,
                        format
                    );
                }
                if planted.icc.is_some() {
                    assert!(
                        decoder.icc_profile().ok().flatten().is_some(),
                        "{} -> {}: icc profile was dropped",
                        name,
                        format
                    );
                }
            }
        }
    }

    /// Any preserved Exif must report "no transforms", because the rotation is already baked
    /// into the pixels. `002585160005.jpg` carries Rotate270, so this has a real case to catch.
    #[test]
    fn corpus_outputs_never_carry_a_stale_orientation_tag() {
        let dir = scratch_dir();
        let mut checked_a_rotated_source = false;

        for path in corpus_paths() {
            let name = name_of(&path);
            let source = path.to_str().unwrap();

            let mut decoder = ImageReader::open(source)
                .unwrap()
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap();
            let source_orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
            if source_orientation != Orientation::NoTransforms {
                checked_a_rotated_source = true;
            }

            let mut opts = options("jpg", OutputMode::CustomFolder);
            opts.output_folder = Some(dir.to_str().unwrap().to_string());
            opts.resize = Some(fit(Some(200), Some(200)));

            let dest = dir.join(format!("orient-{}.jpg", name.replace('.', "_")));
            let result = convert_single_image(source, dest.to_str().unwrap().to_string(), &opts);
            assert!(result.success, "{}: {:?}", name, result.error);

            if let Some(exif) = read_back_exif(&dest) {
                if let Some(found) = Orientation::from_exif_chunk(&exif) {
                    assert_eq!(
                        found,
                        Orientation::NoTransforms,
                        "{} kept orientation {:?}; viewers would rotate it again",
                        name,
                        found
                    );
                }
            }
        }

        assert!(
            checked_a_rotated_source,
            "corpus no longer contains an image with a non-trivial orientation tag, \
             so this test proves nothing — add one back"
        );
    }

    /// Rotated sources must come out with orientation applied, i.e. transposed dimensions.
    #[test]
    fn corpus_rotation_is_applied_to_pixels() {
        for path in corpus_paths() {
            let source = path.to_str().unwrap();
            let mut decoder = ImageReader::open(source)
                .unwrap()
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap();
            let (raw_w, raw_h) = decoder.dimensions();
            let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

            let (img, _) = decode_oriented(source).expect("decode");

            let expected = if orientation_swaps_axes(orientation) {
                (raw_h, raw_w)
            } else {
                (raw_w, raw_h)
            };
            assert_eq!(
                (img.width(), img.height()),
                expected,
                "{}: orientation {:?} not applied",
                name_of(&path),
                orientation
            );

            // read_image_info must agree, since the UI shows its numbers.
            let info = read_image_info(source).expect("info");
            assert_eq!((info.width, info.height), expected, "{} info mismatch", info.name);
        }
    }

    // --- size estimation ---

    /// Percentage error of the estimate against a real full encode.
    fn estimate_error_pct(
        img: &image::DynamicImage,
        opts: &ConversionOptions,
    ) -> f64 {
        let sample = build_sample(img);
        let estimated = estimate_one(&sample, opts).expect("estimate") as f64;

        let transformed = apply_transforms(img.clone(), opts.crop.as_ref(), opts.resize.as_ref());
        let actual = encode_to_bytes(&transformed, opts).expect("encode").len() as f64;

        (estimated / actual - 1.0) * 100.0
    }

    #[test]
    fn estimates_are_not_wildly_wrong_on_synthetic_content() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(1400, 900));

        for format in ["jpg", "webp", "png"] {
            for quality in [40u8, 85] {
                let mut opts = options(format, OutputMode::SameFolder);
                opts.quality = quality;

                let err = estimate_error_pct(&img, &opts);
                assert!(
                    err.abs() < 30.0,
                    "{} q{} estimate off by {:+.1}%",
                    format,
                    quality,
                    err
                );
            }
        }
    }

    /// Diagnostic, not an assertion: prints the estimator's error surface over the whole
    /// corpus. Run with `--ignored --nocapture` when tuning the sampling strategy.
    #[test]
    #[ignore = "diagnostic; run explicitly with --ignored --nocapture"]
    fn report_estimate_accuracy_over_corpus() {
        println!(
            "\n{:<24} {:<6} {:>6} {:>8}",
            "image", "fmt", "target", "err%"
        );

        let mut worst: (String, f64) = (String::new(), 0.0);

        for path in corpus_paths() {
            let name = name_of(&path);
            let (img, _) = decode_oriented(path.to_str().unwrap()).expect("decode");

            for format in ["jpg", "webp", "png"] {
                for target in [None, Some(1200u32), Some(600), Some(200)] {
                    let mut opts = options(format, OutputMode::SameFolder);
                    opts.resize = target.map(|t| fit(Some(t), None));

                    let err = estimate_error_pct(&img, &opts);
                    let label = target.map_or("native".to_string(), |t| t.to_string());
                    println!("{:<24} {:<6} {:>6} {:>+8.1}", name, format, label, err);

                    if err.abs() > worst.1.abs() {
                        worst = (format!("{} {} {}", name, format, label), err);
                    }
                }
            }
        }

        println!("\nworst: {} at {:+.1}%", worst.0, worst.1);
    }

    /// Sanity bound on the estimator across the whole corpus.
    ///
    /// This is deliberately loose. The estimate is a guide, not a measurement, and chasing
    /// every synthetic corner case is not worth it — a 600x600 image with a hard-edged
    /// transparent circle overestimates by ~90% because the quadrant sample tiles land on the
    /// opaque middle and miss the empty corners. What the test is really for is catching a
    /// return to the original implementation, which extrapolated from a downscaled proxy and
    /// *underestimated* by 50-84% on detailed images. Hence the tight floor and loose ceiling.
    #[test]
    fn corpus_estimates_stay_in_the_right_ballpark() {
        for path in corpus_paths() {
            let name = name_of(&path);
            let (img, _) = decode_oriented(path.to_str().unwrap()).expect("decode");

            for format in ["jpg", "webp", "png"] {
                for target in [None, Some(800u32), Some(300)] {
                    let mut opts = options(format, OutputMode::SameFolder);
                    opts.resize = target.map(|t| fit(Some(t), None));

                    let ratio = estimate_error_pct(&img, &opts) / 100.0 + 1.0;
                    assert!(
                        (0.7..2.1).contains(&ratio),
                        "{} {} target {:?}: estimate was {:.2}x the real size",
                        name,
                        format,
                        target,
                        ratio
                    );
                }
            }
        }
    }

    /// Downscaled outputs go through the whole-proxy path, which involves no extrapolation and
    /// should therefore land very close.
    #[test]
    fn downscaled_estimates_are_near_exact() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(1600, 1200));

        for format in ["jpg", "webp", "png"] {
            for target in [600u32, 300, 150] {
                let mut opts = options(format, OutputMode::SameFolder);
                opts.resize = Some(fit(Some(target), None));

                let err = estimate_error_pct(&img, &opts);
                assert!(
                    err.abs() < 10.0,
                    "{} resize to {}px estimate off by {:+.1}%",
                    format,
                    target,
                    err
                );
            }
        }
    }

    #[test]
    fn size_estimates_account_for_cropping() {
        // Kept under SAMPLE_PROXY_EDGE so both estimates take the same code path; comparing a
        // tile-extrapolated figure against a whole-proxy one would not be meaningful.
        let img = image::DynamicImage::ImageRgb8(noisy_image(600, 600));

        let mut opts = options("jpg", OutputMode::SameFolder);
        let full = estimate_one(&build_sample(&img), &opts).unwrap();

        // Cropping to a quarter of the area should roughly quarter the estimate.
        opts.crop = Some(crop(25.0, 25.0, 50.0, 50.0));
        let quarter = estimate_one(&build_sample(&img), &opts).unwrap();

        let ratio = quarter as f64 / full as f64;
        assert!(
            (0.15..0.35).contains(&ratio),
            "quarter-area crop gave ratio {:.2}, expected near 0.25",
            ratio
        );
    }

    #[test]
    fn small_images_are_sampled_whole_and_estimated_exactly() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(200, 150));
        let sample = build_sample(&img);
        assert!(sample.whole_image, "a small image should be sampled entirely");

        let opts = options("jpg", OutputMode::SameFolder);
        let err = estimate_error_pct(&img, &opts);
        assert!(err.abs() < 1.0, "whole-image sample should be near exact, got {:+.3}%", err);
    }

    #[test]
    fn sample_tiles_cover_the_grid_at_native_resolution() {
        let img = image::DynamicImage::ImageRgb8(noisy_image(2000, 1000));
        let sample = build_sample(&img);

        assert!(!sample.whole_image);
        assert_eq!(sample.tiles.len(), (SAMPLE_GRID * SAMPLE_GRID) as usize);
        assert_eq!(sample.source_width, 2000);
        assert_eq!(sample.source_height, 1000);
        for tile in &sample.tiles {
            assert_eq!(
                (tile.width(), tile.height()),
                (SAMPLE_TILE_EDGE, SAMPLE_TILE_EDGE),
                "tiles must be native-resolution crops, not downscales"
            );
        }
    }

    #[test]
    fn orientation_swapping_covers_the_transposing_variants() {
        for o in [
            Orientation::Rotate90,
            Orientation::Rotate270,
            Orientation::Rotate90FlipH,
            Orientation::Rotate270FlipH,
        ] {
            assert!(orientation_swaps_axes(o), "{:?} transposes", o);
        }
        for o in [
            Orientation::NoTransforms,
            Orientation::Rotate180,
            Orientation::FlipHorizontal,
            Orientation::FlipVertical,
        ] {
            assert!(!orientation_swaps_axes(o), "{:?} preserves axes", o);
        }
    }
}
