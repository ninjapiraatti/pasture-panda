# Known issues

Findings from a review of the first working version. Numbering is stable — please don't
renumber when items close.

| # | Issue | Severity | Status |
|---|---|---|---|
| 1 | Conversion ran on the main thread | High | **Fixed** |
| 2 | Conversion was fully sequential | High | **Fixed** |
| 3 | Drag and drop never worked | High | **Fixed** |
| 4 | AVIF input advertised but not decodable | High | **Todo** |
| 5 | Replace mode can destroy the original file | High | Open |
| 6 | WebP output is lossless-only | Medium | Open |
| 7 | EXIF and ICC profiles are silently dropped | Medium | Open |
| 8 | Filenames are interpolated into `innerHTML` | Low | Open |
| 9 | Assorted smaller items | Low | Open |

---

## 1. Conversion ran on the main thread — Fixed

`convert_images` was a synchronous `#[tauri::command]`. Tauri runs non-async commands on
the main thread, which on macOS is the UI thread, so the window froze for the duration of
every batch.

Now `async`, with the decode/encode work inside `tauri::async_runtime::spawn_blocking`.
`get_image_info` and `get_images_info` had the same defect — adding a large selection
froze the window too — and got the same treatment.

## 2. Conversion was fully sequential — Fixed

`paths.iter().map(...)` converted one image at a time on one core. Now `par_iter()` over
rayon's pool (added as an explicit dependency; it was already in the tree via `image`).

This required a change to how destinations are chosen. `get_output_path` used to walk
`while output_path.exists()` to find a free filename, which is a time-of-check/time-of-use
race once conversions run concurrently: two inputs could both see `photo.webp` free and
then both write to it. Destinations are now reserved sequentially up front by
`plan_output_paths`, which threads a `HashSet<PathBuf>` of already-claimed paths through
`get_output_path`; only the encoding fans out.

Replace mode has no renaming to fall back on, so when two inputs in one batch resolve to
the same destination (e.g. `a.png` and `a.jpg` both converting to JPEG) the second now
fails with an explanatory error rather than racing. Previously this was a silent
overwrite.

Note that peak memory is now roughly `num_cpus` decoded images rather than one. That's
bounded by rayon's pool size, not by batch size, but it is a real increase — worth
revisiting if anyone reports trouble with very large TIFFs.

## 3. Drag and drop never worked — Fixed

The handler read `(file as any).path`, which is an Electron API. WKWebView's `File` has no
`.path`, so the collected path list was always empty. The fallback message
("Drag & drop requires the built app. Use file picker in dev mode.") was also wrong: it
would not have worked in the built app either.

Replaced with `getCurrentWebview().onDragDropEvent()`, which is the only source of real
filesystem paths. Two behaviour changes worth knowing about:

- The event covers the whole window, not just the drop zone, so a drop anywhere in the
  app now adds files. This seems like the better behaviour for a batch tool, but it is a
  change.
- Filtering is by file extension (`IMAGE_EXTENSIONS`, shared with the file dialog filter)
  rather than by MIME type, because a path is all we get. Dropped folders are ignored
  rather than expanded.

**Not verified end-to-end.** Both the Rust and TS sides compile, but confirming an actual
drag needs a human and a running app.

## 4. AVIF input advertised but not decodable — Todo

`get_supported_input_formats()` lists AVIF and the file picker filter accepts `.avif`, but
the app cannot read AVIF files.

In `image` 0.25, AVIF *encoding* comes from the `avif` feature (ravif), which **is** in
the default set — so AVIF output works. AVIF *decoding* comes from `avif-native`
(mp4parse + dav1d), which is **not** default. `image = "0.25.10"` pulls default features
only, so any AVIF input fails at decode with an opaque error.

Two ways out:

- **Enable decoding:** `image = { version = "0.25.10", features = ["avif-native"] }`. This
  links dav1d, which is a C dependency and needs to be available at build time — it adds
  a system prerequisite to the build and to CI, and needs checking against the bundling
  setup before committing to it.
- **Drop the claim:** remove AVIF from `get_supported_input_formats()` and from
  `IMAGE_EXTENSIONS` in `src/main.ts`. Keeps AVIF as an output-only format, which is
  honest and costs nothing.

Related: AVIF encoding through ravif at the `image` crate's default settings is slow. Now
that batches run in parallel this is less painful than it was, but a large AVIF batch is
still a long wait with no progress indication (see item 9).

## 5. Replace mode can destroy the original file — Open

When the output format matches the input format, `get_output_path` returns the input path
and `convert_single_image` opens it with `File::create`, which **truncates the original
before encoding begins**. A decode failure, a full disk, or a crash mid-write leaves the
source file destroyed with no copy anywhere.

Fix is the standard one: encode to a temporary file in the same directory, then
`fs::rename` over the original, which is atomic within a volume.

This is the highest-severity item still open — it is the only way the app can lose data,
and "Replace original files" is a normal-looking choice in the dropdown.

Also worth noting: replacing a JPEG with a JPEG re-encodes at the slider quality, so it is
a silent generation-loss step. The UI does not say so.

## 6. WebP output is lossless-only — Open

`WebPEncoder::new_lossless` is the only WebP encoder the `image` crate offers, so
converting a JPEG to WebP will usually produce a *larger* file — the opposite of why
people reach for WebP. `OutputFormatInfo.supports_quality` correctly reports `false`, so
the UI at least doesn't lie about the slider, but the format itself under-delivers.

Fixing means a dedicated encoder (`webp` crate / libwebp bindings). This is probably the
single highest-value functional change in the list — lossy WebP is the main reason a web
developer would pick this tool over what macOS already ships.

## 7. EXIF and ICC profiles are silently dropped — Open

The `image` crate does not carry metadata through a decode/encode round trip, and does not
apply EXIF orientation on decode. Consequences:

- iPhone photos come out rotated wrong.
- Wide-gamut images shift colour, because the ICC profile is discarded.
- Capture dates, GPS, copyright and camera data are lost with no warning.

At minimum, apply orientation on decode. Ideally preserve EXIF/ICC, with a "strip
metadata" checkbox for the people who want that (which is a real use case — it just
shouldn't be the silent default).

## 8. Filenames are interpolated into `innerHTML` — Open

`renderFileList` builds markup with `${img.name}` in a template string. macOS filenames
may contain `<` and `>`, so a file named `<img src=x onerror=...>.png` executes script.

Ordinarily minor, but the blast radius here is larger than usual: `tauri.conf.json` sets
`csp: null` and `withGlobalTauri: true`, and custom commands are not gated by capabilities
in Tauri v2. Injected script therefore reaches `window.__TAURI__.core.invoke` and can call
`convert_images` in `replace_original` mode against arbitrary paths.

Three small independent fixes: use `textContent` for filenames, set `withGlobalTauri` to
`false` (the frontend imports the module API and does not need the global), and configure
a real CSP.

## 9. Assorted smaller items — Open

- **No progress or cancel.** A batch is a single `invoke` with a static "Converting N
  images..." message and no way to stop. Emitting a Tauri event per completed file would
  give a real progress bar cheaply, and matters most for slow formats (item 4).
- **`read_image_info` fully decodes each image** just to read width and height.
  `ImageReader::into_dimensions()` is dramatically cheaper and avoids holding large
  decoded buffers when someone adds a few hundred files.
- **Load errors are silently discarded.** `get_images_info` returns
  `Vec<Result<ImageInfo, String>>`, and the frontend drops every `Err` without telling
  anyone. Files that fail to load just quietly don't appear in the list.
- **No tests.** `get_output_path` is pure, has real branching (three output modes, format
  normalisation, collision handling, reservation) and is the function most likely to
  cause data loss if it regresses. It should have unit tests.
- **No HEIC support.** Not a bug, but it is what every iPhone photo is, and its absence is
  conspicuous in a macOS image tool.

---

## Product notes

Not bugs — recorded so they don't get lost.

macOS has shipped a built-in **Finder → Quick Actions → Convert Image** since Ventura,
with format choice, a size option and a "preserve metadata" checkbox. That is the bar.
Right now this app does less: no resize, no metadata preservation, no HEIC, and WebP
output that is larger than the input.

The gaps that would give it a clear reason to exist, roughly in order of value:

1. **Lossy WebP and working AVIF** (items 4 and 6) — the formats Finder won't touch, and
   exactly what web work needs.
2. **Resize** — long-edge max, percentage, fit-in-box. The biggest missing feature for web
   use, arguably more than format choice.
3. **Saved presets** — e.g. "blog hero: 1600px wide, WebP q80, strip EXIF" as one click.
   Finder makes you re-pick settings every time.
4. **Watched folders** — drop a file into `~/to-optimize`, get a converted copy out.
   Nothing built-in does this, and it turns a utility into background infrastructure.

Separately: "Pasture Panda" is a charming name and completely unsearchable for "image
converter". Fine for a personal tool, worth reconsidering if it ever ships.
