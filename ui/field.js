// The dotted field of AVP's pages: soft blobs summed, thresholded by the Bayer matrix into dots.
// The landing's hero draws it loud, the app's Activity and home feed quiet; both read it from here.
const BAYER = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5].map(v => (v + .5) / 16);

function dither(g, W, H, S, rgb, alpha, value) {
  g.fillStyle = `rgba(${rgb},${alpha})`;
  for (let y = 0, j = 0; y < H; y += S, j++) for (let x = 0, i = 0; x < W; x += S, i++) {
    if (value(x, y) > BAYER[(j & 3) * 4 + (i & 3)]) g.fillRect(x, y, S * .38, S * .38);
  }
}
const blob = (x, y, bx, by, r) => { const dx = x - bx, dy = y - by; return Math.exp(-(dx * dx + dy * dy) / (r * r)); };

// Clouds: each [x, y, radius, speed] (fractions of the box) drifts slowly around where it sits,
// and their sum, lifted by `gain` and lowered by `floor`, is what the dots threshold.
function clouds(g, W, H, t, spec, { S = 5, rgb = "237,237,237", alpha = .75, gain = 1.25, floor = .3 } = {}) {
  const A = Math.max(W, H);
  const B = spec.map(([x, y, r, s], k) => [(x + Math.sin(t * .06 * s + k) * .04) * W, (y + Math.cos(t * .05 * s + k * 2) * .03) * H, r * A]);
  dither(g, W, H, S, rgb, alpha, (x, y) => { let v = 0; for (const [bx, by, r] of B) v += blob(x, y, bx, by, r); return Math.min(1, v) * gain - floor; });
}
