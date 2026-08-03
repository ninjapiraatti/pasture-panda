# Pasture Panda

A small desktop batch image converter for macOS. Drop a pile of images on the window, pick
an output format, hit Convert.

Built with [Tauri 2](https://tauri.app) — a TypeScript/Vite frontend over a Rust backend
that does the decoding and encoding with the [`image`](https://crates.io/crates/image)
crate.

## Status

Early, but no longer known to lose data. Conversions are written to a temporary file and
renamed into place, so a failed encode can't destroy a source file. Remaining gaps are in
[ISSUES.md](ISSUES.md): ICC profiles and EXIF metadata are still discarded (orientation
*is* applied now), there's no progress bar or cancel, and no resize.

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
4. **Choose where output goes:**
   - *Same folder as original* — writes alongside the input. Never overwrites: an existing
     `photo.webp` means the new file becomes `photo_1.webp`.
   - *Custom folder* — everything into one chosen directory, with the same collision-safe
     renaming.
   - *Replace original files* — overwrites the input when the format is unchanged, or
     writes the new file and deletes the original when it changed. Asks for confirmation
     first. Still the destructive option, but a failed conversion now leaves the original
     untouched.
5. **Convert** — the whole batch runs in one go, and the status line reports how many
   succeeded.

Files that can't be read are named in the status line rather than silently skipped.

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

Remaining caveats:

- **ICC profiles and EXIF metadata are dropped.** Capture date, GPS and camera data don't
  survive, and wide-gamut images shift colour. EXIF *orientation* is applied, so photos come
  out upright. Issue 7.
- **No resize, and no HEIC support** — the two most conspicuous absences for a macOS image
  tool. See the product notes in ISSUES.md.

## Tests

```bash
cd src-tauri && cargo test
```

20 tests covering the Rust side: output-path planning across all three output modes, batch
collision handling, the atomic-write guarantee, and the WebP encoder. The most important one
is `encode_failure_after_decode_leaves_the_original_intact`, which reproduces the old
replace-mode data loss by feeding an image too wide for JPEG through the encoder.

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

The Rust side exposes five commands: `get_image_info`, `get_images_info`,
`convert_images`, `get_supported_input_formats` and `get_supported_output_formats`. The
interesting one is `convert_images`, which reserves every destination path up front
single-threaded (`plan_output_paths`) before fanning the encoding out in parallel — doing
it in the other order races two inputs onto the same filename.

Two invariants in `lib.rs` worth not breaking:

- `convert_single_image` always encodes to a temp file and renames it into place. Writing
  straight to the destination is what used to destroy originals in replace mode.
- `read_image_info` reports orientation-corrected dimensions, because `decode_oriented`
  rotates on decode. If one changes, the other has to.
