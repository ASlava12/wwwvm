// Curated demo scenarios for the showcase page. Each preset is a small config
// that pre-fills the Boot-Linux controls (and produces a shareable link via
// demo-link.js). Presets reference a server image by a *substring hint*
// (`imageMatch`) rather than a hard id, so they survive whatever this server
// actually built (alpine-3.21, alpine-gui, …) and degrade gracefully when no
// matching image exists.
//
// Pure data + two pure helpers (no DOM) so they're node-testable like the rest
// of web/*.js — see demo-presets.test.mjs. main.js owns the dropdown glue.

export const DEMO_PRESETS = [
  {
    id: "alpine-shell",
    label: "Alpine shell",
    // Works offline (no relay): boot Alpine to its musl busybox userspace and
    // print system info. The safe default — reliable on any alpine image.
    note: "Boots Alpine and prints uname + os-release. No network needed.",
    imageMatch: "alpine",
    net: false,
    fb: false,
    autorun: "uname -a\ncat /etc/os-release",
  },
  {
    id: "graphics-fb",
    label: "Graphics (framebuffer → canvas)",
    note: "Renders the kernel console as RGB pixels on the canvas (efifb). No network.",
    imageMatch: "alpine",
    net: false,
    fb: true,
    fbres: "1024x768",
    autorun: "uname -a",
  },
  {
    id: "python-apk",
    label: "Python (apk add — needs a relay)",
    note: "Installs CPython over the net and runs a one-liner. Needs a WebSocket↔TCP relay.",
    imageMatch: "alpine",
    net: true,
    allow: "dl-cdn.alpinelinux.org\n*",
    fb: false,
    autorun: "apk add python3\npython3 -c 'print(sum(range(1001)))'",
  },
  {
    id: "numpy",
    label: "Scientific Python: numpy (needs a relay)",
    note: "apk add py3-numpy, then a sum/dot/matmul check. Slow first install; needs a relay.",
    imageMatch: "alpine",
    net: true,
    allow: "dl-cdn.alpinelinux.org\n*",
    fb: false,
    autorun:
      "apk add py3-numpy\npython3 -c 'import numpy as np; a=np.arange(1000); print(a.sum(), a@a)'",
  },
];

// Pick the best server-image id for a preset's substring hint. Matches the id
// first, then the display name; returns "" when nothing matches (the caller
// then leaves the current selection rather than booting the wrong image).
export function pickImageId(manifest, match) {
  if (!Array.isArray(manifest) || !manifest.length || !match) return "";
  const m = String(match).toLowerCase();
  const hit = manifest.find(
    (x) =>
      String(x.id || "").toLowerCase().includes(m) ||
      String(x.name || "").toLowerCase().includes(m),
  );
  return hit ? hit.id : "";
}

// Turn a preset (+ the resolved image id) into a demo-link config object that
// buildHashFromConfig / applyDemoLinkFromHash understand. Only the fields the
// preset actually sets are included; `fb`/`net` stay booleans (demo-link emits
// truthy-only). Never sets `boot` — applying a preset fills the controls for
// review; the user clicks Load/Boot.
export function presetToConfig(preset, imageId) {
  const cfg = {};
  if (!preset || typeof preset !== "object") return cfg;
  if (imageId) cfg.img = imageId;
  for (const k of ["cmdline", "ram", "ramdisk", "fbres", "allow", "autorun"]) {
    if (preset[k] !== undefined && preset[k] !== "") cfg[k] = preset[k];
  }
  for (const k of ["fb", "net"]) {
    if (preset[k] !== undefined) cfg[k] = !!preset[k];
  }
  return cfg;
}
