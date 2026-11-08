# FlashOS: macOS image flasher (Tauri 2 + Rust)

## Goal
A native macOS app, "FlashOS", that writes `.iso`, `.img`, and `.img.xz` files to external USB drives and SD cards so the media boots. One window with three sections: 1. Image, 2. Target disk, 3. Flash.

## Context
- Project dir: `/Users/bryan/src/flash-os-images-tauri` (empty; not a git repo yet).
- Machine: macOS (Apple Silicon, arm64), Rust/cargo stable, bun, Xcode installed.
- Stack: Tauri 2, Rust backend, vanilla TypeScript + Vite frontend, bun as package manager/runner.
- Commands:
  - Dev: `bun run tauri dev`
  - Build: `bun run tauri build --bundles app` (output: `src-tauri/target/release/bundle/macos/FlashOS.app`)
  - Rust checks (in `src-tauri/`): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
  - Frontend type check: `bunx tsc --noEmit`
- Styling: Bryan's dark theme skill at `/Users/bryan/.pi/agent/skills/dark-theme/` (`SKILL.md` + `theme.css`). Read `SKILL.md` in full before any UI work and follow it exactly (tokens, 13px density, Open Sans Variable, Lucide icons, radii, focus = accent border, disabled = opacity 0.5).
- User shell is fish. Any command you tell the user to run must work in fish.

### Target layout
```
package.json, tsconfig.json, vite.config.ts, index.html
src/main.ts                  UI logic
src/styles/theme.css         copied unchanged from the dark-theme skill
src/styles/app.css           app-specific styles (tokens only, no hard-coded hexes)
src-tauri/Cargo.toml         package name `flashos`, lib `flashos_lib`
src-tauri/tauri.conf.json
src-tauri/capabilities/default.json
src-tauri/src/main.rs        calls flashos_lib::run()
src-tauri/src/lib.rs         Tauri builder, command registration
src-tauri/src/diskutil.rs    diskutil plist parsing (used by app AND helper)
src-tauri/src/image.rs       image kind detection + size info
src-tauri/src/flash.rs       app side: launches helper via osascript, tails progress, cancel
src-tauri/src/writer.rs      core copy/verify logic, generic over Read/Write (unit tested)
src-tauri/src/bin/flashos-helper.rs   privileged helper binary (runs as root)
```

## Decisions
1. Frontend: vanilla TS + Vite, no framework, no Tailwind. Use `theme.css` from the dark-theme skill. Dark only. Fonts: `@fontsource-variable/open-sans`. Icons: `lucide` npm package (vanilla).
2. bun is the package manager and script runner.
3. Privilege model: a bundled Rust helper binary `flashos-helper` does the raw write. The app runs it once per flash with `osascript -e 'do shell script "…" with administrator privileges'` (standard macOS password prompt). No SMAppService / launchd helper.
4. Only disks from `diskutil list -plist external physical` are listed or allowed. Internal, virtual, and boot disks are never shown. The helper re-checks this itself before writing (it must not trust the app).
5. After writing, the helper reads the disk back and compares SHA-256 against the bytes written. If it matches, it ejects the disk (`diskutil eject`).
6. No confirmation dialog before flashing. Clicking Flash goes straight to the macOS password prompt. While running, the Flash button becomes Cancel.
7. While idle, the disk list refreshes by itself every 2 s; the "Refresh disks" button refreshes immediately.
8. Image input: drag-and-drop onto the image box, OR click the box to open a file picker (filters: `iso`, `img`, `xz`).
9. Target: Apple Silicon only (`aarch64-apple-darwin`), `minimumSystemVersion` `13.0`, local unsigned build, no notarization. Product name `FlashOS`, identifier `com.bryan.flashos`.
10. Every image is written raw, byte for byte (`dd`-style). Windows ISOs get no special handling.
11. `.img.xz` is decompressed while it streams, in Rust, with the `liblzma` crate (feature `static`). No temp file and no external `xz` binary.
12. Write to the raw device `/dev/rdiskN` (not `/dev/diskN`) in 4 MiB chunks. Zero-pad the final chunk up to a 512-byte multiple. Verification and hashing cover only the real image bytes.
13. Progress/cancel channel between app (user) and helper (root): a per-flash temp dir created by the app (mode 0700) holding `progress.jsonl` (created empty by the app, appended to by the helper) and an optional `cancel` file (created by the app to request cancel).

## Tasks
Tick each box when done. Do not commit (the user approves commits).

- [x] 1. **Scaffold.** `git init`. Scaffold with `bun create tauri-app` (template `vanilla-ts`, manager `bun`) into this directory, or set up the equivalent by hand, matching the target layout above. Add `.gitignore` (node_modules, dist, src-tauri/target). In `tauri.conf.json`: `productName` "FlashOS", `identifier` "com.bryan.flashos", `bundle.macOS.minimumSystemVersion` "13.0", one window: title "FlashOS", 960×560, min 680×480, resizable, `theme: "Dark"`, native title bar. Check: `bun run tauri dev` opens an empty dark window titled FlashOS.

- [x] 2. **Theme + static UI.** Copy `theme.css` from the skill into `src/styles/theme.css`, install `@fontsource-variable/open-sans` and `lucide`. Build the static layout in `index.html`/`src/main.ts`/`src/styles/app.css` to match the screenshot the user provided:
  - Section labels "1. Image", "2. Target disk", "3. Flash" (theme section-label style).
  - Image box: full-width card with a Lucide `disc-3` icon and placeholder "Drop an .iso, .img, or .img.xz here".
  - Disk box: a scrollable list area (fills the spare height), an empty-state line "No external disks found. Plug in a USB drive or SD card.", and a "Refresh disks" button.
  - Flash box: status/progress area on the left, "Flash" primary button on the right (disabled by default).
  - Window content does not scroll; only the disk list scrolls.

- [x] 3. **`diskutil.rs`.** Add crate `plist` + `serde`. Implement:
  - `pub struct Disk { id: String /* "disk4" */, device: String /* "/dev/disk4" */, name: String /* MediaName */, size_bytes: u64 /* TotalSize */, bus: String /* BusProtocol, e.g. "USB", "Secure Digital" */, removable: bool, volumes: Vec<String> }` (Serialize, camelCase).
  - `pub fn list_external_disks() -> Result<Vec<Disk>>`: run `diskutil list -plist external physical`, take `WholeDisks`, run `diskutil info -plist <id>` for each, and pull volume names from `AllDisksAndPartitions[].Partitions[].VolumeName` (and `APFSVolumes` if present).
  - `pub fn is_allowed_target(id: &str) -> Result<bool>`: true only if `id` matches `^disk\d+$`, is in the list above, `Internal == false`, and is not the disk holding `/` (check `diskutil info -plist /` → `ParentWholeDisk`).
  - Debug builds only (`cfg(debug_assertions)`): if env `FLASHOS_ALLOW_VIRTUAL=1`, use `diskutil list -plist external` (includes RAM/disk images) so you can test with `hdiutil`. Release builds must ignore this env var.
  - Unit tests that parse saved sample plist strings (put the fixtures in `src-tauri/tests/fixtures/` or inline).

- [x] 4. **`image.rs`.** `pub enum ImageKind { Iso, Img, ImgXz }`, picked from the lowercase file name suffix (`.iso`, `.img`, `.img.xz`); reject anything else, including a bare `.xz`. `pub struct ImageInfo { path, name, kind, file_size: u64, size_known: bool /* false for ImgXz */ }`. Unit tests for the suffix rules.

- [x] 5. **`writer.rs` (core, testable without root).**
  - `pub fn copy_image<R: Read, W: Write>(src: R, dst: W, on_progress: impl FnMut(u64), should_cancel: impl Fn() -> bool) -> Result<CopyResult>` uses 4 MiB chunks, zero-pads the final chunk to a 512-byte multiple, and keeps a SHA-256 (`sha2` crate) of the unpadded bytes. `CopyResult { bytes_written: u64 /* unpadded */, sha256: [u8; 32] }`. A cancel returns a distinct `Cancelled` error.
  - `pub fn verify<R: Read>(dev: R, len: u64, expected: [u8;32], on_progress, should_cancel) -> Result<()>` reads exactly `len` bytes in 4 MiB chunks and compares the hash.
  - `pub fn open_source(info: &ImageInfo) -> Result<(Box<dyn Read>, CountingReader handle)>`: for `ImgXz`, wrap the file in `liblzma::read::XzDecoder`. Percent progress comes from compressed bytes read ÷ file size, so expose a counter of raw file bytes read.
  - Unit tests with temp files: plain copy round-trips; xz round-trips (compress a small buffer in the test); final chunk is padded; hash mismatch is detected; cancel stops.

- [x] 6. **Helper binary `src-tauri/src/bin/flashos-helper.rs`.** CLI: `flashos-helper --image <path> --disk <diskN> --work-dir <dir>`. Steps, appending one JSON line per event to `<work-dir>/progress.jsonl` and flushing each time:
  1. Refuse unless effective uid is 0. Refuse unless `is_allowed_target(diskN)`. Refuse if the image type is invalid or (for known-size images) the image is larger than the disk.
  2. `diskutil unmountDisk force /dev/diskN`.
  3. Open `/dev/rdiskN` read+write, `copy_image`, then `F_FULLFSYNC` (via `libc::fcntl`). Events: `{"phase":"writing","done":<bytes>,"total":<bytes>,"srcDone":<bytes>,"srcTotal":<bytes>}`, at most every 250 ms.
  4. Run `verify` on a fresh read of `/dev/rdiskN`. Events: `{"phase":"verifying","done":..,"total":..}`.
  5. `diskutil eject /dev/diskN` → `{"phase":"done","bytes":..}`.
  - Cancel: check whether `<work-dir>/cancel` exists before each chunk. If it does, stop, run `diskutil unmountDisk`, write `{"phase":"cancelled"}`, exit 2.
  - Any error: `{"phase":"error","message":"…"}`, exit 1. ENOSPC during an xz write → message "Image is larger than the disk".

- [x] 7. **`flash.rs` (app side) + Tauri commands in `lib.rs`.**
  - `list_disks() -> Vec<Disk>`; `image_info(path) -> ImageInfo`; `start_flash(image_path, disk_id)`; `cancel_flash()`.
  - `start_flash`: refuse if a flash is already running (state in `tauri::State<Mutex<…>>`). Create the work dir `std::env::temp_dir()/flashos-<uuid>` (mode 0700) and an empty `progress.jsonl`. Find the helper at `std::env::current_exe()?.parent()/flashos-helper` (works in dev under `target/debug` and in the bundle under `Contents/MacOS`). Build the AppleScript with `quoted form of` for each argument, escaping `\` and `"` in the AppleScript string literals, and run `osascript -e <script>` on a background thread.
  - While osascript runs, tail `progress.jsonl` every 200 ms and emit the Tauri event `flash://progress` with the parsed JSON, adding `rateBps` and `etaSecs` computed in the app.
  - When osascript exits: stderr containing `(-128)` → emit `{"phase":"cancelled"}` (the user dismissed the password prompt). Otherwise emit the helper's last line. If there is none, emit an error with osascript's stderr. Always delete the work dir afterwards.
  - `cancel_flash`: create `<work-dir>/cancel`.
  - Register `tauri-plugin-dialog` and add only the permissions you need to `capabilities/default.json`.

- [x] 8. **Wire up the UI (`src/main.ts`).**
  - Image: drag-and-drop via `getCurrentWebview().onDragDropEvent` (HTML5 drop gives no paths). Highlight the box border in `--accent` while dragging over. Clicking the box opens the dialog plugin's file picker. On select, call `image_info`, then show the file name, a human-readable size ("1.2 GB", decimal units), and a type badge, plus an `x` icon button to clear. Invalid files show an inline error in `--error`.
  - Disks: one row per disk with name, size, device (`/dev/disk4`, mono), bus, and volume names (dim). Click a row to select it (theme selected-row style). Disks smaller than a known-size image are shown disabled with "Too small". Poll `list_disks` every 2 s while idle. Keep the selection if the disk is still present; clear it if it disappeared. "Refresh disks" refreshes now. Show the empty-state line only when the list is empty.
  - Flash: enabled only when an image and an allowed disk are selected. Click → `start_flash`. During the run, lock the image and disk inputs and stop polling. Show a progress bar (`--running` color) plus status text: "Writing 42% · 38.1 MB/s · 2m 10s left", then "Verifying 70% …". The button turns into "Cancel" (`cancel_flash`). End states: done → "Done. Verified and ejected." with an `--ok` border; error → message in `--error` with an `--error-border` border; cancelled → "Cancelled". The disk may be partly written. After any end state, unlock and resume polling.

- [x] 9. **Bundle check.** Run `bun run tauri build --bundles app`. Confirm `FlashOS.app/Contents/MacOS/flashos-helper` exists. If the Tauri bundler did not include the `src/bin` binary, configure it as `bundle.externalBin` instead (with the `-aarch64-apple-darwin` suffix copied by a `beforeBuildCommand` step) and update the helper path lookup to match. Confirm the release helper ignores `FLASHOS_ALLOW_VIRTUAL`.

- [ ] 10. **Dev test with a virtual disk (no hardware).** In fish:
  `set dev (hdiutil attach -nomount ram://2097152 | string trim)` (1 GiB RAM disk), then run `FLASHOS_ALLOW_VIRTUAL=1 bun run tauri dev`. Flash a small `.img` and a `.img.xz` to it and confirm Done. Then `hdiutil detach $dev`. Record the results under Notes. (Needs the user to type the admin password, so it can't run headless.)

- [ ] 11. **Real hardware test (USER).** The user flashes a Raspberry Pi OS `.img.xz` to an SD card and a Linux `.iso` (e.g. Ubuntu) to a USB stick, then boots each one. The agent stops here and asks the user to confirm.

## Acceptance criteria
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` pass in `src-tauri/`; `bunx tsc --noEmit` passes.
- The app launches as a dark FlashOS window matching the screenshot layout and the dark-theme skill.
- Only external physical disks appear. Plugging or unplugging one updates the list within ~2 s.
- Flashing shows a password prompt, then live write and verify progress, then "Done. Verified and ejected.", and the media boots.
- Cancel at the password prompt or mid-write ends in "Cancelled" with no crash, and the app is usable again.
- The helper refuses (with an error) internal disks, the boot disk, non-`diskN` ids, and runs that aren't as root, even when called directly.

## Out of scope
- Confirmation dialogs, Windows ISO special handling, `.zip`/`.gz`/`.bz2`/`.dmg` inputs, downloading images, multi-disk writes, Intel/universal builds, code signing/notarization, auto-update, light theme, persistent privileged helper (SMAppService), settings/preferences.
- No dependencies beyond: tauri, tauri-plugin-dialog, serde/serde_json, plist, liblzma, sha2, libc, uuid, anyhow/thiserror (Rust); @tauri-apps/api, @tauri-apps/plugin-dialog, lucide, @fontsource-variable/open-sans (JS). Ask the user before adding others.

## Notes / risks
- **Data loss**: there is no confirmation, so the target safety checks in the helper (decision 4) are the only guard. Never loosen them. Never write to `/dev/disk0` or the boot disk's parent.
- macOS shows "The disk you attached was not readable by this computer" after flashing Linux/RPi images. This is expected and must not be treated as an error.
- Write to `rdiskN`, not `diskN`. The buffered device is about 10× slower.
- macOS may auto-mount partitions from the new partition table after the write. Verify reads the raw device, so this is fine. Eject at the end.
- The xz uncompressed size isn't known up front, so percent progress during writing uses compressed bytes, and too-small disks are caught only by ENOSPC.
- Do not use `do shell script` without `quoted form of` for every argument. Paths can contain spaces, quotes, and unicode.
- If writing to `/dev/rdiskN` fails with "Operation not permitted" even as root, the cause is likely TCC (Removable Volumes / Full Disk Access). Document what the user must allow, rather than working around it.

## Implementation notes
- No screenshot was available to the agent; layout follows the text spec and was checked in a mocked browser preview.
- Dev: `beforeDevCommand` runs `bun run dev:app`, which builds `flashos-helper` (debug) before Vite, because `tauri dev` only builds the main bin.
- Task 9: Tauri bundles every `[[bin]]`, so `Contents/MacOS/flashos-helper` is there without `externalBin`. `strings` finds no `FLASHOS_ALLOW_VIRTUAL` in the release helper or app (the code is compiled out).
- `osascript` drops the environment, so debug builds pass `FLASHOS_ALLOW_VIRTUAL=1` explicitly in the shell command. Release builds never do.
- Disk list also skips synthesized APFS containers (they show up in `diskutil list external` for disk images). Boot disk check covers `ParentWholeDisk` of `/` and its APFS physical stores.
- The helper caps writes at the disk size and maps overflow to ENOSPC → "Image is larger than the disk".
- Test images for task 10: `/tmp/flashos-test/test.img` (64 MiB + 1000 bytes, odd size to test padding) and `/tmp/flashos-test/rpi.img.xz`.
