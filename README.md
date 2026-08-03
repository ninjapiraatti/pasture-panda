# Pasture Panda

A small desktop batch image converter for macOS. Drop a pile of images on the window, pick
an output format, hit Convert.

Built with [Tauri 2](https://tauri.app) — a TypeScript/Vite frontend over a Rust backend
that does the decoding and encoding with the [`image`](https://crates.io/crates/image)
crate.

## Status

Early. It works and it is useful, but it is a personal tool rather than a polished product.
Read [ISSUES.md](ISSUES.md) before trusting it with anything irreplaceable — in particular
**"Replace original files" can destroy a source file** if a conversion fails partway
through (issue 5), and image metadata is silently discarded (issue 7).

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
3. **Set quality** — the slider only appears for JPEG, which is the only format here with a
   quality parameter (see the WebP note below).
4. **Choose where output goes:**
   - *Same folder as original* — writes alongside the input. Never overwrites: an existing
     `photo.webp` means the new file becomes `photo_1.webp`.
   - *Custom folder* — everything into one chosen directory, with the same collision-safe
     renaming.
   - *Replace original files* — overwrites the input when the format is unchanged, or
     writes the new file and deletes the original when it changed. Asks for confirmation
     first. This is the destructive option; see issue 5.
5. **Convert** — the whole batch runs in one go, and the status line reports how many
   succeeded.

Conversion runs in parallel across all cores (rayon) on a background thread, so the window
stays responsive. There is no progress bar and no way to cancel a running batch — a large
AVIF batch in particular can take a while with nothing to show for it.

## Formats

**Input:** PNG, JPEG, GIF, WebP, BMP, TIFF, ICO.

The UI also lists AVIF as an input format, but **AVIF files cannot actually be decoded** —
that needs the `image` crate's non-default `avif-native` feature. Adding an AVIF file will
fail at conversion time with an unhelpful error. Tracked as issue 4.

**Output:** PNG, JPEG, WebP, AVIF, GIF, BMP, TIFF.

Two caveats worth knowing before reaching for this tool:

- **WebP output is lossless only.** The `image` crate offers no lossy WebP encoder, so
  converting a JPEG to WebP will usually produce a *larger* file. Issue 6.
- **EXIF and ICC data are dropped.** Orientation is not applied on decode, so iPhone photos
  come out rotated wrong, and wide-gamut images shift colour. Issue 7.

There is no resize option, and no HEIC support.

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

There are no tests yet.
