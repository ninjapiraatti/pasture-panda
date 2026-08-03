# Known issues

Findings from a review of the first working version. Numbering is stable — please don't
renumber when items close.

| # | Issue | Severity | Status |
|---|---|---|---|
| 1 | Conversion ran on the main thread | High | **Fixed** |
| 2 | Conversion was fully sequential | High | **Fixed** |
| 3 | Drag and drop never worked | High | **Fixed** |
| 4 | AVIF input advertised but not decodable | High | **Fixed** |
| 5 | Replace mode can destroy the original file | High | **Fixed** |
| 6 | WebP output is lossless-only | Medium | **Fixed** |
| 7 | EXIF and ICC profiles are silently dropped | Medium | **Fixed** (JPEG/PNG/WebP) |
| 8 | Filenames are interpolated into `innerHTML` | Low | **Fixed** |
| 9 | Assorted smaller items | Low | Partly fixed |
| 10 | Greyscale sources failed to convert to GIF | Medium | **Fixed** |
| 11 | `thumbnail()` enlarged small images | Low | **Fixed** |

## Verification status

Read this before trusting any "Fixed" above.

`cargo test --release` in `src-tauri/` runs 58 tests covering the Rust conversion logic:
output-path planning for all three output modes, batch collision handling, the atomic-write
behaviour, the WebP encoder, Exif/ICC preservation, the crop and resize maths, and
size-estimation accuracy. They call the conversion functions directly.

Several of them run over every image in `src-tauri/test-images/`, a corpus of real photographs
plus generated images covering orientation, ICC, alpha, greyscale, flat colour, hard edges,
noise, extreme aspect ratios and sub-tile sizes. Adding files to that folder widens coverage
without touching test code, and it has already caught two bugs that synthetic tests missed
(items 10 and 11).

**Nothing about the running app is covered by an automated test.** There is no frontend test
tooling in the project at all, and Tauri's WebDriver support does not extend to macOS, so
the UI, the drag/drop handler, IPC and the file dialogs are all verified by hand or not at
all. The checklist under "Needs manual verification" below is the part a human has to do.

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

## 4. AVIF input advertised but not decodable — Fixed

`get_supported_input_formats()` lists AVIF and the file picker filter accepts `.avif`, but
the app cannot read AVIF files.

In `image` 0.25, AVIF *encoding* comes from the `avif` feature (ravif), which **is** in
the default set — so AVIF output works. AVIF *decoding* comes from `avif-native`
(mp4parse + dav1d), which is **not** default. `image = "0.25.10"` pulls default features
only, so any AVIF input fails at decode with an opaque error.

Resolved by dropping the claim rather than adding the dependency: AVIF is gone from
`get_supported_input_formats()`, from `IMAGE_EXTENSIONS` in `src/main.ts`, and from the
drop-zone hint in `index.html`. AVIF remains an *output* format, which works on default
features. `avif_is_not_advertised_as_an_input_format` guards against it creeping back.

Enabling real AVIF decoding is still an option later:
`image = { version = "0.25.10", features = ["avif-native"] }`. It links dav1d, a C library
that has to be present at build time, so it adds a system prerequisite for every build
machine and needs checking against the bundling setup.

Related and still true: AVIF encoding through ravif at the `image` crate's default settings
is slow. A large AVIF batch is a long wait with no progress indication (see item 9).

## 5. Replace mode can destroy the original file — Fixed

When the output format matched the input format, `get_output_path` returned the input path
and `convert_single_image` opened it with `File::create`, which **truncated the original
before encoding began**. A decode failure, a full disk or a crash mid-write left the source
destroyed with no copy anywhere.

Every conversion now encodes to a hidden temporary file in the destination directory and
`fs::rename`s it into place, which is atomic within a volume. The temp name includes a hash
of the input path so parallel conversions cannot collide on it, and it is cleaned up on
failure. This applies to all output modes, not just replace — there is no reason for any of
them to expose a partially written file.

Two tests cover it. `encode_failure_after_decode_leaves_the_original_intact` is the real
regression test: it feeds a 70000x1 PNG to the JPEG encoder, which decodes fine and then
fails at encode time because JPEG cannot store a dimension above 65535 — precisely the
window where the old code had already truncated the destination. Reverting the staging logic
makes it fail with "original must still exist", i.e. the old code deleted the file outright.

Still true, and still not surfaced in the UI: replacing a JPEG with a JPEG re-encodes at the
slider quality, so it is a silent generation-loss step.

## 6. WebP output is lossless-only — Fixed

`WebPEncoder::new_lossless` was the only WebP encoder the `image` crate offers, so
converting a JPEG to WebP produced a *larger* file — the opposite of why people reach for
WebP.

Now encoded through the `webp` crate (libwebp bindings). `libwebp-sys` vendors the C source
and builds it with `cc`, so this adds a C compile step to the build but no system package.
`encode_webp` picks RGB or RGBA depending on whether the source actually has alpha, goes
through `from_rgb`/`from_rgba` rather than `from_image` so 16-bit PNG and TIFF sources are
handled, and treats slider quality 100 as lossless the way `cwebp` does. libwebp signals
failure with an empty buffer rather than an error, so that is checked explicitly, and the
16383px dimension limit is reported as a readable message instead of a generic failure.

`OutputFormatInfo.supports_quality` for WebP is now `true`, so the slider appears and applies.

Measured on an 1200x800 photographic test image saved as JPEG q85:

| | size |
|---|---|
| source JPEG q85 | 861 KB |
| old WebP (lossless) | 2717 KB |
| new WebP (lossy q85) | 600 KB |

So the old path was 3.2x larger than its own input; the new one is ~30% smaller.

Four tests cover the encoder: lossy beats lossless on noisy content, the quality slider
changes output size, alpha survives a round trip, and oversized images produce a clear error.

## 7. EXIF and ICC profiles are silently dropped — Fixed for JPEG, PNG and WebP

The `image` crate does not carry metadata through a decode/encode round trip, and does not
apply EXIF orientation on decode.

**Orientation is applied.** `decode_oriented` reads `ImageDecoder::orientation()` before the
decoder is consumed and calls `DynamicImage::apply_orientation`, so rotated iPhone photos come
out the right way up. The file list reports post-rotation dimensions to match, via
`orientation_swaps_axes` — otherwise a portrait photo would be listed as landscape.

**Exif and ICC now travel with the image.** `decode_oriented` also lifts the raw Exif block
and ICC profile off the decoder, and `attach_metadata` splices them into the encoded output
using `img-parts`. Capture date, GPS, camera data and colour profile survive, so wide-gamut
images no longer shift colour.

Because metadata is spliced into the encoded container, `encode_to_bytes` now encodes into
memory rather than streaming to the file. That costs one encoded image per worker thread,
which is small next to the decoded buffer already held.

**Two traps that had to be handled, both worth not undoing:**

1. **Double rotation.** The rotation is baked into the pixels, so carrying the original
   orientation tag forward would make every Exif-aware viewer rotate the output *again*.
   `Orientation::remove_from_exif_chunk` resets the tag to "no transforms" while leaving the
   rest of the block intact. `preserved_exif_has_its_orientation_tag_neutralised` covers this.
2. **PNG chunk order.** `img-parts` appends `eXIf` just before `IEND`, i.e. after `IDAT`. That
   is legal per the PNG spec but decoders commonly only surface ancillary chunks they meet
   *before* the image data — the `image` crate included — so the metadata read back as absent.
   `move_png_exif_before_idat` relocates it, and `png_exif_chunk_precedes_the_image_data`
   asserts the ordering rather than merely that the chunk exists.

**A "Keep EXIF and colour profile" checkbox** is in the options, checked by default, so
stripping is a deliberate choice. It disables itself with an explanatory hint when the chosen
output format cannot carry metadata, and `preserve_metadata` is gated on the format as well as
the checkbox, since a disabled checkbox keeps its checked state.

**Limits, all deliberate:**

- **Only JPEG, PNG and WebP output carry metadata.** TIFF and AVIF both permit Exif in
  principle, but nothing here writes it, and GIF/BMP have nowhere to put it. Rather than fail
  silently, `OutputFormatInfo.supports_metadata` reports this per format and the UI says so.
- **Reading metadata depends on the source decoder.** `image` surfaces Exif for JPEG, PNG,
  WebP and a few others; elsewhere it reports nothing and there is nothing to carry.
- **Exif's own stored dimensions are not rewritten.** `PixelXDimension`/`PixelYDimension` in a
  preserved block still describe the pre-rotation image. Viewers use the container dimensions
  for display, so this is cosmetic, but it is wrong in the file. Fixing it means editing Exif
  tags rather than passing the block through, which is a much larger job.
- **XMP and IPTC are still dropped.** `image` can read both
  (`xmp_metadata`, `iptc_metadata`); nothing here carries them.

## 8. Filenames are interpolated into `innerHTML` — Fixed

`renderFileList` built markup with `${img.name}` in a template string. macOS filenames may
contain `<` and `>`, so a file named `<img src=x onerror=...>.png` executed script. The blast
radius was larger than usual because `csp: null` and `withGlobalTauri: true` meant injected
script reached `window.__TAURI__.core.invoke` and could call `convert_images` in
`replace_original` mode against arbitrary paths.

All three fixes are in:

- `renderFileList` and `populateFormatSelect` build DOM nodes and assign filenames through
  `textContent`, so filename bytes are never parsed as markup. The remove button closes over
  its index instead of round-tripping through a `data-index` attribute.
- `withGlobalTauri` is `false`. The frontend imports the module API and never needed the global.
- A real CSP is configured, replacing `csp: null`:
  `default-src 'self'`, `script-src 'self'`, `object-src 'none'`, `frame-ancestors 'none'`,
  with `'unsafe-inline'` kept for `style-src` only because Vite injects style tags in dev.

Two things worth knowing about the CSP:

- **It does not apply in `tauri dev`.** Tauri injects the CSP header only for assets it serves
  itself; in dev the page comes from the Vite server. Only `tauri build` output is protected,
  so CSP changes have to be checked against a production build.
- Tauri's IPC uses `fetch()` to `ipc://localhost` on macOS, which *is* subject to
  `connect-src` — hence the `ipc:` source. If that source were wrong, Tauri catches the CSP
  error and silently falls back to the `postMessage` interface, so a mistake here degrades
  performance and logs a console warning rather than breaking the app.

## 9. Assorted smaller items — Partly fixed

**Fixed:**

- **`read_image_info` no longer fully decodes each image.** It goes through `into_decoder()`
  and reads `dimensions()` from the header, so adding a few hundred files no longer decodes
  and holds a few hundred images. (It also reads orientation there — see item 7.)
- **Load errors are surfaced.** `loadImages` now separates successes, duplicates and failures
  and reports all three in the status line, naming up to three failed files and counting the
  rest. Previously every `Err` from `get_images_info` was dropped and the file just silently
  didn't appear.
- **Tests exist.** 30 of them, described under "Verification status" at the top.

**Still open:**

- **No progress or cancel.** A batch is a single `invoke` with a static "Converting N
  images..." message and no way to stop. Emitting a Tauri event per completed file would
  give a real progress bar cheaply, and matters most for AVIF, which is slow (item 4).
- **No HEIC support.** Not a bug, but it is what every iPhone photo is, and its absence is
  conspicuous in a macOS image tool.
- **No frontend tests.** The XSS fix in item 8, the status-line logic and `isSupportedImage`
  are all plain functions that vitest + jsdom could cover. Nothing is installed today.

---

## 10. Greyscale sources failed to convert to GIF — Fixed

Found by the test corpus, not by inspection. Converting a greyscale PNG to GIF died with
"the encoder or decoder for Gif does not support the color type `L8`".

The `image` crate's encoders each accept a different subset of colour types and return an error
rather than converting. Any greyscale input therefore failed for GIF, and 16-bit inputs were
exposed to the same class of failure for several formats.

`normalise_for_format` now converts to a type the target encoder accepts before encoding,
keeping alpha where the format supports it and dropping it for JPEG. `every_corpus_image_converts_to_every_format`
covers the whole matrix of corpus images against every output format, so this class of bug
cannot come back quietly.

## Resize, crop and size estimation — added

Not a defect; recorded because the design has constraints worth knowing.

**Resize** is width/height with an aspect lock (on by default, fitting each image inside the
given box) and a never-enlarge option (also on). With the lock off, each axis is independent and
images stretch — "free resizing". A blank axis keeps the source's size. There is no percentage
mode; two fields covered the ask and adding a third input for it can wait.

**Crop is one rectangle for the whole batch**, stored as percentages so it lands in the same
relative place on any image size. It is set by dragging over a preview of whichever file is
selected. It is deliberately *not* per-image: per-image crops would need per-file state and a
crop editor per row, which is a much larger feature. The consequence is that a batch of mixed
aspect ratios gets the same *relative* region, not the same subject.

Crop runs before resize, so the resize box describes the visible region. `plan_output_dimensions`
is the single source of truth for the resulting size — the UI calls it rather than reimplementing
the rules in TypeScript, and `planned_dimensions_matches_what_conversion_produces` ties it to
what actually gets written.

**Size estimation** encodes a sample and extrapolates. Two regimes: outputs at or below 640px
on the long edge encode a cached whole-image proxy at the exact target size (near-exact, no
extrapolation); larger outputs use four native-resolution tiles, scaled by the factor the real
image gets, stitched into one image, with measured container overhead subtracted before
multiplying up.

The obvious approach — extrapolating from a downscaled proxy — was tried first and
**underestimated by 50-84%**, because shrinking an image averages away the high-frequency detail
that costs the bytes. Sampling at native resolution is the whole point, and
`corpus_estimates_stay_in_the_right_ballpark` has a tight floor and loose ceiling specifically to
catch a regression back to that.

Accuracy over the corpus: real photographs within ~25%, a smooth gradient ~+30%, and an image
that is mostly hard-edged transparency ~+90% because the quadrant tiles land on the opaque
middle and miss the empty corners. It is labelled an estimate in the UI and is not worth
perfecting further. Decoded samples are cached per file and pruned to the current selection, so
memory is bounded by the batch, not the session.

## 11. `thumbnail()` enlarged small images — Fixed

`DynamicImage::thumbnail` scales to *fit* its box, which means it enlarges anything smaller
than the box rather than leaving it alone.

Two places got this wrong. The size-estimation proxy was being upscaled and then scaled back
down to the output size, and the double resample softened the image enough to underestimate by
30%. `get_thumbnail` had the same issue, producing a blurry upscaled preview and a needlessly
large data URI for small images. Both now only shrink.

---

## Needs manual verification

Automated tests cover the Rust conversion logic only. These need a human with the app open,
and none of them have been confirmed since the changes above:

1. **Drag and drop** — still unverified from the original item 3 fix. Drop files anywhere on
   the window; they should be added. Dropped folders should be ignored, not expanded.
2. **A conversion end to end** — add a file, pick a format, convert. This is the only real
   check that IPC works under the new CSP. If the CSP were wrong the app would still work via
   the postMessage fallback, so also worth opening the webview inspector and confirming there
   is no "IPC custom protocol failed" warning.
3. **The WebP quality slider** — it now appears for WebP, where it previously did not. Confirm
   it is visible and that a lower value produces a smaller file.
4. **A rotated photo** — convert a photo straight off an iPhone and confirm the output is
   upright and that the dimensions in the file list match the output. Then check it is upright
   in *another* Exif-aware viewer too (Preview, Finder, a browser): if the orientation tag were
   not being cleared, the pixels would be right here and the image would appear rotated there.
5. **Metadata actually survives** — convert a photo with `Keep EXIF and colour profile` on and
   inspect the output with `exiftool` or Preview's inspector. Capture date, camera and GPS
   should be present. Do it for JPEG, PNG *and* WebP output; each uses a different container
   path. Then repeat with the box unchecked and confirm the output is clean.
6. **The metadata checkbox disables itself** — pick GIF, BMP, TIFF or AVIF output and confirm
   the checkbox greys out with a hint saying that format cannot carry metadata. The new
   checkbox row is also the only untested piece of layout; check it looks right in both light
   and dark mode.
7. **A wide-gamut image** — convert something with a non-sRGB profile and confirm the colours
   don't shift, which is the ICC half of the fix.
8. **Replace mode on a throwaway copy** — confirm the original is replaced correctly and no
   `.tmp` files are left in the folder. Use a copy; this is the destructive path.
9. **A file with `<` or `>` in its name** — e.g. `<img src=x onerror=alert(1)>.png`. The name
   should appear as literal text in the list.
10. **Errors are visible** — add a `.png` that isn't really a PNG and confirm the status line
    names it instead of silently skipping it.
11. **The crop box drags properly** — none of the pointer handling has been exercised in a real
    window. Check that dragging inside moves the selection, that all eight handles resize from
    the right edge, that the box can't leave the image or collapse below its minimum, and that
    the numeric percentage fields and the box stay in sync in both directions.
12. **The crop preview follows the file you pick** — click different files in the list and
    confirm the preview and its highlight follow, and that the crop rectangle stays put.
13. **A rotated image in the crop preview** — `002585160005.jpg` in `test-images/` has EXIF
    orientation `Rotate270`. Its preview must appear upright, otherwise the crop rectangle would
    be framed against a different orientation than the output.
14. **Resize behaves as described** — with *Keep aspect ratio* on, one dimension should be
    enough and the file list should show sensible targets. With it off, both fields should
    stretch to exactly those numbers. *Never enlarge* should stop a small image growing.
15. **The size estimate tracks the settings** — it should update as you drag the quality slider
    or change resize/crop, without visibly lagging on a large batch, and read as an estimate
    rather than a promise.
16. **Layout on a narrow window** — the crop editor, resize fields and estimate line are all new
    and unchecked in a real window, in both light and dark mode.

Note that a production build (`npm run tauri build`) is required to test anything CSP-related;
`tauri dev` does not apply the CSP at all.

---

## Product notes

Not bugs — recorded so they don't get lost.

macOS has shipped a built-in **Finder → Quick Actions → Convert Image** since Ventura,
with format choice, a size option and a "preserve metadata" checkbox. That is the bar.

The app now clears most of it: lossy WebP (which Finder won't produce), metadata preservation
with a strip toggle, resizing, batch cropping, and an output-size estimate before committing —
none of which Finder offers. It is still behind on HEIC input.

The gaps that would give it a clear reason to exist, roughly in order of value:

1. **Saved presets** — e.g. "blog hero: 1600px wide, WebP q80, strip EXIF" as one click. Now
   that there are this many knobs, re-picking them every time is the main friction, and Finder
   makes you do the same.
2. **HEIC input** — what every iPhone photo is.
3. **Watched folders** — drop a file into `~/to-optimize`, get a converted copy out.
   Nothing built-in does this, and it turns a utility into background infrastructure.
4. **Working AVIF input** (item 4) — output already works; decoding needs dav1d.
5. **Percentage resize** — deliberately left out to keep the resize controls to two fields;
   worth adding if "make everything 50%" comes up.

Separately: "Pasture Panda" is a charming name and completely unsearchable for "image
converter". Fine for a personal tool, worth reconsidering if it ever ships.
