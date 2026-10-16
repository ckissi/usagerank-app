// Draws the menu bar template icons and the app icon source as PNGs, with no dependencies.
// The glyph is the design's three rising bars: 14×14 viewBox, 2px round-capped strokes.
//
//   node scripts/make-icons.mjs && npx tauri icon src-tauri/icons/app-source.png
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

const OUT = path.join(path.dirname(new URL(import.meta.url).pathname), '..', 'src-tauri', 'icons');

const BARS = [
  [2.5, 12, 2.5, 9],
  [7, 12, 7, 5.5],
  [11.5, 12, 11.5, 2],
];

const crcTable = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc32 = (buf) => {
  let c = 0xffffffff;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};

function png(size, pixels) {
  const chunk = (type, data) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const body = Buffer.concat([Buffer.from(type), data]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(body));
    return Buffer.concat([len, body, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    pixels.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const distToSegment = (px, py, [ax, ay, bx, by]) => {
  const dx = bx - ax;
  const dy = by - ay;
  const t = Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
};

const inRoundedSquare = (x, y, size, inset, radius) => {
  const lo = inset + radius;
  const hi = size - inset - radius;
  const cx = Math.max(lo, Math.min(hi, x));
  const cy = Math.max(lo, Math.min(hi, y));
  return x >= inset && x <= size - inset && y >= inset && y <= size - inset && Math.hypot(x - cx, y - cy) <= radius;
};

// paint(x, y) returns [r, g, b, a] (0–255) for a point, or null; 4×4 supersampling.
function render(size, paint) {
  const px = Buffer.alloc(size * size * 4);
  const S = 4;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let sy = 0; sy < S; sy++) {
        for (let sx = 0; sx < S; sx++) {
          const c = paint(x + (sx + 0.5) / S, y + (sy + 0.5) / S);
          if (!c) continue;
          const ca = c[3] / 255;
          r += c[0] * ca; g += c[1] * ca; b += c[2] * ca; a += ca;
        }
      }
      const i = (y * size + x) * 4;
      const n = S * S;
      px[i + 3] = Math.round((a / n) * 255);
      if (a > 0) {
        px[i] = Math.round(r / a);
        px[i + 1] = Math.round(g / a);
        px[i + 2] = Math.round(b / a);
      }
    }
  }
  return px;
}

// Menu bar template: black on transparent, macOS tints it. 44px = 22pt @2x.
function trayIcon(withDot) {
  const size = 44;
  const scale = 2.4;
  const offX = withDot ? 9 : 5.2;
  const offY = 5.2;
  return png(size, render(size, (x, y) => {
    const gx = (x - offX) / scale;
    const gy = (y - offY) / scale;
    if (BARS.some((bar) => distToSegment(gx, gy, bar) <= 1)) return [0, 0, 0, 255];
    if (withDot && Math.hypot(x - 4.5, y - 22) <= 3.4) return [0, 0, 0, 255];
    return null;
  }));
}

// App icon source: graphite squircle, ivory bars, the tallest in the brand orange.
function appIcon() {
  const size = 1024;
  const inset = 100;
  const scale = 46;
  const off = (size - 14 * scale) / 2;
  const ground = [32, 33, 36, 255];
  const ivory = [245, 244, 240, 255];
  const orange = [222, 104, 48, 255];
  return png(size, render(size, (x, y) => {
    if (!inRoundedSquare(x, y, size, inset, 185)) return null;
    const gx = (x - off) / scale;
    const gy = (y - off - 0.3 * scale) / scale;
    if (distToSegment(gx, gy, BARS[2]) <= 1.05) return orange;
    if (BARS.slice(0, 2).some((bar) => distToSegment(gx, gy, bar) <= 1.05)) return ivory;
    return ground;
  }));
}

fs.mkdirSync(OUT, { recursive: true });
fs.writeFileSync(path.join(OUT, 'tray.png'), trayIcon(false));
fs.writeFileSync(path.join(OUT, 'tray-syncing.png'), trayIcon(true));
fs.writeFileSync(path.join(OUT, 'app-source.png'), appIcon());
console.log(`wrote tray.png, tray-syncing.png, app-source.png to ${OUT}`);
