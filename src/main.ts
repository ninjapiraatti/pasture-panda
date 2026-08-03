import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open, ask } from "@tauri-apps/plugin-dialog";

// Input formats only. AVIF is absent on purpose — it is an output-only format here,
// because decoding it needs the image crate's non-default avif-native feature.
const IMAGE_EXTENSIONS = [
  "png",
  "jpg",
  "jpeg",
  "gif",
  "webp",
  "bmp",
  "ico",
  "tiff",
  "tif",
];

interface ImageInfo {
  path: string;
  name: string;
  width: number;
  height: number;
  format: string;
  size_bytes: number;
}

interface OutputFormatInfo {
  name: string;
  extension: string;
  supports_quality: boolean;
  supports_metadata: boolean;
}

interface CropRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface ResizeOptions {
  width: number | null;
  height: number | null;
  preserve_aspect: boolean;
  no_upscale: boolean;
}

interface PlannedSize {
  width: number;
  height: number;
}

interface SizeEstimate {
  estimated_bytes: number;
  source_bytes: number;
  counted: number;
  failed: number;
  approximate: boolean;
}

interface ConversionOptions {
  format: string;
  quality: number;
  output_mode: "same_folder" | "custom_folder" | "replace_original";
  output_folder: string | null;
  crop: CropRect | null;
  resize: ResizeOptions | null;
  preserve_metadata: boolean;
}

const FULL_FRAME: CropRect = { x: 0, y: 0, width: 100, height: 100 };

/// Smallest crop the UI allows, as a percentage of the image.
const MIN_CROP_PERCENT = 2;

interface ConversionResult {
  success: boolean;
  input_path: string;
  output_path: string | null;
  error: string | null;
}

interface BatchConversionResult {
  total: number;
  succeeded: number;
  failed: number;
  results: ConversionResult[];
}

let selectedImages: ImageInfo[] = [];
let outputFormats: OutputFormatInfo[] = [];
let outputFolder: string | null = null;

/// One crop region shared by the whole batch, in percentages so it applies to any size.
let cropRect: CropRect = { ...FULL_FRAME };
/// Which image the crop preview shows. The crop applies to all of them regardless.
let previewPath: string | null = null;
/// Target dimensions per selected image, keyed by path, from plan_output_dimensions.
let plannedSizes = new Map<string, PlannedSize>();

let dropZone: HTMLElement;
let fileList: HTMLElement;
let fileListContainer: HTMLElement;
let conversionOptions: HTMLElement;
let formatSelect: HTMLSelectElement;
let qualityContainer: HTMLElement;
let qualitySlider: HTMLInputElement;
let qualityValue: HTMLElement;
let preserveMetadataCheckbox: HTMLInputElement;
let metadataHint: HTMLElement;
let resizeWidthInput: HTMLInputElement;
let resizeHeightInput: HTMLInputElement;
let lockAspectCheckbox: HTMLInputElement;
let noUpscaleCheckbox: HTMLInputElement;
let enableCropCheckbox: HTMLInputElement;
let cropEditor: HTMLElement;
let cropStage: HTMLElement;
let cropPreview: HTMLImageElement;
let cropBox: HTMLElement;
let cropCaption: HTMLElement;
let cropXInput: HTMLInputElement;
let cropYInput: HTMLInputElement;
let cropWInput: HTMLInputElement;
let cropHInput: HTMLInputElement;
let cropResetBtn: HTMLElement;
let estimateEl: HTMLElement;
let outputModeSelect: HTMLSelectElement;
let folderSelectBtn: HTMLElement;
let selectedFolderEl: HTMLElement;
let folderRow: HTMLElement;
let convertBtn: HTMLElement;
let clearBtn: HTMLElement;
let statusEl: HTMLElement;
let fileCountEl: HTMLElement;

document.addEventListener("DOMContentLoaded", () => {
  dropZone = document.getElementById("drop-zone")!;
  fileList = document.getElementById("file-list")!;
  fileListContainer = document.getElementById("file-list-container")!;
  conversionOptions = document.getElementById("conversion-options")!;
  formatSelect = document.getElementById("format-select") as HTMLSelectElement;
  qualityContainer = document.getElementById("quality-container")!;
  qualitySlider = document.getElementById("quality-slider") as HTMLInputElement;
  qualityValue = document.getElementById("quality-value")!;
  preserveMetadataCheckbox = document.getElementById("preserve-metadata") as HTMLInputElement;
  metadataHint = document.getElementById("metadata-hint")!;
  resizeWidthInput = document.getElementById("resize-width") as HTMLInputElement;
  resizeHeightInput = document.getElementById("resize-height") as HTMLInputElement;
  lockAspectCheckbox = document.getElementById("lock-aspect") as HTMLInputElement;
  noUpscaleCheckbox = document.getElementById("no-upscale") as HTMLInputElement;
  enableCropCheckbox = document.getElementById("enable-crop") as HTMLInputElement;
  cropEditor = document.getElementById("crop-editor")!;
  cropStage = document.getElementById("crop-stage")!;
  cropPreview = document.getElementById("crop-preview") as HTMLImageElement;
  cropBox = document.getElementById("crop-box")!;
  cropCaption = document.getElementById("crop-caption")!;
  cropXInput = document.getElementById("crop-x") as HTMLInputElement;
  cropYInput = document.getElementById("crop-y") as HTMLInputElement;
  cropWInput = document.getElementById("crop-w") as HTMLInputElement;
  cropHInput = document.getElementById("crop-h") as HTMLInputElement;
  cropResetBtn = document.getElementById("crop-reset")!;
  estimateEl = document.getElementById("estimate")!;
  outputModeSelect = document.getElementById("output-mode") as HTMLSelectElement;
  folderSelectBtn = document.getElementById("folder-select-btn")!;
  selectedFolderEl = document.getElementById("selected-folder")!;
  folderRow = document.getElementById("folder-row")!;
  convertBtn = document.getElementById("convert-btn")!;
  clearBtn = document.getElementById("clear-btn")!;
  statusEl = document.getElementById("status")!;
  fileCountEl = document.getElementById("file-count")!;

  init();
});

async function init() {
  try {
    outputFormats = await invoke<OutputFormatInfo[]>("get_supported_output_formats");
    populateFormatSelect();
    setupEventListeners();
    setCropRect({ ...FULL_FRAME });
    await setupDragDrop();
    updateUI();
  } catch (error) {
    console.error("Failed to initialize:", error);
  }
}

function populateFormatSelect() {
  formatSelect.textContent = "";
  for (const f of outputFormats) {
    const option = document.createElement("option");
    option.value = f.extension;
    option.textContent = f.name;
    formatSelect.append(option);
  }
}

function setupEventListeners() {
  dropZone.addEventListener("click", selectFiles);

  formatSelect.addEventListener("change", () => {
    updateQualityVisibility();
    updateMetadataAvailability();
    scheduleEstimate();
  });
  qualitySlider.addEventListener("input", () => {
    qualityValue.textContent = `${qualitySlider.value}%`;
    scheduleEstimate();
  });

  for (const input of [resizeWidthInput, resizeHeightInput]) {
    input.addEventListener("input", () => void refreshPlan());
  }
  for (const box of [lockAspectCheckbox, noUpscaleCheckbox]) {
    box.addEventListener("change", () => void refreshPlan());
  }

  enableCropCheckbox.addEventListener("change", () => {
    updateCropVisibility();
    void refreshPlan();
  });
  for (const input of [cropXInput, cropYInput, cropWInput, cropHInput]) {
    input.addEventListener("input", cropRectFromInputs);
  }
  cropResetBtn.addEventListener("click", () => {
    setCropRect({ ...FULL_FRAME });
    void refreshPlan();
  });
  setupCropInteraction();

  preserveMetadataCheckbox.addEventListener("change", scheduleEstimate);

  outputModeSelect.addEventListener("change", updateFolderVisibility);
  folderSelectBtn.addEventListener("click", selectOutputFolder);

  convertBtn.addEventListener("click", convertImages);
  clearBtn.addEventListener("click", clearSelection);
}

async function selectFiles() {
  try {
    const selected = await open({
      multiple: true,
      filters: [{ name: "Images", extensions: IMAGE_EXTENSIONS }],
    });

    if (selected) {
      const paths = Array.isArray(selected) ? selected : [selected];
      await loadImages(paths);
    }
  } catch (error) {
    console.error("Failed to open file dialog:", error);
    showStatus(`Failed to open file dialog: ${error}`, "error");
  }
}

function basename(path: string): string {
  return path.split(/[/\\]/).pop() || path;
}

function isSupportedImage(path: string): boolean {
  const match = /\.([^./\\]+)$/.exec(path);
  return match !== null && IMAGE_EXTENSIONS.includes(match[1].toLowerCase());
}

// The webview intercepts OS drops, so HTML5 drag events never reach us and a dropped
// File carries no filesystem path. Tauri's own drag/drop event is the only source of
// real paths, and it covers the whole window rather than just the drop zone.
async function setupDragDrop() {
  await getCurrentWebview().onDragDropEvent((event) => {
    const payload = event.payload;

    if (payload.type === "enter" || payload.type === "over") {
      dropZone.classList.add("drag-over");
      return;
    }

    dropZone.classList.remove("drag-over");

    if (payload.type === "drop") {
      const paths = payload.paths.filter(isSupportedImage);
      if (paths.length > 0) {
        void loadImages(paths);
      } else if (payload.paths.length > 0) {
        showStatus("No supported image files in that drop", "error");
      }
    }
  });
}

async function loadImages(paths: string[]) {
  showStatus("Loading images...", "info");

  try {
    const results = await invoke<({ Ok: ImageInfo } | { Err: string })[]>("get_images_info", { paths });

    let loadedCount = 0;
    let duplicateCount = 0;
    const failed: string[] = [];

    results.forEach((result, index) => {
      if ("Ok" in result) {
        const info = result.Ok;
        if (selectedImages.some((img) => img.path === info.path)) {
          duplicateCount++;
        } else {
          selectedImages.push(info);
          loadedCount++;
        }
      } else {
        // Files that fail to load used to vanish without explanation.
        failed.push(basename(paths[index]));
      }
    });

    updateUI();

    const parts: string[] = [];
    if (loadedCount > 0) parts.push(`Added ${loadedCount} image${loadedCount > 1 ? "s" : ""}`);
    if (duplicateCount > 0) parts.push(`${duplicateCount} already in the list`);
    if (failed.length > 0) {
      const shown = failed.slice(0, 3).join(", ");
      const rest = failed.length > 3 ? ` and ${failed.length - 3} more` : "";
      parts.push(`could not read ${shown}${rest}`);
    }

    if (parts.length === 0) {
      showStatus("No new images added", "info");
    } else {
      showStatus(parts.join(" · "), failed.length > 0 ? "error" : "success");
    }

    await refreshPlan();
  } catch (error) {
    console.error("Failed to load images:", error);
    showStatus(`Failed to load images: ${error}`, "error");
  }
}

function updateUI() {
  const hasImages = selectedImages.length > 0;

  fileListContainer.style.display = hasImages ? "block" : "none";
  conversionOptions.style.display = hasImages ? "block" : "none";

  fileCountEl.textContent = `${selectedImages.length} file${selectedImages.length !== 1 ? "s" : ""} selected`;

  renderFileList();
  updateQualityVisibility();
  updateMetadataAvailability();
  updateCropVisibility();
  updateFolderVisibility();
}

// Filenames are attacker-controlled enough to matter: a file named
// `<img src=x onerror=...>.png` would execute if interpolated into innerHTML. Everything
// here is built as DOM nodes with textContent so filename bytes are never parsed as markup.
function renderFileList() {
  fileList.textContent = "";

  selectedImages.forEach((img, index) => {
    const name = document.createElement("span");
    name.className = "file-name";
    name.textContent = img.name;
    name.title = img.path;

    // Show the target size only when crop or resize actually change it.
    const planned = plannedSizes.get(img.path);
    const resized =
      planned && (planned.width !== img.width || planned.height !== img.height);
    const dimensions = resized
      ? `${img.width}x${img.height} → ${planned!.width}x${planned!.height}`
      : `${img.width}x${img.height}`;

    const meta = document.createElement("span");
    meta.className = "file-meta";
    meta.textContent = `${dimensions} · ${img.format} · ${formatBytes(img.size_bytes)}`;

    const info = document.createElement("div");
    info.className = "file-info";
    info.append(name, meta);

    const remove = document.createElement("button");
    remove.className = "remove-btn";
    remove.title = "Remove";
    remove.textContent = "×";
    remove.addEventListener("click", (event) => {
      event.stopPropagation(); // don't also select it as the crop preview
      selectedImages.splice(index, 1);
      if (previewPath === img.path) previewPath = null;
      updateUI();
      void refreshPlan();
    });

    const item = document.createElement("div");
    item.className = "file-item";
    if (enableCropCheckbox?.checked && img.path === previewPath) {
      item.classList.add("selected");
    }
    item.append(info, remove);

    // Clicking a file frames the crop against it. Harmless when cropping is off.
    item.addEventListener("click", () => {
      if (previewPath === img.path) return;
      previewPath = img.path;
      renderFileList();
      if (enableCropCheckbox.checked) void loadCropPreview();
    });

    fileList.append(item);
  });
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

// --- resize and crop state ---

function parseDimension(input: HTMLInputElement): number | null {
  const value = parseInt(input.value, 10);
  return Number.isFinite(value) && value > 0 ? value : null;
}

/// The resize settings, or null when nothing has been asked for.
function currentResize(): ResizeOptions | null {
  const width = parseDimension(resizeWidthInput);
  const height = parseDimension(resizeHeightInput);
  if (width === null && height === null) return null;

  return {
    width,
    height,
    preserve_aspect: lockAspectCheckbox.checked,
    no_upscale: noUpscaleCheckbox.checked,
  };
}

function currentCrop(): CropRect | null {
  return enableCropCheckbox.checked ? cropRect : null;
}

/// Everything the backend needs, shared by conversion and estimation so the two can't drift.
function currentOptions(): ConversionOptions {
  return {
    format: formatSelect.value,
    quality: parseInt(qualitySlider.value),
    output_mode: outputModeSelect.value as ConversionOptions["output_mode"],
    output_folder: outputFolder,
    crop: currentCrop(),
    resize: currentResize(),
    // A disabled checkbox keeps its checked state, so gate on the format too.
    preserve_metadata: preserveMetadataCheckbox.checked && !preserveMetadataCheckbox.disabled,
  };
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}

function setCropRect(rect: CropRect, syncInputs = true) {
  // Keep the rectangle inside the frame and above the minimum size.
  const width = clamp(rect.width, MIN_CROP_PERCENT, 100);
  const height = clamp(rect.height, MIN_CROP_PERCENT, 100);
  cropRect = {
    width,
    height,
    x: clamp(rect.x, 0, 100 - width),
    y: clamp(rect.y, 0, 100 - height),
  };

  cropBox.style.left = `${cropRect.x}%`;
  cropBox.style.top = `${cropRect.y}%`;
  cropBox.style.width = `${cropRect.width}%`;
  cropBox.style.height = `${cropRect.height}%`;

  if (syncInputs) {
    const round = (n: number) => Math.round(n * 10) / 10;
    cropXInput.value = String(round(cropRect.x));
    cropYInput.value = String(round(cropRect.y));
    cropWInput.value = String(round(cropRect.width));
    cropHInput.value = String(round(cropRect.height));
  }
}

function cropRectFromInputs() {
  const num = (input: HTMLInputElement, fallback: number) => {
    const value = parseFloat(input.value);
    return Number.isFinite(value) ? value : fallback;
  };
  setCropRect(
    {
      x: num(cropXInput, cropRect.x),
      y: num(cropYInput, cropRect.y),
      width: num(cropWInput, cropRect.width),
      height: num(cropHInput, cropRect.height),
    },
    false
  );
  refreshPlan();
}

/// Drag to move the selection, or drag a handle to resize it. Everything is computed in
/// percentages against the rendered preview, which is why the stage is sized to the image.
function setupCropInteraction() {
  let handle: string | null = null;
  let origin = { x: 0, y: 0 };
  let startRect = { ...FULL_FRAME };

  cropStage.addEventListener("pointerdown", (event) => {
    const target = event.target as HTMLElement;
    const grabbed = target.dataset.handle;
    if (!grabbed && target !== cropBox) return;

    handle = grabbed ?? "move";
    origin = { x: event.clientX, y: event.clientY };
    startRect = { ...cropRect };
    cropStage.setPointerCapture(event.pointerId);
    event.preventDefault();
  });

  cropStage.addEventListener("pointermove", (event) => {
    if (!handle) return;

    const bounds = cropPreview.getBoundingClientRect();
    if (bounds.width === 0 || bounds.height === 0) return;
    const dx = ((event.clientX - origin.x) / bounds.width) * 100;
    const dy = ((event.clientY - origin.y) / bounds.height) * 100;

    if (handle === "move") {
      setCropRect({ ...startRect, x: startRect.x + dx, y: startRect.y + dy });
    } else {
      const next = { ...startRect };
      // Handle names combine edges, so "nw" drags both the north and west edges.
      if (handle.includes("w")) {
        const right = startRect.x + startRect.width;
        next.x = clamp(startRect.x + dx, 0, right - MIN_CROP_PERCENT);
        next.width = right - next.x;
      }
      if (handle.includes("e")) {
        next.width = clamp(startRect.width + dx, MIN_CROP_PERCENT, 100 - startRect.x);
      }
      if (handle.includes("n")) {
        const bottom = startRect.y + startRect.height;
        next.y = clamp(startRect.y + dy, 0, bottom - MIN_CROP_PERCENT);
        next.height = bottom - next.y;
      }
      if (handle.includes("s")) {
        next.height = clamp(startRect.height + dy, MIN_CROP_PERCENT, 100 - startRect.y);
      }
      setCropRect(next);
    }
  });

  const finish = (event: PointerEvent) => {
    if (!handle) return;
    handle = null;
    if (cropStage.hasPointerCapture(event.pointerId)) {
      cropStage.releasePointerCapture(event.pointerId);
    }
    refreshPlan();
  };

  cropStage.addEventListener("pointerup", finish);
  cropStage.addEventListener("pointercancel", finish);
}

async function loadCropPreview() {
  if (!enableCropCheckbox.checked || selectedImages.length === 0) return;

  // Default to the first image; the crop applies to all of them either way.
  if (!previewPath || !selectedImages.some((img) => img.path === previewPath)) {
    previewPath = selectedImages[0].path;
  }

  const target = selectedImages.find((img) => img.path === previewPath)!;
  try {
    cropPreview.src = await invoke<string>("get_thumbnail", { path: target.path });
    cropCaption.textContent =
      selectedImages.length > 1
        ? `Previewing ${target.name} — click another file to frame against it`
        : target.name;
  } catch (error) {
    console.error("Failed to load preview:", error);
    cropCaption.textContent = `Could not preview ${target.name}`;
  }
}

function updateCropVisibility() {
  const enabled = enableCropCheckbox.checked && selectedImages.length > 0;
  cropEditor.style.display = enabled ? "block" : "none";
  if (enabled) void loadCropPreview();
}

// --- planned dimensions and size estimate ---

/// Recomputes target dimensions and re-runs the estimate. Dimensions come from the backend so
/// there is a single implementation of the crop/resize rules rather than a drifting copy here.
async function refreshPlan() {
  await refreshPlannedSizes();
  scheduleEstimate();
}

async function refreshPlannedSizes() {
  if (selectedImages.length === 0) {
    plannedSizes.clear();
    return;
  }

  try {
    const planned = await invoke<PlannedSize[]>("plan_output_dimensions", {
      sources: selectedImages.map((img) => ({ width: img.width, height: img.height })),
      crop: currentCrop(),
      resize: currentResize(),
    });

    plannedSizes = new Map(
      selectedImages.map((img, index) => [img.path, planned[index]])
    );
    renderFileList();
  } catch (error) {
    console.error("Failed to plan dimensions:", error);
  }
}

let estimateTimer: number | undefined;
/// Guards against an older, slower estimate overwriting a newer one.
let estimateToken = 0;

function scheduleEstimate() {
  window.clearTimeout(estimateTimer);
  if (selectedImages.length === 0) {
    estimateEl.textContent = "";
    return;
  }

  estimateEl.textContent = "Estimating output size…";
  estimateTimer = window.setTimeout(() => void runEstimate(), 300);
}

async function runEstimate() {
  const token = ++estimateToken;
  const paths = selectedImages.map((img) => img.path);

  try {
    const estimate = await invoke<SizeEstimate>("estimate_output_size", {
      paths,
      options: currentOptions(),
    });
    if (token !== estimateToken) return; // superseded

    renderEstimate(estimate);
  } catch (error) {
    if (token !== estimateToken) return;
    console.error("Failed to estimate size:", error);
    estimateEl.textContent = "";
  }
}

function renderEstimate(estimate: SizeEstimate) {
  estimateEl.textContent = "";
  if (estimate.counted === 0) return;

  const prefix = estimate.approximate ? "≈ " : "";
  const value = document.createElement("span");
  value.className = "estimate-value";
  value.textContent = `${prefix}${formatBytes(estimate.estimated_bytes)}`;

  const detail = document.createTextNode(
    ` from ${formatBytes(estimate.source_bytes)}` +
      (estimate.approximate ? " · estimated, not exact" : "") +
      (estimate.failed > 0 ? ` · ${estimate.failed} could not be read` : "")
  );

  estimateEl.append(document.createTextNode("Output "), value, detail);
}

function updateQualityVisibility() {
  const selectedFormat = outputFormats.find((f) => f.extension === formatSelect.value);
  qualityContainer.style.display = selectedFormat?.supports_quality ? "flex" : "none";
}

// Only JPEG, PNG and WebP output can carry EXIF/ICC. Rather than leave the checkbox looking
// effective for formats where it does nothing, disable it and say why.
function updateMetadataAvailability() {
  const selectedFormat = outputFormats.find((f) => f.extension === formatSelect.value);
  const supported = selectedFormat?.supports_metadata ?? false;

  preserveMetadataCheckbox.disabled = !supported;
  metadataHint.textContent = supported
    ? ""
    : `${selectedFormat?.name ?? "This format"} output cannot carry metadata`;
}

function updateFolderVisibility() {
  folderRow.style.display = outputModeSelect.value === "custom_folder" ? "flex" : "none";
}

async function selectOutputFolder() {
  try {
    const selected = await open({
      directory: true,
      multiple: false,
    });

    if (selected) {
      outputFolder = selected as string;
      selectedFolderEl.textContent = outputFolder.split("/").pop() || outputFolder;
      selectedFolderEl.title = outputFolder;
    }
  } catch (error) {
    console.error("Failed to select folder:", error);
    showStatus(`Failed to select folder: ${error}`, "error");
  }
}

async function convertImages() {
  if (selectedImages.length === 0) return;

  const outputMode = outputModeSelect.value as ConversionOptions["output_mode"];

  if (outputMode === "replace_original") {
    const confirmed = await ask(
      "This will replace or delete the original files. Are you sure?",
      { title: "Confirm Replace", kind: "warning" }
    );
    if (!confirmed) return;
  }

  if (outputMode === "custom_folder" && !outputFolder) {
    showStatus("Please select an output folder", "error");
    return;
  }

  // Same builder the estimate uses, so what was previewed is what gets converted.
  const options = currentOptions();

  const paths = selectedImages.map((img) => img.path);

  try {
    convertBtn.setAttribute("disabled", "true");
    convertBtn.textContent = "Converting...";
    showStatus(`Converting ${paths.length} image${paths.length > 1 ? "s" : ""}...`, "info");

    const result = await invoke<BatchConversionResult>("convert_images", { paths, options });

    if (result.failed === 0) {
      showStatus(`Successfully converted ${result.succeeded} image${result.succeeded > 1 ? "s" : ""}`, "success");
      selectedImages = [];
      outputFolder = null;
      previewPath = null;
      plannedSizes.clear();
      selectedFolderEl.textContent = "None selected";
      estimateEl.textContent = "";
      updateUI();
    } else if (result.succeeded === 0) {
      const firstError = result.results.find((r) => r.error)?.error;
      showStatus(`All conversions failed: ${firstError}`, "error");
    } else {
      showStatus(
        `Converted ${result.succeeded}/${result.total}. ${result.failed} failed.`,
        "error"
      );
    }
  } catch (error) {
    showStatus(`Error: ${error}`, "error");
  } finally {
    convertBtn.removeAttribute("disabled");
    convertBtn.textContent = "Convert";
  }
}

function clearSelection() {
  selectedImages = [];
  outputFolder = null;
  previewPath = null;
  plannedSizes.clear();
  selectedFolderEl.textContent = "None selected";
  estimateEl.textContent = "";
  updateUI();
  showStatus("", "info");
}

function showStatus(message: string, type: "success" | "error" | "info") {
  statusEl.textContent = message;
  statusEl.className = `status ${type}`;
}
