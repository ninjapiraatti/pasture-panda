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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConversionOptions {
    format: String,
    quality: u8,
    output_mode: OutputMode,
    output_folder: Option<String>,
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
    let write_with = |format: ImageFormat| {
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, format).map_err(|e| e.to_string())?;
        Ok(buf.into_inner())
    };

    match options.format.to_lowercase().as_str() {
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
        .invoke_handler(tauri::generate_handler![
            get_image_info,
            get_images_info,
            convert_images,
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
            preserve_metadata: true,
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
