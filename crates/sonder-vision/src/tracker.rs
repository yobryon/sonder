//! Blob detection and tracking on small grayscale frames.
//!
//! Pipeline per frame:
//!
//! 1. Frame statistics (mean, stddev).
//! 2. A binary mask: either luma above an adaptive threshold ([`Mode::Bright`],
//!    headlights at night) or `|frame − background| > threshold`
//!    ([`Mode::Motion`], a running-average background).
//! 3. Connected-component labeling (8-connectivity, union-find, one pixel pass).
//! 4. Blob filtering (`min_blob_px`), merging of close blobs (a headlight pair
//!    becomes one car), `max_blobs` cap.
//! 5. Greedy nearest-neighbour association with the previous frame's tracks:
//!    stable ids, EMA velocity, `entered` after confirmation, `exited` after
//!    `max_missed` unmatched frames, `crossings` of the configured line.
//!
//! All output coordinates are normalized `0..1` (x right, y down). All buffers
//! are reused between frames; `process` allocates only the small output vectors
//! (use [`Tracker::process_into`] to avoid even those).

use sonder_core::Seconds;

/// What a frame is thresholded on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Bright regions on a dark frame (headlights at night).
    #[default]
    Bright,
    /// Pixels that differ from a running-average background (daytime / any cam).
    Motion,
}

impl Mode {
    /// Parse `"bright"` / `"motion"` (case-insensitive).
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bright" | "headlights" | "night" => Some(Mode::Bright),
            "motion" | "diff" | "day" => Some(Mode::Motion),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Bright => "bright",
            Mode::Motion => "motion",
        }
    }
}

/// Tracker tuning. `..Default::default()` is tuned for a 160×120 night freeway.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackerConfig {
    /// Brightness threshold `0..1`. Bright mode: absolute luma level, blended
    /// with an adaptive level (see `adaptive_k`, `adaptive_blend`). Motion mode:
    /// minimum `|frame − background|` as a fraction of full scale (0.15–0.3 is
    /// typical).
    pub threshold: f32,
    /// Minimum component area in pixels (at the processed/downscaled resolution).
    pub min_blob_px: u32,
    /// Keep at most this many blobs per frame (largest first).
    pub max_blobs: usize,
    /// Integer downscale the *caller* applies before `process` (the tracker only
    /// records it; `min_blob_px` and `merge_dist` are in downscaled pixels).
    pub downscale: u32,
    pub mode: Mode,
    /// Normalized y of a crossing line; `None` disables crossing events.
    pub line: Option<f32>,
    /// Motion mode: running-average background update rate per frame (0..1).
    pub background_alpha: f32,
    /// Blobs whose bounding boxes are closer than this (pixels, gap between
    /// boxes) are merged.
    pub merge_dist: f32,
    /// Bright mode: adaptive threshold = mean + k·stddev (in luma units).
    pub adaptive_k: f32,
    /// Bright mode: weight of the adaptive threshold vs the absolute one (0 =
    /// purely absolute, 1 = purely adaptive).
    pub adaptive_blend: f32,
    /// Frames a track may go unmatched before it ends.
    pub max_missed: u32,
    /// Frames a new track must be matched before it is reported (`entered`).
    pub confirm_frames: u32,
    /// EMA factor for the velocity estimate (1 = instantaneous).
    pub velocity_alpha: f32,
    /// Association gate: `gate_base + gate_scale · max(blob diag, track diag)`
    /// in normalized units.
    pub gate_base: f32,
    pub gate_scale: f32,
    /// Smoothing factor for `motion_energy` (1 = raw per-frame value).
    pub energy_alpha: f32,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            threshold: 0.6,
            min_blob_px: 4,
            max_blobs: 64,
            downscale: 4,
            mode: Mode::Bright,
            line: Some(0.5),
            background_alpha: 0.02,
            merge_dist: 6.0,
            adaptive_k: 2.5,
            adaptive_blend: 0.5,
            max_missed: 3,
            confirm_frames: 2,
            velocity_alpha: 0.4,
            gate_base: 0.04,
            gate_scale: 1.5,
            energy_alpha: 0.3,
        }
    }
}

/// A detected / tracked blob. Coordinates and sizes are normalized to the
/// frame (`0..1`, x right, y down); velocities are frame-fractions per second.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Blob {
    /// Stable track id (0 for a raw, untracked detection).
    pub id: u64,
    /// Centroid.
    pub x: f32,
    pub y: f32,
    /// Bounding box size.
    pub w: f32,
    pub h: f32,
    /// Area as a fraction of the frame.
    pub area: f32,
    /// Mean luma of the blob's pixels, `0..1`.
    pub brightness: f32,
    /// Velocity estimate (EMA) in frame-fractions per second.
    pub vx: f32,
    pub vy: f32,
    /// Speed: `|v|` for live blobs; for `exited` blobs the mean speed
    /// `distance / age_s`.
    pub speed: f32,
    /// Seconds since the track was first seen.
    pub age_s: f32,
    /// Total path length travelled (frame fractions).
    pub distance: f32,
    /// Direction of travel: `+1` moving down (y increasing), `-1` moving up,
    /// `0` none. For `crossings` this is the direction the line was crossed.
    pub dir: f32,
}

/// Result of [`Tracker::process`] for one frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameAnalysis {
    /// Timestamp passed to `process`.
    pub t: Seconds,
    /// Processed frame size in pixels.
    pub width: u32,
    pub height: u32,
    /// Smoothed fraction of the frame that is "active" (changed pixels in
    /// Motion mode, bright pixels in Bright mode), `0..1`.
    pub motion_energy: f32,
    /// Mean luma of the whole frame, `0..1`.
    pub mean_brightness: f32,
    /// Number of components after merging, before the `max_blobs` cap.
    pub raw_blob_count: usize,
    /// Confirmed tracks seen in this frame.
    pub blobs: Vec<Blob>,
    /// Tracks confirmed this frame.
    pub entered: Vec<Blob>,
    /// Tracks that ended this frame (last known position, mean speed, duration).
    pub exited: Vec<Blob>,
    /// Confirmed tracks whose centroid crossed `line` since the previous frame.
    pub crossings: Vec<Blob>,
}

impl FrameAnalysis {
    fn clear(&mut self) {
        self.blobs.clear();
        self.entered.clear();
        self.exited.clear();
        self.crossings.clear();
        self.raw_blob_count = 0;
    }
}

/// Per-component accumulator used during labeling.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    n: u32,
    sx: u64,
    sy: u64,
    sb: u64,
    minx: u32,
    miny: u32,
    maxx: u32,
    maxy: u32,
}

impl Acc {
    #[inline]
    fn new(x: u32, y: u32, b: u8) -> Self {
        Self { n: 1, sx: x as u64, sy: y as u64, sb: b as u64, minx: x, miny: y, maxx: x, maxy: y }
    }
    #[inline]
    fn add(&mut self, x: u32, y: u32, b: u8) {
        self.n += 1;
        self.sx += x as u64;
        self.sy += y as u64;
        self.sb += b as u64;
        self.minx = self.minx.min(x);
        self.maxx = self.maxx.max(x);
        self.miny = self.miny.min(y);
        self.maxy = self.maxy.max(y);
    }
    #[inline]
    fn merge(&mut self, o: &Acc) {
        self.n += o.n;
        self.sx += o.sx;
        self.sy += o.sy;
        self.sb += o.sb;
        self.minx = self.minx.min(o.minx);
        self.maxx = self.maxx.max(o.maxx);
        self.miny = self.miny.min(o.miny);
        self.maxy = self.maxy.max(o.maxy);
    }
}

/// A raw detection in pixel units (before tracking).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Det {
    cx: f32,
    cy: f32,
    minx: f32,
    miny: f32,
    maxx: f32,
    maxy: f32,
    area: f32,
    /// mean luma 0..255
    bright: f32,
}

impl Det {
    fn from_acc(a: &Acc) -> Self {
        let n = a.n as f32;
        Det {
            cx: a.sx as f32 / n,
            cy: a.sy as f32 / n,
            minx: a.minx as f32,
            miny: a.miny as f32,
            maxx: a.maxx as f32 + 1.0,
            maxy: a.maxy as f32 + 1.0,
            area: n,
            bright: a.sb as f32 / n,
        }
    }
    /// Gap between bounding boxes (0 if they overlap).
    fn gap(&self, o: &Det) -> f32 {
        let dx = (self.minx - o.maxx).max(o.minx - self.maxx).max(0.0);
        let dy = (self.miny - o.maxy).max(o.miny - self.maxy).max(0.0);
        (dx * dx + dy * dy).sqrt()
    }
    fn absorb(&mut self, o: &Det) {
        let total = self.area + o.area;
        self.cx = (self.cx * self.area + o.cx * o.area) / total;
        self.cy = (self.cy * self.area + o.cy * o.area) / total;
        self.bright = (self.bright * self.area + o.bright * o.area) / total;
        self.minx = self.minx.min(o.minx);
        self.miny = self.miny.min(o.miny);
        self.maxx = self.maxx.max(o.maxx);
        self.maxy = self.maxy.max(o.maxy);
        self.area = total;
    }
}

#[derive(Debug, Clone, Copy)]
struct Track {
    id: u64,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    area: f32,
    brightness: f32,
    vx: f32,
    vy: f32,
    start_t: Seconds,
    last_t: Seconds,
    start_y: f32,
    distance: f32,
    hits: u32,
    missed: u32,
    confirmed: bool,
    /// Set during association; cleared each frame.
    matched: bool,
}

impl Track {
    fn to_blob(&self, now: Seconds) -> Blob {
        let age = (now - self.start_t).max(0.0) as f32;
        let dy = self.y - self.start_y;
        Blob {
            id: self.id,
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
            area: self.area,
            brightness: self.brightness,
            vx: self.vx,
            vy: self.vy,
            speed: (self.vx * self.vx + self.vy * self.vy).sqrt(),
            age_s: age,
            distance: self.distance,
            dir: if dy > 1e-4 { 1.0 } else if dy < -1e-4 { -1.0 } else { 0.0 },
        }
    }
    fn to_exit_blob(&self) -> Blob {
        let mut b = self.to_blob(self.last_t);
        b.speed = if b.age_s > 1e-3 { self.distance / b.age_s } else { b.speed };
        b
    }
}

/// Blob detector + tracker. See the module docs.
#[derive(Debug)]
pub struct Tracker {
    cfg: TrackerConfig,
    width: u32,
    height: u32,
    /// Motion mode background, 8.8 fixed point.
    bg: Vec<u16>,
    bg_valid: bool,
    /// Per-pixel provisional label (0 = background), reused.
    labels: Vec<u32>,
    parent: Vec<u32>,
    accs: Vec<Acc>,
    dets: Vec<Det>,
    merged: Vec<Det>,
    tracks: Vec<Track>,
    pairs: Vec<(f32, u32, u32)>,
    det_used: Vec<bool>,
    next_id: u64,
    energy: f32,
    last_t: Option<Seconds>,
    frames: u64,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self {
            cfg,
            width: 0,
            height: 0,
            bg: Vec::new(),
            bg_valid: false,
            labels: Vec::new(),
            parent: Vec::new(),
            accs: Vec::new(),
            dets: Vec::new(),
            merged: Vec::new(),
            tracks: Vec::new(),
            pairs: Vec::new(),
            det_used: Vec::new(),
            next_id: 1,
            energy: 0.0,
            last_t: None,
            frames: 0,
        }
    }

    pub fn config(&self) -> &TrackerConfig {
        &self.cfg
    }

    /// Replace the configuration. Tracks survive; the background is reset if
    /// the mode changed.
    pub fn set_config(&mut self, cfg: TrackerConfig) {
        if cfg.mode != self.cfg.mode {
            self.bg_valid = false;
        }
        self.cfg = cfg;
    }

    /// Forget all tracks and the background model.
    pub fn reset(&mut self) {
        self.tracks.clear();
        self.bg_valid = false;
        self.energy = 0.0;
        self.last_t = None;
    }

    /// Frames processed so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Number of live tracks (confirmed or not).
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Process one grayscale frame. `gray` must hold at least `w*h` bytes;
    /// otherwise an empty analysis is returned (never panics).
    pub fn process(&mut self, gray: &[u8], w: u32, h: u32, t: Seconds) -> FrameAnalysis {
        let mut out = FrameAnalysis::default();
        self.process_into(gray, w, h, t, &mut out);
        out
    }

    /// Like [`Tracker::process`] but reuses the caller's output vectors.
    pub fn process_into(&mut self, gray: &[u8], w: u32, h: u32, t: Seconds, out: &mut FrameAnalysis) {
        out.clear();
        out.t = t;
        out.width = w;
        out.height = h;
        let n = (w as usize).saturating_mul(h as usize);
        if w == 0 || h == 0 || gray.len() < n || n > (1 << 28) {
            out.motion_energy = self.energy;
            return;
        }
        let gray = &gray[..n];
        if w != self.width || h != self.height {
            self.width = w;
            self.height = h;
            self.bg_valid = false;
            self.labels.clear();
            self.labels.resize(n, 0);
        }
        self.frames += 1;

        // 1. statistics
        let (sum, sumsq) = gray.iter().fold((0u64, 0u64), |(s, q), &v| (s + v as u64, q + (v as u64) * (v as u64)));
        let mean = sum as f32 / n as f32;
        let var = (sumsq as f32 / n as f32 - mean * mean).max(0.0);
        let std = var.sqrt();
        out.mean_brightness = mean / 255.0;

        // 2. mask + 3. labeling (fused: we label straight from the predicate)
        let active = match self.cfg.mode {
            Mode::Bright => {
                let abs = self.cfg.threshold.clamp(0.0, 1.0) * 255.0;
                let adaptive = mean + self.cfg.adaptive_k * std;
                let blend = self.cfg.adaptive_blend.clamp(0.0, 1.0);
                let thr = (blend * adaptive + (1.0 - blend) * abs).clamp(8.0, 254.0) as u8;
                self.label_bright(gray, w, h, thr)
            }
            Mode::Motion => {
                let thr = (self.cfg.threshold.clamp(0.0, 1.0) * 255.0).clamp(2.0, 255.0) as i32;
                self.label_motion(gray, w, h, thr)
            }
        };
        let raw_energy = active as f32 / n as f32;
        let ea = self.cfg.energy_alpha.clamp(0.0, 1.0);
        self.energy += (raw_energy - self.energy) * ea;
        out.motion_energy = self.energy;

        // 4. detections: resolve labels, filter, sort, merge, cap
        self.collect_detections();
        out.raw_blob_count = self.merged.len();
        if self.merged.len() > self.cfg.max_blobs {
            self.merged.truncate(self.cfg.max_blobs);
        }

        // 5. tracking
        self.associate(t, out);
        self.last_t = Some(t);
    }

    /// Labels pixels with `luma >= thr`. Returns the number of active pixels.
    fn label_bright(&mut self, gray: &[u8], w: u32, h: u32, thr: u8) -> usize {
        self.label_with(gray, w, h, |v, _| v >= thr)
    }

    /// Labels pixels where `|frame − bg| > thr`, then updates the background.
    fn label_motion(&mut self, gray: &[u8], w: u32, h: u32, thr: i32) -> usize {
        let n = gray.len();
        if !self.bg_valid || self.bg.len() != n {
            self.bg.clear();
            self.bg.extend(gray.iter().map(|&v| (v as u16) << 8));
            self.bg_valid = true;
            // First frame: nothing can have moved yet.
            self.reset_labels_only(n);
            return 0;
        }
        // Take bg out to satisfy the borrow checker; put it back after.
        let bg = std::mem::take(&mut self.bg);
        let active = self.label_with(gray, w, h, |v, i| ((v as i32) - (bg[i] >> 8) as i32).abs() > thr);
        self.bg = bg;
        // Background update: bg += alpha * (frame - bg), in 8.8 fixed point.
        let alpha_q = (self.cfg.background_alpha.clamp(0.0, 1.0) * 65536.0) as i64;
        for (b, &v) in self.bg.iter_mut().zip(gray) {
            let cur = *b as i64;
            let target = (v as i64) << 8;
            let next = cur + (((target - cur) * alpha_q) >> 16);
            *b = next.clamp(0, 0xFFFF) as u16;
        }
        active
    }

    fn reset_labels_only(&mut self, n: usize) {
        self.labels.clear();
        self.labels.resize(n, 0);
        self.parent.clear();
        self.parent.push(0);
        self.accs.clear();
        self.accs.push(Acc::default());
    }

    /// One-pass 8-connected labeling with union-find. Per-label accumulators
    /// are filled during the pass; roots are resolved in `collect_detections`.
    fn label_with<F: Fn(u8, usize) -> bool>(&mut self, gray: &[u8], w: u32, h: u32, pred: F) -> usize {
        let (w, h) = (w as usize, h as usize);
        let n = w * h;
        self.labels.clear();
        self.labels.resize(n, 0);
        self.parent.clear();
        self.parent.push(0); // label 0 = background
        self.accs.clear();
        self.accs.push(Acc::default());
        let mut active = 0usize;
        let labels = &mut self.labels;
        let parent = &mut self.parent;
        let accs = &mut self.accs;

        #[inline]
        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            let mut root = x;
            while parent[root as usize] != root {
                root = parent[root as usize];
            }
            while parent[x as usize] != root {
                let next = parent[x as usize];
                parent[x as usize] = root;
                x = next;
            }
            root
        }
        #[inline]
        fn union(parent: &mut [u32], a: u32, b: u32) -> u32 {
            let ra = find(parent, a);
            let rb = find(parent, b);
            if ra == rb {
                return ra;
            }
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent[hi as usize] = lo;
            lo
        }

        for y in 0..h {
            let row = y * w;
            let mut left: u32 = 0;
            for x in 0..w {
                let i = row + x;
                let v = gray[i];
                if !pred(v, i) {
                    left = 0;
                    continue;
                }
                active += 1;
                // neighbours already labeled: left, up-left, up, up-right
                let mut lab = left;
                if y > 0 {
                    let up = i - w;
                    let u = labels[up];
                    let ul = if x > 0 { labels[up - 1] } else { 0 };
                    let ur = if x + 1 < w { labels[up + 1] } else { 0 };
                    for nb in [u, ul, ur] {
                        if nb != 0 {
                            lab = if lab == 0 { nb } else { union(parent, lab, nb) };
                        }
                    }
                }
                if lab == 0 {
                    lab = parent.len() as u32;
                    parent.push(lab);
                    accs.push(Acc::new(x as u32, y as u32, v));
                } else {
                    accs[lab as usize].add(x as u32, y as u32, v);
                }
                labels[i] = lab;
                left = lab;
            }
        }
        active
    }

    /// Resolve union-find roots, merge accumulators, filter by `min_blob_px`,
    /// sort by area and merge nearby boxes.
    fn collect_detections(&mut self) {
        // Merge each label's stats into its root (labels are resolved in
        // increasing order; roots are always the smallest label of a set).
        for i in 1..self.parent.len() {
            let mut r = self.parent[i] as usize;
            while self.parent[r] as usize != r {
                r = self.parent[r] as usize;
            }
            self.parent[i] = r as u32;
            if r != i {
                let a = self.accs[i];
                self.accs[r].merge(&a);
                self.accs[i].n = 0;
            }
        }
        self.dets.clear();
        let min_px = self.cfg.min_blob_px.max(1);
        for a in self.accs.iter().skip(1) {
            if a.n >= min_px {
                self.dets.push(Det::from_acc(a));
            }
        }
        self.dets.sort_by(|a, b| b.area.partial_cmp(&a.area).unwrap_or(std::cmp::Ordering::Equal));
        // Bound the merge work for pathological frames.
        let pre_cap = (self.cfg.max_blobs * 4).max(64);
        if self.dets.len() > pre_cap {
            self.dets.truncate(pre_cap);
        }
        // Greedy merge: absorb any later det within merge_dist of an earlier one.
        self.merged.clear();
        let md = self.cfg.merge_dist.max(0.0);
        for i in 0..self.dets.len() {
            let d = self.dets[i];
            if d.area <= 0.0 {
                continue;
            }
            let mut m = d;
            let mut changed = true;
            while changed {
                changed = false;
                for j in (i + 1)..self.dets.len() {
                    let o = self.dets[j];
                    if o.area > 0.0 && m.gap(&o) <= md {
                        m.absorb(&o);
                        self.dets[j].area = 0.0;
                        changed = true;
                    }
                }
            }
            self.merged.push(m);
        }
        self.merged.sort_by(|a, b| b.area.partial_cmp(&a.area).unwrap_or(std::cmp::Ordering::Equal));
    }

    fn associate(&mut self, t: Seconds, out: &mut FrameAnalysis) {
        let fw = self.width as f32;
        let fh = self.height as f32;
        let dt = match self.last_t {
            Some(lt) if t > lt => (t - lt) as f32,
            _ => 0.0,
        };
        let predict = dt > 0.0 && dt < 2.0;

        for tr in &mut self.tracks {
            tr.matched = false;
        }
        self.det_used.clear();
        self.det_used.resize(self.merged.len(), false);

        // Candidate pairs within the gate.
        self.pairs.clear();
        for (ti, tr) in self.tracks.iter().enumerate() {
            let (px, py) = if predict { (tr.x + tr.vx * dt, tr.y + tr.vy * dt) } else { (tr.x, tr.y) };
            let tdiag = (tr.w * tr.w + tr.h * tr.h).sqrt();
            for (di, d) in self.merged.iter().enumerate() {
                let dx = d.cx / fw - px;
                let dy = d.cy / fh - py;
                let dist = (dx * dx + dy * dy).sqrt();
                let bw = (d.maxx - d.minx) / fw;
                let bh = (d.maxy - d.miny) / fh;
                let ddiag = (bw * bw + bh * bh).sqrt();
                let gate = self.cfg.gate_base + self.cfg.gate_scale * tdiag.max(ddiag);
                if dist <= gate {
                    self.pairs.push((dist, ti as u32, di as u32));
                }
            }
        }
        self.pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let va = self.cfg.velocity_alpha.clamp(0.0, 1.0);
        let line = self.cfg.line.filter(|l| l.is_finite());
        for &(_, ti, di) in &self.pairs {
            let (ti, di) = (ti as usize, di as usize);
            if self.tracks[ti].matched || self.det_used[di] {
                continue;
            }
            self.det_used[di] = true;
            let d = self.merged[di];
            let tr = &mut self.tracks[ti];
            tr.matched = true;
            let nx = d.cx / fw;
            let ny = d.cy / fh;
            let (sx, sy) = (nx - tr.x, ny - tr.y);
            let step = (sx * sx + sy * sy).sqrt();
            if dt > 0.0 {
                let ivx = sx / dt;
                let ivy = sy / dt;
                if tr.hits <= 1 {
                    tr.vx = ivx;
                    tr.vy = ivy;
                } else {
                    tr.vx += (ivx - tr.vx) * va;
                    tr.vy += (ivy - tr.vy) * va;
                }
            }
            let prev_y = tr.y;
            tr.x = nx;
            tr.y = ny;
            tr.w = (d.maxx - d.minx) / fw;
            tr.h = (d.maxy - d.miny) / fh;
            tr.area = d.area / (fw * fh);
            tr.brightness = d.bright / 255.0;
            tr.distance += step;
            tr.hits += 1;
            tr.missed = 0;
            tr.last_t = t;
            let was_confirmed = tr.confirmed;
            if !tr.confirmed && tr.hits >= self.cfg.confirm_frames.max(1) {
                tr.confirmed = true;
            }
            if tr.confirmed {
                let b = tr.to_blob(t);
                if !was_confirmed {
                    out.entered.push(b);
                }
                out.blobs.push(b);
                if let Some(l) = line {
                    if was_confirmed && (prev_y < l) != (ny < l) {
                        let mut c = b;
                        c.dir = if ny >= l { 1.0 } else { -1.0 };
                        out.crossings.push(c);
                    }
                }
            }
        }

        // Unmatched detections start new tracks.
        for (di, used) in self.det_used.iter().enumerate() {
            if *used {
                continue;
            }
            let d = self.merged[di];
            let id = self.next_id;
            self.next_id += 1;
            let confirm_now = self.cfg.confirm_frames <= 1;
            let tr = Track {
                id,
                x: d.cx / fw,
                y: d.cy / fh,
                w: (d.maxx - d.minx) / fw,
                h: (d.maxy - d.miny) / fh,
                area: d.area / (fw * fh),
                brightness: d.bright / 255.0,
                vx: 0.0,
                vy: 0.0,
                start_t: t,
                last_t: t,
                start_y: d.cy / fh,
                distance: 0.0,
                hits: 1,
                missed: 0,
                confirmed: confirm_now,
                matched: true,
            };
            if confirm_now {
                let b = tr.to_blob(t);
                out.entered.push(b);
                out.blobs.push(b);
            }
            self.tracks.push(tr);
        }

        // Unmatched tracks age out.
        let max_missed = self.cfg.max_missed;
        let exited = &mut out.exited;
        self.tracks.retain_mut(|tr| {
            if tr.matched {
                return true;
            }
            tr.missed += 1;
            if tr.missed > max_missed {
                if tr.confirmed {
                    exited.push(tr.to_exit_blob());
                }
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen::{self, Disc};

    fn mask_frame(rows: &[&str]) -> (Vec<u8>, u32, u32) {
        let h = rows.len() as u32;
        let w = rows[0].len() as u32;
        let mut g = Vec::with_capacity((w * h) as usize);
        for r in rows {
            for c in r.chars() {
                g.push(if c == '#' { 255 } else { 0 });
            }
        }
        (g, w, h)
    }

    fn cfg_no_adaptive() -> TrackerConfig {
        TrackerConfig {
            adaptive_blend: 0.0,
            threshold: 0.5,
            min_blob_px: 1,
            merge_dist: 0.0,
            confirm_frames: 1,
            line: None,
            ..Default::default()
        }
    }

    #[test]
    fn labels_hand_drawn_components() {
        let (g, w, h) = mask_frame(&[
            "##......#.",
            "##.....#..",
            "......#...",
            "...##.....",
            "...##..###",
            ".......###",
        ]);
        let mut tr = Tracker::new(cfg_no_adaptive());
        let a = tr.process(&g, w, h, 0.0);
        // 8-connectivity: diagonal line is one component -> 4 blobs total.
        assert_eq!(a.raw_blob_count, 4, "{:?}", a.blobs);
        assert_eq!(a.blobs.len(), 4);
        let mut areas: Vec<f32> = a.blobs.iter().map(|b| b.area * (w * h) as f32).collect();
        areas.sort_by(|x, y| x.partial_cmp(y).unwrap());
        assert_eq!(areas.iter().map(|a| a.round() as u32).collect::<Vec<_>>(), vec![3, 4, 4, 6]);
        // the 2x2 block at top-left: centroid (0.5,0.5) px -> normalized
        let tl = a.blobs.iter().find(|b| b.x < 0.2 && b.y < 0.2).unwrap();
        assert!((tl.x - 0.5 / w as f32).abs() < 1e-5);
        assert!((tl.w - 2.0 / w as f32).abs() < 1e-5);
        assert!((tl.brightness - 1.0).abs() < 1e-5);
        assert!(a.motion_energy > 0.0);
    }

    #[test]
    fn min_blob_filters_speckle_and_ids_are_stable() {
        let (g, w, h) = mask_frame(&["#....", ".....", "..###", "..###"]);
        let mut cfg = cfg_no_adaptive();
        cfg.min_blob_px = 2;
        let mut tr = Tracker::new(cfg);
        let a = tr.process(&g, w, h, 0.0);
        assert_eq!(a.blobs.len(), 1);
        let id = a.blobs[0].id;
        let b = tr.process(&g, w, h, 0.1);
        assert_eq!(b.blobs.len(), 1);
        assert_eq!(b.blobs[0].id, id);
        assert!(b.entered.is_empty());
    }

    #[test]
    fn tracks_three_discs_moving_down_and_emits_three_exits() {
        let (w, h) = (160u32, 120u32);
        let fps = 20.0;
        let speed_px = 2.0; // px per frame downward
        let mut tr = Tracker::new(TrackerConfig { line: Some(0.5), ..Default::default() });
        let mut entered = 0;
        let mut exits = Vec::new();
        let mut crossings = 0;
        let mut max_live = 0;
        for f in 0..60 {
            let y = -10.0 + f as f32 * speed_px;
            let discs = [
                Disc { x: 30.0, y, r: 4.0, brightness: 230 },
                Disc { x: 80.0, y: y + 5.0, r: 4.0, brightness: 220 },
                Disc { x: 130.0, y: y - 5.0, r: 4.0, brightness: 240 },
            ];
            let frame = testgen::render(w, h, 18, 10, &discs, f as u64 + 1);
            let a = tr.process(&frame, w, h, f as f64 / fps);
            entered += a.entered.len();
            crossings += a.crossings.len();
            exits.extend(a.exited.iter().copied());
            max_live = max_live.max(a.blobs.len());
        }
        // Discs leave the bottom at y=120 around frame 65; push a few empty frames.
        for f in 60..75 {
            let frame = testgen::render(w, h, 18, 10, &[], f as u64 + 1);
            let a = tr.process(&frame, w, h, f as f64 / fps);
            exits.extend(a.exited.iter().copied());
            crossings += a.crossings.len();
        }
        assert_eq!(entered, 3, "entered");
        assert_eq!(max_live, 3);
        assert_eq!(exits.len(), 3, "exits: {exits:?}");
        assert_eq!(crossings, 3, "line crossings");
        let expected_speed = speed_px * fps / h as f32; // frame fractions per second
        for e in &exits {
            assert_eq!(e.dir, 1.0, "moving down");
            assert!((e.speed - expected_speed).abs() < expected_speed * 0.15, "speed {} vs {}", e.speed, expected_speed);
            assert!(e.age_s > 2.0, "duration {}", e.age_s);
            assert!(e.y > 0.8, "exit near the bottom: {}", e.y);
            assert!(e.vy > 0.0);
        }
    }

    #[test]
    fn headlight_pairs_merge_into_one_car() {
        let (w, h) = (160u32, 120u32);
        let discs = [
            Disc { x: 60.0, y: 60.0, r: 2.5, brightness: 250 },
            Disc { x: 68.0, y: 60.0, r: 2.5, brightness: 250 },
            // a second car far away
            Disc { x: 120.0, y: 30.0, r: 2.5, brightness: 250 },
            Disc { x: 128.0, y: 30.0, r: 2.5, brightness: 250 },
        ];
        let frame = testgen::render(w, h, 15, 6, &discs, 7);
        let mut tr = Tracker::new(TrackerConfig { merge_dist: 6.0, confirm_frames: 1, line: None, ..Default::default() });
        let a = tr.process(&frame, w, h, 0.0);
        assert_eq!(a.blobs.len(), 2, "{:?}", a.blobs);
        let car = a.blobs.iter().find(|b| b.y > 0.4).unwrap();
        assert!((car.x - 64.0 / 160.0).abs() < 0.01);
        assert!(car.w > 10.0 / 160.0);
        // Without merging we see four.
        let mut tr2 = Tracker::new(TrackerConfig { merge_dist: 0.0, confirm_frames: 1, line: None, ..Default::default() });
        assert_eq!(tr2.process(&frame, w, h, 0.0).blobs.len(), 4);
    }

    #[test]
    fn line_crossings_count_both_directions() {
        let (w, h) = (80u32, 60u32);
        let mut tr = Tracker::new(TrackerConfig { line: Some(0.5), confirm_frames: 1, ..Default::default() });
        let mut down = 0;
        let mut up = 0;
        // one disc goes down, another goes up
        for f in 0..30 {
            let yd = 5.0 + f as f32 * 1.5;
            let yu = 55.0 - f as f32 * 1.5;
            let discs = [Disc { x: 20.0, y: yd, r: 3.0, brightness: 240 }, Disc { x: 60.0, y: yu, r: 3.0, brightness: 240 }];
            let frame = testgen::render(w, h, 10, 4, &discs, f as u64 + 3);
            let a = tr.process(&frame, w, h, f as f64 * 0.05);
            for c in &a.crossings {
                if c.dir > 0.0 {
                    down += 1;
                } else {
                    up += 1;
                }
            }
        }
        assert_eq!((down, up), (1, 1));
    }

    #[test]
    fn motion_mode_sees_moving_disc_and_ignores_static_bright_region() {
        let (w, h) = (120u32, 90u32);
        let cfg = TrackerConfig { mode: Mode::Motion, threshold: 0.25, min_blob_px: 4, confirm_frames: 1, line: None, ..Default::default() };
        let mut tr = Tracker::new(cfg);
        let lamp = Disc { x: 100.0, y: 15.0, r: 6.0, brightness: 255 };
        // settle the background with the static lamp only
        for f in 0..5 {
            let frame = testgen::render(w, h, 80, 8, &[lamp], 100 + f);
            let a = tr.process(&frame, w, h, f as f64 * 0.1);
            if f > 0 {
                assert!(a.blobs.is_empty(), "static lamp should not be a blob: {:?}", a.blobs);
            }
        }
        // a moderately bright disc moves across a mid-gray background
        let mut seen = 0;
        let mut max_blobs = 0;
        for f in 5..30 {
            let mover = Disc { x: 10.0 + (f - 5) as f32 * 4.0, y: 60.0, r: 5.0, brightness: 170 };
            let frame = testgen::render(w, h, 80, 8, &[lamp, mover], 100 + f);
            let a = tr.process(&frame, w, h, f as f64 * 0.1);
            max_blobs = max_blobs.max(a.blobs.len());
            if let Some(b) = a.blobs.first() {
                seen += 1;
                assert!(b.y > 0.5, "blob must be the mover, not the lamp: {b:?}");
            }
            assert!(a.motion_energy > 0.0);
        }
        assert!(seen >= 20, "mover seen in {seen} frames");
        assert_eq!(max_blobs, 1);
    }

    #[test]
    fn garbage_dimensions_do_not_panic() {
        let mut tr = Tracker::new(TrackerConfig::default());
        let a = tr.process(&[1, 2, 3], 10, 10, 0.0);
        assert!(a.blobs.is_empty());
        let a = tr.process(&[], 0, 0, 0.0);
        assert!(a.blobs.is_empty());
        let big = vec![255u8; 64];
        let a = tr.process(&big, 8, 8, 1.0);
        assert!(a.raw_blob_count <= 1);
    }

    #[test]
    fn frame_320x240_processes_fast_enough() {
        let (w, h) = (320u32, 240u32);
        let mut tr = Tracker::new(TrackerConfig::default());
        let mut times = Vec::new();
        let mut out = FrameAnalysis::default();
        for f in 0..30 {
            let discs: Vec<Disc> =
                (0..12).map(|i| Disc { x: 20.0 + i as f32 * 25.0, y: (f * 3 + i * 17) as f32 % 240.0, r: 5.0, brightness: 230 }).collect();
            let frame = testgen::render(w, h, 20, 30, &discs, f as u64 + 1);
            let start = std::time::Instant::now();
            tr.process_into(&frame, w, h, f as f64 / 15.0, &mut out);
            times.push(start.elapsed());
        }
        times.sort();
        let median = times[times.len() / 2];
        eprintln!("320x240 bright-mode frame: median {:?}, max {:?}", median, times[times.len() - 1]);
        assert!(median < std::time::Duration::from_millis(20), "median {median:?}");
    }
}
