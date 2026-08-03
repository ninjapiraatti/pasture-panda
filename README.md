# Pasture Panda

A small desktop batch image converter for macOS. Drop a pile of images on the window, pick
an output format, hit Convert.

Built with [Tauri 2](https://tauri.app) — a TypeScript/Vite frontend over a Rust backend
that does the decoding and encoding with the [`image`](https://crates.io/crates/image)
crate.

## Status

Early, but no longer known to lose data. Conversions are written to a temporary file and
renamed into place, so a failed encode can't destroy a source file. EXIF and ICC survive
conversion to JPEG, PNG and WebP, and there's resizing, batch cropping and a live output-size
estimate. Remaining gaps are in [ISSUES.md](ISSUES.md): no HEIC input, and no progress bar or
cancel on a running batch.

Testing is uneven and worth knowing about before you trust a change: `cargo test` covers
the Rust conversion logic well, and nothing covers the running app. See
[Tests](#tests) below.

## Running it

Prerequisites: [Rust](https://rustup.rs), Node 18+, and Xcode Command Line Tools. Developed
against Rust 1.96 and Node 22.

```bash
npm install
npm run tauri dev
```

The first launch is slow while Cargo builds the dependency tree; after that, frontend edits
hot-reload and Rust edits trigger a rebuild.

To produce a bundled app:

```bash
npm run tauri build
```

Output lands in `src-tauri/target/release/bundle/` as a `.app` and a `.dmg`. The build is
unsigned, so macOS Gatekeeper will complain when it is opened on another machine.

`npm run dev` alone serves just the frontend at `localhost:1420`. It renders, but every
Tauri call fails — no file dialog, no conversion — so it is only useful for looking at CSS.

## Using it

1. **Add images** — drag them anywhere onto the window, or click the drop zone to open a
   file picker. Non-image files in a drop are ignored, and dropped folders are skipped
   rather than expanded. Each file shows its dimensions, format and size, and can be
   removed individually.
2. **Pick an output format** — PNG, JPEG, WebP, AVIF, GIF, BMP or TIFF.
3. **Set quality** — the slider appears for JPEG and WebP.
4. **Resize** — type a width, a height, or both. Blank means automatic.
   - *Keep aspect ratio* (on by default) fits each image inside the box you give, so one
     dimension is enough. With both set, images are fitted inside rather than stretched.
   - Turn it off for free resizing: each axis is set independently and images are stretched
     to exactly those numbers. A blank axis keeps the source's own size.
   - *Never enlarge* (on by default) stops small images being scaled up to fill the box.
5. **Crop** — tick *Crop all images* and drag the rectangle over the preview, or type exact
   percentages. Drag inside the box to move it, drag a handle to resize it.
   - The crop is **one region shared by the whole batch**, stored as percentages, so it lands
     in the same relative place on every image regardless of their sizes. It is not per-image.
   - Click any file in the list to frame the crop against that image instead.
   - Cropping happens **before** resizing, so the resize box describes the visible region.
6. **Decide about metadata** — *Keep EXIF and colour profile* is on by default. Uncheck it to
   strip capture date, GPS and camera data from the output. The checkbox disables itself for
   formats that can't carry metadata (see below).
7. **Choose where output goes:**
   - *Same folder as original* — writes alongside the input. Never overwrites: an existing
     `photo.webp` means the new file becomes `photo_1.webp`.
   - *Custom folder* — everything into one chosen directory, with the same collision-safe
     renaming.
   - *Replace original files* — overwrites the input when the format is unchanged, or
     writes the new file and deletes the original when it changed. Asks for confirmation
     first. Still the destructive option, but a failed conversion now leaves the original
     untouched.
8. **Convert** — the whole batch runs in one go, and the status line reports how many
   succeeded.

The file list shows each image's target size (`3456x3456 → 800x800`) as you change settings,
and an estimated total output size sits above the Convert button. Files that can't be read are
named in the status line rather than silently skipped.

Conversion runs in parallel across all cores (rayon) on a background thread, so the window
stays responsive. There is no progress bar and no way to cancel a running batch — a large
AVIF batch in particular can take a while with nothing to show for it.

## Formats

**Input:** PNG, JPEG, GIF, WebP, BMP, TIFF, ICO.

AVIF is deliberately *not* an input format. Decoding it needs the `image` crate's
non-default `avif-native` feature, which links dav1d as a build-time system dependency; the
app used to advertise AVIF input and then fail on it (issue 4).

**Output:** PNG, JPEG, **WebP (lossy)**, AVIF, GIF, BMP, TIFF.

The quality slider applies to JPEG and WebP. For WebP, 100 means lossless.

**Metadata** — EXIF and ICC profiles are carried into **JPEG, PNG and WebP** output, so capture
date, GPS, camera data and colour profile survive. EXIF orientation is applied to the pixels and
the stored tag reset, so photos come out upright without being double-rotated by viewers.

GIF, BMP, TIFF and AVIF output do **not** carry metadata — TIFF and AVIF permit it in principle,
but nothing here writes it. The UI disables the metadata checkbox and says so rather than
dropping it silently. XMP and IPTC are not carried for any format.

Remaining caveats:

- **No HEIC input** — conspicuous for a macOS image tool, since it's what iPhones shoot.
- **Preserved EXIF keeps its original stored dimensions.** After a rotation, crop or resize,
  the `PixelXDimension`/`PixelYDimension` tags still describe the original image. Viewers use
  the container dimensions, so this is cosmetic, but it is wrong in the file.

## Size estimation

The estimate above the Convert button is a real measurement of a sample, not a guess, but it
is still an estimate. It works two ways depending on the output size:

- **Output at or below 640px on the long edge** — a cached whole-image proxy is encoded at the
  exact target size. No extrapolation, so this is near-exact.
- **Larger output** — four native-resolution tiles are cut from the source, scaled by the same
  factor the real image gets, stitched, and encoded; bytes-per-pixel is then multiplied up.
  Container overhead is measured and subtracted separately so it isn't multiplied too.

Measured against the images in `src-tauri/test-images/`, real photographs land within about
25%. Synthetic edge cases do worse — a smooth gradient overestimates by ~30%, and an image
that is mostly hard-edged transparency by ~90%, because the sample tiles miss the empty parts.

The awkward part is why it can't be simpler: extrapolating from a *downscaled* proxy — the
obvious approach — underestimates by 50–84%, because shrinking an image averages away the
high-frequency detail that costs the bytes. Sampling at native resolution is the point.

Decoded samples are cached per source file, so dragging the quality slider re-encodes small
samples rather than re-decoding originals. The cache is pruned to the current selection.

## Tests

```bash
cd src-tauri && cargo test --release   # --release: the corpus tests decode 12-megapixel photos
```

58 tests covering the Rust side: output-path planning across all three output modes, batch
collision handling, the atomic-write guarantee, the WebP encoder, EXIF/ICC preservation, the
crop and resize maths, and size-estimation accuracy.

Two are worth knowing about:

- `encode_failure_after_decode_leaves_the_original_intact` reproduces the old replace-mode data
  loss by feeding an image too wide for JPEG through the encoder.
- `planned_dimensions_matches_what_conversion_produces` ties the dimensions shown in the UI to
  the dimensions actually written. `plan_output_dimensions` exists so the crop/resize rules have
  one implementation in Rust rather than a copy in TypeScript that drifts.

### The test-images corpus

`src-tauri/test-images/` holds real photographs plus generated images chosen to cover awkward
cases: EXIF orientation `Rotate270`, ICC profiles, alpha, greyscale, flat colour, hard edges,
pure noise, a 25:1 aspect ratio, and an image smaller than one sample tile. Several tests run
the whole pipeline over every file in the folder, so dropping more images in widens coverage
without touching any test code.

It has already earned its keep by catching two bugs that synthetic tests missed:

- **Greyscale → GIF failed outright** with "the encoder or decoder for Gif does not support the
  color type `L8`". The crate's encoders each accept a different subset of colour types and
  error rather than converting; `normalise_for_format` now converts up front.
- **`thumbnail()` enlarges images smaller than the box.** The estimation proxy was being
  upscaled and then scaled back down, and the resulting double resample underestimated by 30%.

A diagnostic that prints the estimator's full error surface is available with:

```bash
cargo test --release --lib report_estimate_accuracy_over_corpus -- --ignored --nocapture
```

**The running app has no automated coverage.** There's no frontend test tooling in the
project, and Tauri's WebDriver support doesn't extend to macOS, so the UI, drag and drop,
IPC and the file dialogs are hand-checked. ISSUES.md has a "Needs manual verification"
checklist for exactly this.

One trap when testing security-related config: **the CSP does not apply under
`npm run tauri dev`.** Tauri only injects the CSP header for assets it serves itself, and in
dev the page comes from the Vite server. Use `npm run tauri build` to test anything
CSP-related.

## Layout

```
index.html                   markup for the single-screen UI
src/main.ts                  all frontend logic — file list, options, invoke calls
src/styles.css               styling
src-tauri/src/lib.rs         every Tauri command: image info, path planning, conversion
src-tauri/tauri.conf.json    window, bundle and dev-server config
ISSUES.md                    known defects and product notes
```

The Rust side exposes eight commands: `get_image_info`, `get_images_info`, `convert_images`,
`get_thumbnail`, `plan_output_dimensions`, `estimate_output_size`,
`get_supported_input_formats` and `get_supported_output_formats`. The interesting one is
`convert_images`, which reserves every destination path up front single-threaded
(`plan_output_paths`) before fanning the encoding out in parallel — doing it in the other order
races two inputs onto the same filename.

The transform order is fixed: **orientation, then crop, then resize**, then encode, then
metadata is spliced into the encoded container.

Five invariants in `lib.rs` worth not breaking:

- `convert_single_image` always encodes to a temp file and renames it into place. Writing
  straight to the destination is what used to destroy originals in replace mode.
- `read_image_info` reports orientation-corrected dimensions, because `decode_oriented`
  rotates on decode. If one changes, the other has to.
- `decode_oriented` clears the EXIF orientation tag on the metadata it carries forward. It
  bakes the rotation into the pixels, so leaving the tag set makes viewers rotate a second time.
- `move_png_exif_before_idat` exists because `img-parts` writes `eXIf` after `IDAT`, where most
  decoders never look. Removing it makes PNG metadata silently unreadable.
- `CropRect::to_pixels` clamps everything. The rectangle arrives from the UI, and `crop_imm`
  panics on out-of-range bounds.
