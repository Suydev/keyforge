/* charts.js — tiny canvas charting, no dependencies.
 *
 * WHY HAND-ROLLED: Chart.js is ~200KB and animates on every data change. This is
 * ~6KB, draws only when data actually changes, and never runs an animation loop.
 * On a Pad Go that difference is the whole battery budget.
 *
 * DESIGN RULES APPLIED
 *  - trend data -> line chart (skill: chart-type)
 *  - series distinguished by line STYLE as well as colour (skill: color-guidance,
 *    pattern-texture) so it is readable without colour perception
 *  - grid lines low-contrast so they never compete with data (gridline-subtle)
 *  - tap/hover tooltip with exact values (tooltip-on-interact)
 *  - empty state instead of a bare axis frame (empty-data-state)
 *  - no entrance animation; data is readable immediately (animation-optional)
 */
'use strict';

/**
 * Read a design token off :root.
 *
 * Cached, because `getComputedStyle` forces style resolution and the draw loop
 * asks for the same five tokens on every frame. The cache is cleared by
 * `themeChanged()` whenever the theme/density attributes change, so charts
 * always follow the active mode without paying for a lookup per draw.
 */
const _varCache = new Map();
const CSSVAR = (name, fallback) => {
  if (_varCache.has(name)) return _varCache.get(name);
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const out = v || fallback;
  _varCache.set(name, out);
  return out;
};

/** Invalidate the token cache — call after switching theme. */
export function themeChanged() {
  _varCache.clear();
}

/**
 * `#rrggbb` + alpha -> `rgba()`.
 *
 * Canvas gradients need a real colour with an alpha channel; `globalAlpha`
 * cannot express "opaque at the top, transparent at the bottom". Non-hex input
 * (a CSS variable that resolved to `rgb(...)`, say) is returned unchanged so a
 * theme using another notation degrades to a flat fill instead of drawing
 * nothing.
 */
export function hexA(hex, a) {
  const h = String(hex).trim();
  if (h[0] !== '#' || (h.length !== 7 && h.length !== 4)) return h;
  const full = h.length === 4 ? '#' + [...h.slice(1)].map((c) => c + c).join('') : h;
  const n = parseInt(full.slice(1), 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}

/** Format bytes with a sensible unit. */
export function fmtBytes(n) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), u.length - 1);
  const v = n / Math.pow(1024, i);
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}

/** Locale-aware integer (skill: number-formatting). */
export function fmtInt(n) {
  return (n ?? 0).toLocaleString();
}

/** Money with enough precision to be honest about sub-cent costs. */
export function fmtMoney(n) {
  const v = Number(n ?? 0);
  if (v === 0) return '$0.00';
  if (v < 0.01) return '$' + v.toFixed(5);
  if (v < 1000) return '$' + v.toFixed(2);
  return '$' + Math.round(v).toLocaleString();
}

export function fmtDuration(secs) {
  const s = Math.max(0, Math.floor(secs || 0));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

export function fmtClock(unixSecs) {
  return new Date(unixSecs * 1000).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
  });
}

export function fmtAgo(unixSecs) {
  const d = Math.max(0, Math.floor(Date.now() / 1000 - unixSecs));
  if (d < 10) return 'just now';
  if (d < 60) return `${d}s ago`;
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  return `${Math.floor(d / 86400)}d ago`;
}

/**
 * A multi-series line chart bound to one canvas.
 *
 * Series get an explicit `dash` pattern so they remain distinguishable in
 * greyscale or with colour-vision deficiency.
 */
/**
 * Quality tiers.
 *
 * The dashboard runs on a tablet, so "make it pretty" and "keep the battery"
 * are in direct tension. Rather than guess, the cost is exposed as a setting:
 *
 *   high     — gradient fill, glow pass, monotone-spline curves, all markers
 *   balanced — gradient fill, spline curves, no glow  (default)
 *   low      — flat fill, straight segments, no glow, fewer markers
 *
 * Every tier draws the SAME data with the same scales. The tier only removes
 * decoration, never information — a cheaper chart must not be a less truthful
 * one.
 */
export const QUALITY = {
  high: { glow: true, gradient: true, curve: true, markers: true, dots: true },
  balanced: { glow: false, gradient: true, curve: true, markers: true, dots: true },
  low: { glow: false, gradient: false, curve: false, markers: false, dots: false },
};

/** X-axis label formatters, chosen by bucket width rather than hardcoded. */
export const X_FMT = {
  // Sub-hour buckets: clock time is the only useful label.
  minute: (t) => fmtClock(t),
  // Hour buckets: the minutes are always :00, so show the hour and the day.
  hour: (t) => {
    const d = new Date(t * 1000);
    return `${String(d.getHours()).padStart(2, '0')}:00`;
  },
  // Day buckets: month + day, locale-aware.
  day: (t) =>
    new Date(t * 1000).toLocaleDateString([], { month: 'short', day: 'numeric' }),
};

export class LineChart {
  /**
   * @param {HTMLCanvasElement} canvas
   * @param {{key:string,label:string,color:string,dash?:number[],axis?:'left'|'right',fill?:boolean,fmt?:(n:number)=>string}[]} series
   */
  constructor(canvas, series, opts = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d');
    this.series = series;
    this.opts = opts;
    this.data = [];
    this.hidden = new Set();
    this.hoverIndex = -1;
    this.dpr = 1;
    this.quality = QUALITY[opts.quality] || QUALITY.balanced;
    this.xFmt = X_FMT[opts.xRes] || X_FMT.minute;
    /** Set while a wider range is being fetched, so we can shimmer rather than blank. */
    this.loading = false;

    // Resize is debounced via rAF so a drag-resize cannot thrash layout.
    this._pendingResize = false;
    this._ro = new ResizeObserver(() => {
      if (this._pendingResize) return;
      this._pendingResize = true;
      requestAnimationFrame(() => {
        this._pendingResize = false;
        this.resize();
      });
    });
    this._ro.observe(canvas);

    // Pointer events cover mouse AND touch (skill: hover-vs-tap).
    canvas.addEventListener('pointermove', (e) => this._onPointer(e), { passive: true });
    canvas.addEventListener('pointerdown', (e) => this._onPointer(e), { passive: true });
    canvas.addEventListener('pointerleave', () => {
      this.hoverIndex = -1;
      this.draw();
    }, { passive: true });

    // Keyboard access to values (skill: tooltip-keyboard, focusable-elements).
    canvas.tabIndex = 0;
    canvas.addEventListener('keydown', (e) => {
      if (!this.data.length) return;
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
        e.preventDefault();
        const dir = e.key === 'ArrowRight' ? 1 : -1;
        const start = this.hoverIndex < 0 ? this.data.length - 1 : this.hoverIndex;
        this.hoverIndex = Math.max(0, Math.min(this.data.length - 1, start + dir));
        this.draw();
        this._announce();
      } else if (e.key === 'Home') {
        e.preventDefault();
        this.hoverIndex = 0;
        this.draw();
        this._announce();
      } else if (e.key === 'End') {
        e.preventDefault();
        this.hoverIndex = this.data.length - 1;
        this.draw();
        this._announce();
      } else if (e.key === 'Escape') {
        this.hoverIndex = -1;
        this.draw();
      }
    });

    this.resize();
  }

  /** Switch quality tier at runtime (the Appearance panel calls this). */
  setQuality(name) {
    this.quality = QUALITY[name] || QUALITY.balanced;
    this.draw();
  }

  /** Switch x-axis formatting when the range resolution changes. */
  setXRes(name) {
    this.xFmt = X_FMT[name] || X_FMT.minute;
    this.draw();
  }

  setLoading(v) {
    this.loading = !!v;
    this.draw();
  }

  /** Live region text so a screen reader can read the focused point. */
  _announce() {
    if (!this.opts.liveRegion || this.hoverIndex < 0) return;
    const d = this.data[this.hoverIndex];
    if (!d) return;
    const parts = this.series
      .filter((s) => !this.hidden.has(s.key))
      .map((s) => `${s.label} ${(s.fmt || fmtInt)(d[s.key] ?? 0)}`);
    this.opts.liveRegion.textContent = `${this.xFmt(d.t)}: ${parts.join(', ')}`;
  }

  /**
   * Screen-reader summary of the whole series (skill: screen-reader-summary).
   * A canvas is opaque to assistive tech, so the shape has to be described.
   */
  summary() {
    if (this.data.length < 2) return 'No chart data yet.';
    const parts = [];
    for (const s of this.series) {
      if (this.hidden.has(s.key)) continue;
      const vals = this.data.map((d) => Number(d[s.key] ?? 0));
      const f = s.fmt || fmtInt;
      const max = Math.max(...vals);
      const last = vals[vals.length - 1];
      const total = vals.reduce((a, b) => a + b, 0);
      parts.push(`${s.label}: now ${f(last)}, peak ${f(max)}, total ${f(total)}`);
    }
    return `${this.data.length} points from ${this.xFmt(this.data[0].t)} to ${this.xFmt(
      this.data[this.data.length - 1].t,
    )}. ${parts.join('. ')}.`;
  }

  _onPointer(e) {
    if (!this.data.length) return;
    const rect = this.canvas.getBoundingClientRect();
    const x = e.clientX - rect.left;
    const { padL, padR } = this._pad();
    const w = rect.width - padL - padR;
    if (w <= 0) return;
    const frac = Math.max(0, Math.min(1, (x - padL) / w));
    const idx = Math.round(frac * (this.data.length - 1));
    if (idx !== this.hoverIndex) {
      this.hoverIndex = idx;
      this.draw();
      this._announce();
    }
  }

  _pad() {
    const twoAxis = this.series.some((s) => s.axis === 'right');
    return { padL: 46, padR: twoAxis ? 46 : 14, padT: 12, padB: 22 };
  }

  resize() {
    const rect = this.canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    // Cap DPR at 2: beyond that the pixel cost is real and the gain is not.
    this.dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.canvas.width = Math.round(rect.width * this.dpr);
    this.canvas.height = Math.round(rect.height * this.dpr);
    this.draw();
  }

  setData(rows) {
    this.data = Array.isArray(rows) ? rows : [];
    // A stale hover index would point past the end of a shorter series and
    // read the wrong value into the tooltip.
    if (this.hoverIndex >= this.data.length) this.hoverIndex = -1;
    this.loading = false;
    this.draw();
  }

  toggle(key) {
    if (this.hidden.has(key)) this.hidden.delete(key);
    else this.hidden.add(key);
    this.draw();
  }

  isHidden(key) {
    return this.hidden.has(key);
  }

  /**
   * Monotone cubic tangents (Fritsch–Carlson).
   *
   * Plain bezier smoothing overshoots: between two points it can dip below the
   * lower one, drawing negative requests or a latency spike that never
   * happened. That is a chart lying about data, which is worse than a chart
   * with corners. This variant is provably monotone between samples — the
   * curve never leaves the interval its endpoints define.
   */
  _tangents(ys, dx) {
    const n = ys.length;
    if (n < 2) return [0];
    const d = new Array(n - 1);
    for (let i = 0; i < n - 1; i++) d[i] = (ys[i + 1] - ys[i]) / dx;
    const m = new Array(n);
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for (let i = 1; i < n - 1; i++) {
      // Sign change (a local extremum) => flat tangent, so no overshoot.
      m[i] = d[i - 1] * d[i] <= 0 ? 0 : (d[i - 1] + d[i]) / 2;
    }
    for (let i = 0; i < n - 1; i++) {
      if (d[i] === 0) {
        m[i] = 0;
        m[i + 1] = 0;
        continue;
      }
      const a = m[i] / d[i];
      const b = m[i + 1] / d[i];
      const h = Math.hypot(a, b);
      if (h > 3) {
        m[i] = ((3 * a) / h) * d[i];
        m[i + 1] = ((3 * b) / h) * d[i];
      }
    }
    return m;
  }

  /** Trace a series path (curved or straight) without stroking or filling it. */
  _tracePath(ctx, pts, curve) {
    if (!pts.length) return;
    ctx.moveTo(pts[0].x, pts[0].y);
    if (!curve || pts.length < 3) {
      for (let i = 1; i < pts.length; i++) ctx.lineTo(pts[i].x, pts[i].y);
      return;
    }
    const dx = pts[1].x - pts[0].x;
    const m = this._tangents(pts.map((p) => p.y), dx);
    for (let i = 0; i < pts.length - 1; i++) {
      const p0 = pts[i];
      const p1 = pts[i + 1];
      ctx.bezierCurveTo(
        p0.x + dx / 3,
        p0.y + (m[i] * dx) / 3,
        p1.x - dx / 3,
        p1.y - (m[i + 1] * dx) / 3,
        p1.x,
        p1.y,
      );
    }
  }

  draw() {
    const ctx = this.ctx;
    const rect = this.canvas.getBoundingClientRect();
    const W = rect.width;
    const H = rect.height;
    if (!W || !H) return;

    ctx.save();
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);

    const fgFaint = CSSVAR('--fg-faint', '#6b7a99');
    const border = CSSVAR('--border', '#2c3a5a');
    const fg = CSSVAR('--fg', '#f8fafc');
    const surface2 = CSSVAR('--surface-2', '#202c4a');
    const bg = CSSVAR('--bg', '#0f172a');

    // Empty / loading state: say so in words rather than drawing an axis frame
    // around nothing (skill: empty-data-state, loading-chart).
    if (this.data.length < 2) {
      ctx.fillStyle = fgFaint;
      ctx.font = '13px ' + CSSVAR('--sans', 'Inter, sans-serif');
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(
        this.loading ? 'Loading history…' : 'No traffic in this range yet',
        W / 2,
        H / 2,
      );
      ctx.restore();
      return;
    }

    const { padL, padR, padT, padB } = this._pad();
    const plotW = W - padL - padR;
    const plotH = H - padT - padB;

    const visible = this.series.filter((s) => !this.hidden.has(s.key));
    const leftSeries = visible.filter((s) => s.axis !== 'right');
    const rightSeries = visible.filter((s) => s.axis === 'right');

    const maxOf = (list) => {
      let m = 0;
      for (const s of list) {
        for (const d of this.data) {
          const v = Number(d[s.key] ?? 0);
          if (v > m) m = v;
        }
      }
      return m;
    };
    // Nice-ish ceiling so the axis labels are readable numbers.
    const ceil = (m) => {
      if (m <= 0) return 1;
      const mag = Math.pow(10, Math.floor(Math.log10(m)));
      return Math.ceil(m / mag) * mag;
    };
    const maxL = ceil(maxOf(leftSeries));
    const maxR = ceil(maxOf(rightSeries));

    const xAt = (i) => padL + (plotW * i) / (this.data.length - 1);
    const yAt = (v, right) => {
      const max = right ? maxR : maxL;
      return padT + plotH - (plotH * Math.min(Number(v ?? 0), max)) / (max || 1);
    };

    // ── grid + axis labels ────────────────────────────────────────────────
    ctx.strokeStyle = border;
    ctx.lineWidth = 1;
    ctx.fillStyle = fgFaint;
    ctx.font = '10px ' + CSSVAR('--mono', 'monospace');
    ctx.textBaseline = 'middle';

    const ROWS = 4;
    for (let r = 0; r <= ROWS; r++) {
      const y = padT + (plotH * r) / ROWS;
      ctx.globalAlpha = 0.5;
      ctx.beginPath();
      ctx.moveTo(padL, y + 0.5);
      ctx.lineTo(padL + plotW, y + 0.5);
      ctx.stroke();
      ctx.globalAlpha = 1;

      const vL = maxL * (1 - r / ROWS);
      ctx.textAlign = 'right';
      ctx.fillText(this._axisLabel(vL, leftSeries), padL - 6, y);

      if (rightSeries.length) {
        const vR = maxR * (1 - r / ROWS);
        ctx.textAlign = 'left';
        ctx.fillText(this._axisLabel(vR, rightSeries), padL + plotW + 6, y);
      }
    }

    // X labels: auto-skip so ticks never crowd on a narrow screen.
    const targetTicks = Math.max(2, Math.min(6, Math.floor(plotW / 76)));
    const step = Math.max(1, Math.floor((this.data.length - 1) / targetTicks));
    ctx.textAlign = 'center';
    ctx.textBaseline = 'top';
    for (let i = 0; i < this.data.length; i += step) {
      ctx.fillText(this.xFmt(this.data[i].t), xAt(i), padT + plotH + 6);
    }

    // ── series ────────────────────────────────────────────────────────────
    const curve = this.quality.curve;
    for (const s of visible) {
      const right = s.axis === 'right';
      const pts = this.data.map((d, i) => ({ x: xAt(i), y: yAt(d[s.key], right) }));

      // Area fill first, so the stroke sits on top of its own gradient.
      if (s.fill) {
        ctx.beginPath();
        this._tracePath(ctx, pts, curve);
        ctx.lineTo(pts[pts.length - 1].x, padT + plotH);
        ctx.lineTo(pts[0].x, padT + plotH);
        ctx.closePath();
        if (this.quality.gradient) {
          // Vertical fade: dense at the line, transparent at the baseline, so
          // overlapping series stay readable instead of muddying each other.
          const grad = ctx.createLinearGradient(0, padT, 0, padT + plotH);
          grad.addColorStop(0, hexA(s.color, 0.34));
          grad.addColorStop(0.6, hexA(s.color, 0.1));
          grad.addColorStop(1, hexA(s.color, 0));
          ctx.fillStyle = grad;
          ctx.fill();
        } else {
          ctx.globalAlpha = 0.12;
          ctx.fillStyle = s.color;
          ctx.fill();
          ctx.globalAlpha = 1;
        }
      }

      // Glow: one extra wide, low-alpha stroke under the real one. Cheap, and
      // only on the `high` tier because it is pure decoration.
      if (this.quality.glow) {
        ctx.beginPath();
        this._tracePath(ctx, pts, curve);
        ctx.setLineDash([]);
        ctx.strokeStyle = hexA(s.color, 0.22);
        ctx.lineWidth = 7;
        ctx.lineJoin = 'round';
        ctx.lineCap = 'round';
        ctx.stroke();
      }

      ctx.beginPath();
      this._tracePath(ctx, pts, curve);
      ctx.setLineDash(s.dash || []);
      ctx.strokeStyle = s.color;
      ctx.lineWidth = 2;
      ctx.lineJoin = 'round';
      ctx.lineCap = 'round';
      ctx.stroke();
      ctx.setLineDash([]);

      // Peak + current markers. Direct labelling beats eye-travel to an axis
      // (skill: direct-labeling), and the ring on the last point is what makes
      // the chart read as live without any animation.
      if (this.quality.markers && !right) {
        const vals = this.data.map((d) => Number(d[s.key] ?? 0));
        const maxV = Math.max(...vals);
        const maxI = vals.indexOf(maxV);
        const f = s.fmt || fmtInt;

        if (maxV > 0) {
          const mx = xAt(maxI);
          const my = yAt(maxV, false);
          ctx.beginPath();
          ctx.arc(mx, my, 2.5, 0, Math.PI * 2);
          ctx.fillStyle = s.color;
          ctx.fill();
          // Label flips below the point near the top edge so it never clips.
          const above = my > padT + 16;
          ctx.font = '10px ' + CSSVAR('--mono', 'monospace');
          ctx.textAlign = mx > padL + plotW - 40 ? 'right' : 'left';
          ctx.textBaseline = above ? 'bottom' : 'top';
          ctx.fillStyle = hexA(s.color, 0.95);
          ctx.fillText(`peak ${f(maxV)}`, mx + (ctx.textAlign === 'right' ? -4 : 4), above ? my - 5 : my + 5);
        }

        const lastI = this.data.length - 1;
        const lx = xAt(lastI);
        const ly = yAt(vals[lastI], false);
        ctx.beginPath();
        ctx.arc(lx, ly, 5.5, 0, Math.PI * 2);
        ctx.fillStyle = hexA(s.color, 0.2);
        ctx.fill();
        ctx.beginPath();
        ctx.arc(lx, ly, 3, 0, Math.PI * 2);
        ctx.fillStyle = s.color;
        ctx.fill();
        ctx.strokeStyle = bg;
        ctx.lineWidth = 1.5;
        ctx.stroke();
      }
    }

    // ── crosshair + tooltip ───────────────────────────────────────────────
    if (this.hoverIndex >= 0 && this.hoverIndex < this.data.length) {
      const i = this.hoverIndex;
      const x = xAt(i);
      const d = this.data[i];

      ctx.strokeStyle = fgFaint;
      ctx.globalAlpha = 0.55;
      ctx.setLineDash([3, 3]);
      ctx.beginPath();
      ctx.moveTo(x + 0.5, padT);
      ctx.lineTo(x + 0.5, padT + plotH);
      ctx.stroke();

      // Horizontal arm on the FIRST visible left-axis series only. Drawing one
      // per series would be a cage of dotted lines.
      const primary = leftSeries[0];
      if (primary) {
        const hy = yAt(d[primary.key], false);
        ctx.beginPath();
        ctx.moveTo(padL, hy + 0.5);
        ctx.lineTo(padL + plotW, hy + 0.5);
        ctx.stroke();
      }
      ctx.setLineDash([]);
      ctx.globalAlpha = 1;

      if (this.quality.dots) {
        for (const s of visible) {
          const y = yAt(d[s.key], s.axis === 'right');
          ctx.beginPath();
          ctx.arc(x, y, 3.5, 0, Math.PI * 2);
          ctx.fillStyle = s.color;
          ctx.fill();
          ctx.strokeStyle = bg;
          ctx.lineWidth = 1.5;
          ctx.stroke();
        }
      }

      const lines = [this.xFmt(d.t)];
      for (const s of visible) lines.push(`${s.label}: ${(s.fmt || fmtInt)(d[s.key] ?? 0)}`);

      ctx.font = '11px ' + CSSVAR('--mono', 'monospace');
      const tw = Math.max(...lines.map((l) => ctx.measureText(l).width)) + 16;
      const th = lines.length * 15 + 10;
      let tx = x + 10;
      if (tx + tw > W - 4) tx = x - tw - 10;
      tx = Math.max(4, tx);
      const ty = Math.max(4, padT + 4);

      ctx.fillStyle = surface2;
      ctx.strokeStyle = border;
      ctx.lineWidth = 1;
      if (ctx.roundRect) {
        ctx.beginPath();
        ctx.roundRect(tx, ty, tw, th, 8);
        ctx.fill();
        ctx.stroke();
      } else {
        ctx.fillRect(tx, ty, tw, th);
        ctx.strokeRect(tx, ty, tw, th);
      }

      ctx.textAlign = 'left';
      ctx.textBaseline = 'top';
      lines.forEach((l, k) => {
        ctx.fillStyle = k === 0 ? fgFaint : fg;
        ctx.fillText(l, tx + 8, ty + 6 + k * 15);
      });
    }

    ctx.restore();
  }

  _axisLabel(v, list) {
    const f = list.find((s) => s.fmt)?.fmt;
    if (f) return f(v);
    return v >= 1000 ? Math.round(v / 1000) + 'k' : String(Math.round(v));
  }

  destroy() {
    this._ro.disconnect();
  }
}

/**
 * One-shot alert tone via WebAudio.
 *
 * No audio file to ship, and nothing is allocated until the user actually
 * enables alerts — so an idle page holds no audio context.
 */
export class Chime {
  constructor() {
    this.ctx = null;
    this.enabled = false;
  }

  /** Must be called from a user gesture (browser autoplay policy). */
  enable() {
    if (!this.ctx) {
      const AC = window.AudioContext || window.webkitAudioContext;
      if (!AC) return false;
      this.ctx = new AC();
    }
    if (this.ctx.state === 'suspended') this.ctx.resume();
    this.enabled = true;
    return true;
  }

  disable() {
    this.enabled = false;
  }

  /** Two-note chime; `bad` drops the interval so errors sound different. */
  play(bad = false) {
    if (!this.enabled || !this.ctx) return;
    const t0 = this.ctx.currentTime;
    const notes = bad ? [440, 330] : [660, 880];
    notes.forEach((f, i) => {
      const osc = this.ctx.createOscillator();
      const gain = this.ctx.createGain();
      osc.type = 'sine';
      osc.frequency.value = f;
      // Short envelope, low peak — a notification, not an alarm.
      gain.gain.setValueAtTime(0.0001, t0 + i * 0.14);
      gain.gain.exponentialRampToValueAtTime(0.16, t0 + i * 0.14 + 0.015);
      gain.gain.exponentialRampToValueAtTime(0.0001, t0 + i * 0.14 + 0.13);
      osc.connect(gain).connect(this.ctx.destination);
      osc.start(t0 + i * 0.14);
      osc.stop(t0 + i * 0.14 + 0.15);
    });
  }
}
