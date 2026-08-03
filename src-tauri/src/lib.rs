use image::{
    codecs::jpeg::JpegEncoder, metadata::Orientation, ImageDecoder, ImageFormat, ImageReader,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::BufWriter;
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

fn decode_oriented(input_path: &str) -> Result<image::DynamicImage, String> {
    let mut decoder = ImageReader::open(input_path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;

    // Read orientation before the decoder is consumed. The `image` crate does not apply
    // it automatically, which is why unrotated iPhone photos used to come out sideways.
    let orientation = decoder
        .orientation()
        .unwrap_or(Orientation::NoTransforms);

    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// libwebp's hard limit on either dimension.
const WEBP_MAX_DIMENSION: u32 = 16383;

/// Lossy WebP via libwebp. The `image` crate only offers a lossless WebP encoder, which
/// made JPEG -> WebP conversions *larger* than their input — the opposite of the point.
///
/// `quality` is the UI slider, 1-100. 100 switches to lossless, matching how `cwebp`
/// treats the top of the range.
fn encode_webp(img: &image::DynamicImage, dest: &Path, quality: u8) -> Result<(), String> {
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

    fs::write(dest, &*encoded).map_err(|e| e.to_string())
}

/// Encodes to `dest`, which is expected to be a fresh temporary path.
fn encode_to(
    img: &image::DynamicImage,
    dest: &Path,
    options: &ConversionOptions,
) -> Result<(), String> {
    match options.format.to_lowercase().as_str() {
        "jpg" | "jpeg" => {
            let file = File::create(dest).map_err(|e| e.to_string())?;
            let writer = BufWriter::new(file);
            let encoder = JpegEncoder::new_with_quality(writer, options.quality);
            img.write_with_encoder(encoder).map_err(|e| e.to_string())
        }
        "webp" => encode_webp(img, dest, options.quality),
        "png" => img
            .save_with_format(dest, ImageFormat::Png)
            .map_err(|e| e.to_string()),
        "avif" => img
            .save_with_format(dest, ImageFormat::Avif)
            .map_err(|e| e.to_string()),
        "gif" => img
            .save_with_format(dest, ImageFormat::Gif)
            .map_err(|e| e.to_string()),
        "bmp" => img
            .save_with_format(dest, ImageFormat::Bmp)
            .map_err(|e| e.to_string()),
        "tiff" => img
            .save_with_format(dest, ImageFormat::Tiff)
            .map_err(|e| e.to_string()),
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

    let img = match decode_oriented(input_path) {
        Ok(img) => img,
        Err(e) => return failure(format!("Failed to open image: {}", e)),
    };

    // Always stage the encode in a temporary file and rename it into place. In replace
    // mode the destination *is* the input, so encoding straight into it would truncate
    // the original before the first byte is written — a decode error, a full disk or a
    // crash mid-write would leave no copy of the source anywhere.
    let final_path = Path::new(&output_path);
    let temp = temp_path_for(final_path, input_path);

    if let Err(e) = encode_to(&img, &temp, options) {
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
    vec![
        OutputFormatInfo {
            name: "PNG".to_string(),
            extension: "png".to_string(),
            supports_quality: false,
        },
        OutputFormatInfo {
            name: "JPEG".to_string(),
            extension: "jpg".to_string(),
            supports_quality: true,
        },
        OutputFormatInfo {
            name: "WebP".to_string(),
            extension: "webp".to_string(),
            supports_quality: true,
        },
        OutputFormatInfo {
            name: "AVIF".to_string(),
            extension: "avif".to_string(),
            supports_quality: false,
        },
        OutputFormatInfo {
            name: "GIF".to_string(),
            extension: "gif".to_string(),
            supports_quality: false,
        },
        OutputFormatInfo {
            name: "BMP".to_string(),
            extension: "bmp".to_string(),
            supports_quality: false,
        },
        OutputFormatInfo {
            name: "TIFF".to_string(),
            extension: "tiff".to_string(),
            supports_quality: false,
        },
    ]
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OutputFormatInfo {
    name: String,
    extension: String,
    supports_quality: bool,
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

        let lossy = dir.join("q60.webp");
        let lossless = dir.join("q100.webp");
        encode_webp(&img, &lossy, 60).expect("lossy encode");
        encode_webp(&img, &lossless, 100).expect("lossless encode");

        let lossy_size = fs::metadata(&lossy).unwrap().len();
        let lossless_size = fs::metadata(&lossless).unwrap().len();
        assert!(
            lossy_size < lossless_size,
            "expected lossy ({} bytes) to beat lossless ({} bytes)",
            lossy_size,
            lossless_size
        );

        // And the result must still be a readable WebP of the right size.
        let (w, h) = ImageReader::open(&lossy)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!((w, h), (256, 256));
    }

    #[test]
    fn webp_quality_slider_changes_output_size() {
        let dir = scratch_dir();
        let img = image::DynamicImage::ImageRgb8(noisy_image(128, 128));

        let low = dir.join("low.webp");
        let high = dir.join("high.webp");
        encode_webp(&img, &low, 20).unwrap();
        encode_webp(&img, &high, 95).unwrap();

        assert!(
            fs::metadata(&low).unwrap().len() < fs::metadata(&high).unwrap().len(),
            "quality 20 should be smaller than quality 95"
        );
    }

    #[test]
    fn webp_preserves_alpha() {
        let dir = scratch_dir();
        let mut img = image::RgbaImage::new(32, 32);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([255, 0, 0, if x < 16 { 0 } else { 255 }]);
        }
        let dest = dir.join("alpha.webp");
        encode_webp(&image::DynamicImage::ImageRgba8(img), &dest, 100).unwrap();

        let decoded = image::open(&dest).expect("decode webp");
        assert!(decoded.color().has_alpha(), "alpha channel must survive");
        assert_eq!(decoded.to_rgba8().get_pixel(0, 0)[3], 0, "left half transparent");
        assert_eq!(decoded.to_rgba8().get_pixel(31, 0)[3], 255, "right half opaque");
    }

    #[test]
    fn webp_rejects_oversized_images_with_a_clear_message() {
        let dir = scratch_dir();
        let img = image::DynamicImage::new_rgb8(WEBP_MAX_DIMENSION + 1, 1);
        let err = encode_webp(&img, &dir.join("huge.webp"), 80).unwrap_err();
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
