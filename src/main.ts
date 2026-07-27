import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open, ask } from "@tauri-apps/plugin-dialog";

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
  "avif",
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
}

interface ConversionOptions {
  format: string;
  quality: number;
  output_mode: "same_folder" | "custom_folder" | "replace_original";
  output_folder: string | null;
}

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

let dropZone: HTMLElement;
let fileList: HTMLElement;
let fileListContainer: HTMLElement;
let conversionOptions: HTMLElement;
let formatSelect: HTMLSelectElement;
let qualityContainer: HTMLElement;
let qualitySlider: HTMLInputElement;
let qualityValue: HTMLElement;
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
    await setupDragDrop();
    updateUI();
  } catch (error) {
    console.error("Failed to initialize:", error);
  }
}

function populateFormatSelect() {
  formatSelect.innerHTML = outputFormats
    .map((f) => `<option value="${f.extension}">${f.name}</option>`)
    .join("");
}

function setupEventListeners() {
  dropZone.addEventListener("click", selectFiles);

  formatSelect.addEventListener("change", updateQualityVisibility);
  qualitySlider.addEventListener("input", () => {
    qualityValue.textContent = `${qualitySlider.value}%`;
  });

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
    for (const result of results) {
      if ("Ok" in result) {
        const info = result.Ok;
        if (!selectedImages.some((img) => img.path === info.path)) {
          selectedImages.push(info);
          loadedCount++;
        }
      }
    }

    updateUI();

    if (loadedCount > 0) {
      showStatus(`Added ${loadedCount} image${loadedCount > 1 ? "s" : ""}`, "success");
    } else {
      showStatus("No new images added", "info");
    }
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
  updateFolderVisibility();
}

function renderFileList() {
  fileList.innerHTML = selectedImages
    .map(
      (img, index) => `
      <div class="file-item">
        <div class="file-info">
          <span class="file-name">${img.name}</span>
          <span class="file-meta">${img.width}x${img.height} · ${img.format} · ${formatBytes(img.size_bytes)}</span>
        </div>
        <button class="remove-btn" data-index="${index}" title="Remove">×</button>
      </div>
    `
    )
    .join("");

  fileList.querySelectorAll(".remove-btn").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      const index = parseInt((e.target as HTMLElement).dataset.index!);
      selectedImages.splice(index, 1);
      updateUI();
    });
  });
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function updateQualityVisibility() {
  const selectedFormat = outputFormats.find((f) => f.extension === formatSelect.value);
  qualityContainer.style.display = selectedFormat?.supports_quality ? "flex" : "none";
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

  const options: ConversionOptions = {
    format: formatSelect.value,
    quality: parseInt(qualitySlider.value),
    output_mode: outputMode,
    output_folder: outputFolder,
  };

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
      selectedFolderEl.textContent = "None selected";
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
  selectedFolderEl.textContent = "None selected";
  updateUI();
  showStatus("", "info");
}

function showStatus(message: string, type: "success" | "error" | "info") {
  statusEl.textContent = message;
  statusEl.className = `status ${type}`;
}
