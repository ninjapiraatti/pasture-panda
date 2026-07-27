use image::{codecs::jpeg::JpegEncoder, codecs::webp::WebPEncoder, ImageFormat, ImageReader};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
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

fn read_image_info(path: &str) -> Result<ImageInfo, String> {
    let img = image::open(path).map_err(|e| format!("Failed to open image: {}", e))?;

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
        width: img.width(),
        height: img.height(),
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

fn convert_single_image(
    input_path: &str,
    output_path: String,
    options: &ConversionOptions,
) -> ConversionResult {
    let img = match ImageReader::open(input_path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())
        .and_then(|r| r.decode().map_err(|e| e.to_string()))
    {
        Ok(img) => img,
        Err(e) => {
            return ConversionResult {
                success: false,
                input_path: input_path.to_string(),
                output_path: None,
                error: Some(format!("Failed to open image: {}", e)),
            }
        }
    };

    let format_lower = options.format.to_lowercase();
    let result = match format_lower.as_str() {
        "jpg" | "jpeg" => {
            let file = File::create(&output_path).map_err(|e| e.to_string());
            file.and_then(|f| {
                let writer = BufWriter::new(f);
                let encoder = JpegEncoder::new_with_quality(writer, options.quality);
                img.write_with_encoder(encoder).map_err(|e| e.to_string())
            })
        }
        "webp" => {
            // Note: image crate only supports lossless WebP
            let file = File::create(&output_path).map_err(|e| e.to_string());
            file.and_then(|f| {
                let writer = BufWriter::new(f);
                let encoder = WebPEncoder::new_lossless(writer);
                img.write_with_encoder(encoder).map_err(|e| e.to_string())
            })
        }
        "png" => img
            .save_with_format(&output_path, ImageFormat::Png)
            .map_err(|e| e.to_string()),
        "avif" => img
            .save_with_format(&output_path, ImageFormat::Avif)
            .map_err(|e| e.to_string()),
        "gif" => img
            .save_with_format(&output_path, ImageFormat::Gif)
            .map_err(|e| e.to_string()),
        "bmp" => img
            .save_with_format(&output_path, ImageFormat::Bmp)
            .map_err(|e| e.to_string()),
        "tiff" => img
            .save_with_format(&output_path, ImageFormat::Tiff)
            .map_err(|e| e.to_string()),
        _ => Err(format!("Unsupported format: {}", options.format)),
    };

    match result {
        Ok(_) => {
            // If replace mode and format changed, delete original
            if matches!(options.output_mode, OutputMode::ReplaceOriginal) && output_path != input_path
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
        Err(e) => ConversionResult {
            success: false,
            input_path: input_path.to_string(),
            output_path: None,
            error: Some(format!("Failed to save image: {}", e)),
        },
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
        "AVIF".to_string(),
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
            supports_quality: false, // image crate only supports lossless
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
