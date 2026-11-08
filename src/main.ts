import "./styles/theme.css";
import "./styles/app.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { createElement, Disc3, HardDrive, type IconNode, RefreshCw, Usb, X } from "lucide";

type ImageKind = "Iso" | "Img" | "ImgXz";
interface ImageInfo {
  path: string;
  name: string;
  kind: ImageKind;
  fileSize: number;
  sizeKnown: boolean;
}
interface Disk {
  id: string;
  device: string;
  name: string;
  sizeBytes: number;
  bus: string;
  removable: boolean;
  volumes: string[];
}
type Progress =
  | { phase: "writing"; done: number; total: number | null; srcDone: number; srcTotal: number; rateBps?: number; etaSecs?: number }
  | { phase: "verifying"; done: number; total: number; rateBps?: number; etaSecs?: number }
  | { phase: "done"; bytes: number }
  | { phase: "error"; message: string }
  | { phase: "cancelled" };

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const icon = (node: IconNode) => createElement(node, { class: "icon" });

const imageBox = $("image-box");
const imagePlaceholder = $("image-placeholder");
const imageSelected = $("image-selected");
const imageName = $("image-name");
const imageKind = $("image-kind");
const imageSize = $("image-size");
const imageError = $("image-error");
const imageClear = $<HTMLButtonElement>("image-clear");
const refreshBtn = $<HTMLButtonElement>("refresh");
const diskList = $("disk-list");
const diskEmpty = $("disk-empty");
const diskError = $("disk-error");
const flashBox = $("flash-box");
const statusEl = $("status");
const progressEl = $("progress");
const progressFill = $("progress-fill");
const flashBtn = $<HTMLButtonElement>("flash");

$("image-icon").append(icon(Disc3));
$("refresh-icon").append(icon(RefreshCw));
imageClear.append(icon(X));

let image: ImageInfo | null = null;
let disks: Disk[] = [];
let selectedId: string | null = null;
let running = false;
let cancelling = false;
let polling = false;

// ---------- formatting ----------

function fmtBytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1000 && i < units.length - 1) {
    n /= 1000;
    i++;
  }
  return i === 0 ? `${n} B` : `${n.toFixed(1)} ${units[i]}`;
}

function fmtEta(s: number): string {
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}

const kindLabel: Record<ImageKind, string> = { Iso: "ISO", Img: "IMG", ImgXz: "IMG.XZ" };

// ---------- image ----------

function renderImage() {
  imagePlaceholder.hidden = !!image;
  imageSelected.hidden = !image;
  imageClear.hidden = !image;
  if (image) {
    imageName.textContent = image.name;
    imageName.title = image.path;
    imageKind.textContent = kindLabel[image.kind];
    imageSize.textContent = image.sizeKnown ? fmtBytes(image.fileSize) : `${fmtBytes(image.fileSize)} compressed`;
  }
}

async function setImagePath(path: string) {
  if (running) return;
  try {
    image = await invoke<ImageInfo>("image_info", { path });
    imageError.hidden = true;
  } catch (e) {
    image = null;
    imageError.textContent = String(e);
    imageError.hidden = false;
  }
  renderImage();
  resetStatus();
  renderDisks();
}

async function pickImage() {
  if (running) return;
  const path = await open({
    multiple: false,
    directory: false,
    filters: [{ name: "Disk images", extensions: ["iso", "img", "xz"] }],
  });
  if (typeof path === "string") await setImagePath(path);
}

imageBox.addEventListener("click", pickImage);
imageBox.addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    pickImage();
  }
});
imageClear.addEventListener("click", (e) => {
  e.stopPropagation();
  if (running) return;
  image = null;
  imageError.hidden = true;
  renderImage();
  resetStatus();
  renderDisks();
});

getCurrentWebview().onDragDropEvent((event) => {
  const p = event.payload;
  if (running) return;
  if (p.type === "enter" || p.type === "over") {
    imageBox.classList.add("drag");
  } else if (p.type === "leave") {
    imageBox.classList.remove("drag");
  } else if (p.type === "drop") {
    imageBox.classList.remove("drag");
    if (p.paths.length > 0) setImagePath(p.paths[0]);
  }
});

// ---------- disks ----------

function tooSmall(d: Disk): boolean {
  return !!image && image.sizeKnown && image.fileSize > d.sizeBytes;
}

function selectedDisk(): Disk | undefined {
  return disks.find((d) => d.id === selectedId);
}

function renderDisks() {
  diskList.replaceChildren(
    ...disks.map((d) => {
      const small = tooSmall(d);
      const row = document.createElement("div");
      row.className = "row disk-row";
      row.setAttribute("role", "option");
      row.classList.toggle("current", d.id === selectedId && !small);
      row.classList.toggle("disabled", small);
      row.classList.toggle("locked", running);
      row.setAttribute("aria-selected", String(d.id === selectedId));

      const main = document.createElement("div");
      main.className = "disk-main";
      const name = document.createElement("span");
      name.className = "disk-name";
      name.textContent = d.name || "Untitled disk";
      main.append(name);
      if (d.volumes.length) {
        const vols = document.createElement("span");
        vols.className = "disk-vols";
        vols.textContent = d.volumes.join(", ");
        main.append(vols);
      }
      if (small) {
        const ts = document.createElement("span");
        ts.className = "too-small";
        ts.textContent = "Too small";
        main.append(ts);
      }
      const bus = document.createElement("span");
      bus.className = "disk-bus";
      bus.textContent = d.bus;
      const dev = document.createElement("span");
      dev.className = "disk-dev";
      dev.textContent = d.device;
      const size = document.createElement("span");
      size.className = "disk-size";
      size.textContent = fmtBytes(d.sizeBytes);

      row.append(icon(d.bus === "USB" ? Usb : HardDrive), main, bus, dev, size);
      row.addEventListener("click", () => {
        if (running || small) return;
        selectedId = d.id;
        resetStatus();
        renderDisks();
      });
      return row;
    }),
  );
  diskList.classList.toggle("locked", running);
  diskEmpty.hidden = disks.length > 0 || !diskError.hidden;
  updateFlashButton();
}

async function refreshDisks() {
  if (polling || running) return;
  polling = true;
  try {
    disks = await invoke<Disk[]>("list_disks");
    diskError.hidden = true;
  } catch (e) {
    disks = [];
    diskError.textContent = `Couldn't list disks: ${e}`;
    diskError.hidden = false;
  } finally {
    polling = false;
  }
  if (running) return; // A flash started while we were listing; keep the locked list.
  if (selectedId && !disks.some((d) => d.id === selectedId)) selectedId = null;
  renderDisks();
}

refreshBtn.addEventListener("click", refreshDisks);
setInterval(() => {
  if (!running) refreshDisks();
}, 2000);

// ---------- flash ----------

function canFlash(): boolean {
  const d = selectedDisk();
  return !!image && !!d && !tooSmall(d);
}

function updateFlashButton() {
  if (running) {
    flashBtn.disabled = cancelling;
    flashBtn.textContent = "Cancel";
    flashBtn.classList.add("cancel");
  } else {
    flashBtn.disabled = !canFlash();
    flashBtn.textContent = "Flash";
    flashBtn.classList.remove("cancel");
  }
}

function setStatus(text: string, cls: "" | "running" | "ok" | "error", textClass = "") {
  statusEl.textContent = text;
  statusEl.className = textClass;
  flashBox.classList.remove("running", "ok", "error");
  if (cls) flashBox.classList.add(cls);
}

function setProgress(fraction: number | null) {
  progressEl.hidden = fraction === null;
  if (fraction !== null) progressFill.style.width = `${Math.min(100, Math.max(0, fraction * 100)).toFixed(1)}%`;
}

function resetStatus() {
  if (running) return;
  setProgress(null);
  setStatus(canFlash() ? "Ready to flash." : "Choose an image and a target disk.", "", "dim");
}

function setRunning(on: boolean) {
  running = on;
  cancelling = false;
  imageBox.classList.toggle("locked", on);
  imageBox.tabIndex = on ? -1 : 0;
  refreshBtn.disabled = on;
  renderDisks();
}

function rateEta(p: { rateBps?: number; etaSecs?: number }): string {
  let s = "";
  if (p.rateBps) s += ` · ${fmtBytes(p.rateBps)}/s`;
  if (p.etaSecs !== undefined) s += ` · ${fmtEta(p.etaSecs)} left`;
  return s;
}

function onProgress(p: Progress) {
  switch (p.phase) {
    case "writing": {
      const frac = p.total ? p.done / p.total : p.srcTotal ? p.srcDone / p.srcTotal : 0;
      setProgress(frac);
      if (!cancelling) setStatus(`Writing ${Math.floor(frac * 100)}%${rateEta(p)}`, "running");
      break;
    }
    case "verifying": {
      const frac = p.total ? p.done / p.total : 0;
      setProgress(frac);
      if (!cancelling) setStatus(`Verifying ${Math.floor(frac * 100)}%${rateEta(p)}`, "running");
      break;
    }
    case "done":
      finish();
      setStatus("Done. Verified and ejected.", "ok");
      break;
    case "error":
      finish();
      setStatus(p.message, "error", "text-error");
      break;
    case "cancelled":
      finish();
      setStatus("Cancelled", "", "dim");
      break;
  }
}

function finish() {
  setProgress(null);
  setRunning(false);
  refreshDisks();
}

listen<Progress>("flash://progress", (e) => onProgress(e.payload));

flashBtn.addEventListener("click", async () => {
  if (running) {
    cancelling = true;
    updateFlashButton();
    setStatus("Cancelling…", "running");
    try {
      await invoke("cancel_flash");
    } catch (e) {
      setStatus(`Cancel failed: ${e}`, "error", "text-error");
    }
    return;
  }
  const d = selectedDisk();
  if (!image || !d || !canFlash()) return;
  setRunning(true);
  setProgress(null);
  setStatus(`Waiting for administrator password to write to ${d.device}…`, "running");
  try {
    await invoke("start_flash", { imagePath: image.path, diskId: d.id });
  } catch (e) {
    setRunning(false);
    setStatus(String(e), "error", "text-error");
  }
});

renderImage();
resetStatus();
refreshDisks();
