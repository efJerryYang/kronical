#![allow(unused)]
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use clap::{ArgAction, Parser, ValueEnum};
use kronical::daemon::events::{MouseEventData, MouseEventKind, MousePosition};
use kronical_core::compression::{CompactMouseTrajectory, RawEventPointer};
use rusqlite as sqlite;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;
// Progress bar for long selections
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Parser, Debug)]
#[command(name = "bench-mouse-traj", version, about = "Benchmark mouse trajectory compressors vs. reference", long_about = None)]
struct Cli {
    /// Database path (SQLite3 or DuckDB)
    #[arg(long, default_value = "~/.kronical/data.sqlite3")]
    db: String,

    /// DB backend
    #[arg(long, value_enum, default_value_t = DbBackend::Sqlite)]
    backend: DbBackend,

    /// Plot/benchmark selection by indices
    #[arg(
        long,
        value_delimiter = ',',
        help = "Comma-separated indices (0-based) to include"
    )]
    indices: Option<Vec<usize>>,

    /// Select a range [start:end] inclusive (0-based)
    #[arg(long)]
    range: Option<String>,

    /// Last N compact events to include
    #[arg(long, default_value_t = 1)]
    last: usize,

    /// Max events to process (after selection)
    #[arg(long)]
    limit: Option<usize>,

    /// Print per-event details
    #[arg(long, action = ArgAction::SetTrue)]
    verbose: bool,

    /// Pixel tolerance used for readability-focused PASS criteria (default: 2.0px)
    #[arg(long, default_value_t = 2.0)]
    tau: f32,

    /// Allowed path length drift percentage for PASS (default: 10%)
    #[arg(long, default_value_t = 10.0)]
    max_length_drift_pct: f32,

    /// Enable parallel processing over selected events
    #[arg(long, action = ArgAction::SetTrue)]
    parallel: bool,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug, ValueEnum)]
enum DbBackend {
    Sqlite,
    Duckdb,
}

#[derive(Debug, Clone, Deserialize)]
struct CompactRow {
    row_index: usize,
    payload: CompactMouseTrajectory,
    raw_ref: Option<RawEventPointer>,
}

#[derive(Debug, Clone)]
struct RawMousePoint {
    ts: DateTime<Utc>,
    id: u64,
    pos: MousePosition,
    kind: MouseEventKind,
}

#[derive(Debug, Clone)]
struct EventWork {
    // English comment: index in the compact rows vector, for debugging/logging
    idx: usize,
    // English comment: raw points reconstructed for this compact event
    raw_pts: Vec<MousePosition>,
    // English comment: reference simplified path stored in DB
    ref_path: Vec<MousePosition>,
}

// ------------- DB Readers -------------

trait DbReader {
    fn load_all_compact_mouse(&mut self) -> Result<Vec<CompactRow>>;
    fn load_raw_mouse_for_link(
        &mut self,
        link: Option<&RawEventPointer>,
    ) -> Result<Vec<RawMousePoint>>;
}

struct SqliteReader {
    conn: sqlite::Connection,
}

impl SqliteReader {
    fn new(path: &str) -> Result<Self> {
        let p = shellexpand::tilde(path).to_string();
        let conn = sqlite::Connection::open(p)?;
        Ok(Self { conn })
    }
}

impl DbReader for SqliteReader {
    fn load_all_compact_mouse(&mut self) -> Result<Vec<CompactRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT rowid, payload, raw_event_ref FROM compact_events WHERE kind='compact:mouse_traj' ORDER BY start_time"
        )?;
        let rows = stmt
            .query_map([], |row| {
                let rowid: i64 = row.get(0)?;
                let payload_js: Option<String> = row.get(1)?;
                let raw_ref_js: Option<String> = row.get(2)?;
                let payload: CompactMouseTrajectory =
                    serde_json::from_str(&payload_js.unwrap_or_else(|| "null".to_string()))
                        .unwrap();
                let raw_ref: Option<RawEventPointer> = raw_ref_js
                    .and_then(|js| serde_json::from_str(&js).ok())
                    .or_else(|| payload.raw_event_ids.clone());
                Ok(CompactRow {
                    row_index: rowid as usize - 1,
                    payload,
                    raw_ref,
                })
            })?
            .collect::<sqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn load_raw_mouse_for_link(
        &mut self,
        link: Option<&RawEventPointer>,
    ) -> Result<Vec<RawMousePoint>> {
        let Some(link) = link else {
            return Ok(vec![]);
        };
        if link.ids.is_empty() {
            return Ok(vec![]);
        }
        let mut out: Vec<RawMousePoint> = Vec::new();
        // chunk to avoid large IN clause
        for chunk in link.ids.chunks(500) {
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT event_id, timestamp, kind, payload FROM raw_envelopes WHERE kind='signal:mouse' AND event_id IN ({}) ORDER BY timestamp, event_id",
                placeholders
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = chunk
                .iter()
                .map(|id| sqlite::types::Value::from(*id as i64))
                .collect::<Vec<_>>();
            let rows = stmt.query_map(sqlite::params_from_iter(params), |row| {
                let id: i64 = row.get(0)?;
                let ts_s: String = row.get(1)?;
                let kind_s: String = row.get(2)?;
                let payload_js: Option<String> = row.get(3)?;
                let ts = DateTime::parse_from_rfc3339(&ts_s)
                    .unwrap()
                    .with_timezone(&Utc);
                let js = payload_js.unwrap_or_else(|| "{}".into());
                let data: MouseEventData = serde_json::from_str(&js).unwrap_or(MouseEventData {
                    position: MousePosition { x: 0, y: 0 },
                    button: None,
                    click_count: None,
                    event_type: None,
                    wheel_amount: None,
                    wheel_rotation: None,
                    wheel_axis: None,
                    wheel_scroll_type: None,
                });
                // Only movement and dragged
                let kind = match data.event_type {
                    Some(MouseEventKind::Moved) => MouseEventKind::Moved,
                    Some(MouseEventKind::Dragged) => MouseEventKind::Dragged,
                    _ => return Ok(None),
                };
                let _ = kind_s; // not used further
                Ok(Some(RawMousePoint {
                    ts,
                    id: id as u64,
                    pos: data.position,
                    kind,
                }))
            })?;
            for r in rows {
                if let Some(p) = r? {
                    out.push(p);
                }
            }
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));
        Ok(out)
    }
}

// DuckDB reader (optional). We keep it minimal and mirror the same schema.
struct DuckReader {
    conn: duckdb::Connection,
}
impl DuckReader {
    fn new(path: &str) -> Result<Self> {
        let p = shellexpand::tilde(path).to_string();
        let conn = duckdb::Connection::open(p)?;
        Ok(Self { conn })
    }
}
impl DbReader for DuckReader {
    fn load_all_compact_mouse(&mut self) -> Result<Vec<CompactRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT rowid, payload, raw_event_ref FROM compact_events WHERE kind='compact:mouse_traj' ORDER BY start_time"
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let payload_js: Option<String> = row.get(1)?;
            let raw_ref_js: Option<String> = row.get(2)?;
            let payload: CompactMouseTrajectory =
                serde_json::from_str(&payload_js.unwrap_or_else(|| "null".to_string())).unwrap();
            let raw_ref: Option<RawEventPointer> = raw_ref_js
                .and_then(|js| serde_json::from_str(&js).ok())
                .or_else(|| payload.raw_event_ids.clone());
            out.push(CompactRow {
                row_index: rowid as usize - 1,
                payload,
                raw_ref,
            });
        }
        Ok(out)
    }

    fn load_raw_mouse_for_link(
        &mut self,
        link: Option<&RawEventPointer>,
    ) -> Result<Vec<RawMousePoint>> {
        let Some(link) = link else {
            return Ok(vec![]);
        };
        if link.ids.is_empty() {
            return Ok(vec![]);
        }
        let mut out: Vec<RawMousePoint> = Vec::new();
        for chunk in link.ids.chunks(900) {
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT event_id, timestamp, kind, payload FROM raw_envelopes WHERE kind='signal:mouse' AND event_id IN ({}) ORDER BY timestamp, event_id",
                placeholders
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = duckdb::params_from_iter(chunk.iter().copied().map(|x| x as i64));
            let mut rows = stmt.query(params)?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let ts: DateTime<Utc> = row.get::<_, chrono::DateTime<Utc>>(1)?;
                let _kind_s: String = row.get(2)?;
                let payload_js: Option<String> = row.get(3)?;
                let js = payload_js.unwrap_or_else(|| "{}".into());
                let data: MouseEventData = serde_json::from_str(&js).unwrap_or(MouseEventData {
                    position: MousePosition { x: 0, y: 0 },
                    button: None,
                    click_count: None,
                    event_type: None,
                    wheel_amount: None,
                    wheel_rotation: None,
                    wheel_axis: None,
                    wheel_scroll_type: None,
                });
                let kind = match data.event_type {
                    Some(MouseEventKind::Moved) => MouseEventKind::Moved,
                    Some(MouseEventKind::Dragged) => MouseEventKind::Dragged,
                    _ => continue,
                };
                out.push(RawMousePoint {
                    ts,
                    id: id as u64,
                    pos: data.position,
                    kind,
                });
            }
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.id.cmp(&b.id)));
        Ok(out)
    }
}

// ------------- Geometry helpers + metrics -------------

// Compute polyline length (in pixels)
fn path_length(pts: &[MousePosition]) -> f32 {
    if pts.len() < 2 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    for w in pts.windows(2) {
        sum += dist(&w[0], &w[1]);
    }
    sum
}

fn dist(a: &MousePosition, b: &MousePosition) -> f32 {
    let dx = (a.x - b.x) as f32;
    let dy = (a.y - b.y) as f32;
    (dx * dx + dy * dy).sqrt()
}

fn point_segment_distance(p: &MousePosition, a: &MousePosition, b: &MousePosition) -> f32 {
    if a.x == b.x && a.y == b.y {
        return dist(p, a);
    }
    let ax = a.x as f32;
    let ay = a.y as f32;
    let bx = b.x as f32;
    let by = b.y as f32;
    let px = p.x as f32;
    let py = p.y as f32;
    let vx = bx - ax;
    let vy = by - ay;
    let wx = px - ax;
    let wy = py - ay;
    let c1 = vx * wx + vy * wy;
    if c1 <= 0.0 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let c2 = vx * vx + vy * vy;
    if c2 <= 0.0 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let t = (c1 / c2).clamp(0.0, 1.0);
    let projx = ax + t * vx;
    let projy = ay + t * vy;
    ((px - projx).powi(2) + (py - projy).powi(2)).sqrt()
}

fn polyline_deviation_stats(raw: &[MousePosition], simplified: &[MousePosition]) -> (f32, f32) {
    if simplified.is_empty() {
        return (f32::INFINITY, f32::INFINITY);
    }
    if simplified.len() == 1 {
        let d: Vec<f32> = raw.iter().map(|p| dist(p, &simplified[0])).collect();
        let mean = d.iter().copied().sum::<f32>() / (d.len().max(1) as f32);
        let maxd = d.into_iter().fold(0.0, f32::max);
        return (mean, maxd);
    }
    let mut sum = 0.0;
    let mut maxd = 0.0;
    let n = raw.len().max(1) as f32;
    for p in raw.iter() {
        // find nearest segment
        let mut best = f32::INFINITY;
        for w in simplified.windows(2) {
            let d = point_segment_distance(p, &w[0], &w[1]);
            if d < best {
                best = d;
            }
        }
        sum += best;
        if best > maxd {
            maxd = best;
        }
    }
    (sum / n, maxd)
}

// ------------- Algorithms -------------

trait Simplifier {
    fn name(&self) -> &'static str;
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition>;
    fn streaming(&self) -> bool {
        false
    }
}

// A: Reference Ramer-Douglas-Peucker (RDP) as in engine
struct AlgoRdp;
impl AlgoRdp {
    fn rdp(points: &[MousePosition], eps: f32) -> Vec<MousePosition> {
        if points.len() <= 2 {
            return points.to_vec();
        }
        let mut max_d = 0.0;
        let mut max_i = 0usize;
        for i in 1..(points.len() - 1) {
            let d = point_segment_distance(&points[i], &points[0], &points[points.len() - 1]);
            if d > max_d {
                max_d = d;
                max_i = i;
            }
        }
        if max_d > eps {
            let mut left = Self::rdp(&points[0..=max_i], eps);
            let right = Self::rdp(&points[max_i..], eps);
            left.pop();
            left.into_iter().chain(right.into_iter()).collect()
        } else {
            vec![points[0].clone(), points[points.len() - 1].clone()]
        }
    }
}
impl Simplifier for AlgoRdp {
    fn name(&self) -> &'static str {
        "rdp"
    }
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition> {
        Self::rdp(pts, param.max(0.0))
    }
}

// B: Radial-distance (one-pass threshold on distance from last kept)
struct AlgoRadial;
impl Simplifier for AlgoRadial {
    fn name(&self) -> &'static str {
        "radial"
    }
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition> {
        if pts.is_empty() {
            return vec![];
        }
        let eps = param.max(0.0);
        let mut out = vec![pts[0].clone()];
        let mut last = pts[0].clone();
        for p in &pts[1..] {
            if dist(&last, p) >= eps {
                out.push(p.clone());
                last = p.clone();
            }
        }
        if out
            .last()
            .map(|q| q.x != pts[pts.len() - 1].x || q.y != pts[pts.len() - 1].y)
            .unwrap_or(true)
        {
            out.push(pts[pts.len() - 1].clone());
        }
        out
    }
}

// C: Reumann–Witkam (corridor width)
struct AlgoReumannWitkam;
impl Simplifier for AlgoReumannWitkam {
    fn name(&self) -> &'static str {
        "reumann_witkam"
    }
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition> {
        if pts.len() <= 2 {
            return pts.to_vec();
        }
        let w = param.max(0.0);
        let mut out: Vec<MousePosition> = Vec::with_capacity(pts.len());
        let mut i = 0;
        out.push(pts[0].clone());
        while i < pts.len() - 1 {
            let mut j = i + 1;
            // advance j as long as within corridor around segment pts[i]->pts[i+1]
            while j < pts.len() {
                let d = point_segment_distance(&pts[j], &pts[i], &pts[i + 1]);
                if d > w {
                    break;
                }
                j += 1;
            }
            let keep = if j >= pts.len() { pts.len() - 1 } else { j - 1 };
            if out
                .last()
                .map(|p| p.x != pts[keep].x || p.y != pts[keep].y)
                .unwrap_or(true)
            {
                out.push(pts[keep].clone());
            }
            i = keep;
        }
        if out
            .last()
            .map(|q| q.x != pts[pts.len() - 1].x || q.y != pts[pts.len() - 1].y)
            .unwrap_or(true)
        {
            out.push(pts[pts.len() - 1].clone());
        }
        out
    }
}

// D: Visvalingam–Whyatt (remove points by smallest effective area)
struct AlgoVisvalingam;
#[derive(Clone, Debug)]
struct VwNode {
    idx: usize,
    area: f32,
}
impl PartialEq for VwNode {
    fn eq(&self, other: &Self) -> bool {
        self.area.eq(&other.area)
    }
}
impl Eq for VwNode {}
impl PartialOrd for VwNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        other.area.partial_cmp(&self.area)
    }
} // min-heap via reverse
impl Ord for VwNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}
impl Simplifier for AlgoVisvalingam {
    fn name(&self) -> &'static str {
        "visvalingam"
    }
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition> {
        if pts.len() <= 2 {
            return pts.to_vec();
        }
        let thresh = param.max(0.0);
        let n = pts.len();
        let mut removed = vec![false; n];
        let mut areas = vec![f32::INFINITY; n];
        let mut heap: BinaryHeap<VwNode> = BinaryHeap::new();
        // compute initial areas
        for i in 1..n - 1 {
            let a = triangle_area(&pts[i - 1], &pts[i], &pts[i + 1]);
            areas[i] = a;
            heap.push(VwNode { idx: i, area: a });
        }
        while let Some(VwNode { idx, area }) = heap.pop() {
            if removed[idx] || areas[idx] != area {
                continue;
            }
            if area >= thresh {
                break;
            }
            removed[idx] = true;
            // recompute neighbors
            let prev = (idx as isize - 1) as usize;
            let next = idx + 1;
            if idx > 1 && !removed[prev] && next < n && !removed[next] {
                let a = triangle_area(
                    &seek_left(pts, &removed, prev),
                    &pts[prev],
                    &seek_right(pts, &removed, next),
                );
                areas[prev] = a;
                heap.push(VwNode { idx: prev, area: a });
            }
            if next < n - 1 && !removed[next] && idx > 0 && !removed[prev] {
                let a = triangle_area(
                    &seek_left(pts, &removed, idx),
                    &pts[next],
                    &seek_right(pts, &removed, next + 1),
                );
                areas[next] = a;
                heap.push(VwNode { idx: next, area: a });
            }
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            if !removed[i] {
                out.push(pts[i].clone());
            }
        }
        if out.len() < 2 {
            return pts[0..2.min(pts.len())].to_vec();
        }
        out
    }
}

fn triangle_area(a: &MousePosition, b: &MousePosition, c: &MousePosition) -> f32 {
    0.5 * ((a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y)) as f32).abs()
}
fn seek_left(pts: &[MousePosition], removed: &[bool], mut i: usize) -> MousePosition {
    while i > 0 && removed[i] {
        i -= 1;
    }
    pts[i].clone()
}
fn seek_right(pts: &[MousePosition], removed: &[bool], mut i: usize) -> MousePosition {
    while i < pts.len() - 1 && removed[i] {
        i += 1;
    }
    pts[i].clone()
}

// E: Bottom-up (iteratively remove point with minimum perpendicular error)
struct AlgoBottomUp;
#[derive(Clone, Debug)]
struct BuNode {
    idx: usize,
    err: f32,
}
impl PartialEq for BuNode {
    fn eq(&self, other: &Self) -> bool {
        self.err.eq(&other.err)
    }
}
impl Eq for BuNode {}
impl PartialOrd for BuNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        other.err.partial_cmp(&self.err)
    }
} // min-heap via reverse
impl Ord for BuNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}
impl Simplifier for AlgoBottomUp {
    fn name(&self) -> &'static str {
        "bottom_up"
    }
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition> {
        if pts.len() <= 2 {
            return pts.to_vec();
        }
        let eps = param.max(0.0);
        let n = pts.len();
        let mut removed = vec![false; n];
        let mut heap: BinaryHeap<BuNode> = BinaryHeap::new();
        let mut left = vec![0usize; n];
        let mut right = vec![0usize; n];
        for i in 0..n {
            left[i] = i.saturating_sub(1);
            right[i] = (i + 1).min(n - 1);
        }
        for i in 1..n - 1 {
            let e = point_segment_distance(&pts[i], &pts[i - 1], &pts[i + 1]);
            heap.push(BuNode { idx: i, err: e });
        }
        while let Some(BuNode { idx, err }) = heap.pop() {
            if removed[idx] {
                continue;
            }
            // recompute with current neighbors
            let li = left[idx];
            let ri = right[idx];
            let curr = point_segment_distance(&pts[idx], &pts[li], &pts[ri]);
            if (curr - err).abs() > 1e-6 {
                heap.push(BuNode { idx, err: curr });
                continue;
            }
            if curr > eps {
                break;
            }
            removed[idx] = true;
            right[li] = ri;
            left[ri] = li;
            if li > 0 {
                let e = point_segment_distance(&pts[li], &pts[left[li]], &pts[ri]);
                heap.push(BuNode { idx: li, err: e });
            }
            if ri < n - 1 {
                let e = point_segment_distance(&pts[ri], &pts[li], &pts[right[ri]]);
                heap.push(BuNode { idx: ri, err: e });
            }
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            if !removed[i] {
                out.push(pts[i].clone());
            }
        }
        if out.len() < 2 {
            return pts[0..2.min(pts.len())].to_vec();
        }
        out
    }
}

// F: Streaming radial (one-pass), distinct from B by stricter turn-preserve heuristic
struct AlgoStreamingRadial;
impl Simplifier for AlgoStreamingRadial {
    fn name(&self) -> &'static str {
        "radial_s"
    }
    fn simplify(&mut self, pts: &[MousePosition], param: f32) -> Vec<MousePosition> {
        if pts.len() <= 2 {
            return pts.to_vec();
        }
        let eps = param.max(0.0);
        let mut out = Vec::with_capacity(pts.len());
        out.push(pts[0].clone());
        let mut anchor = pts[0].clone();
        let mut last = pts[0].clone();
        for p in &pts[1..] {
            let d = dist(&last, p);
            // preserve corners by angle check if last deviates direction
            let keep_for_turn = if out.len() >= 1 {
                let a = &anchor;
                let b = &last;
                let c = p;
                let abx = (b.x - a.x) as f32;
                let aby = (b.y - a.y) as f32;
                let bcx = (c.x - b.x) as f32;
                let bcy = (c.y - b.y) as f32;
                let dot = abx * bcx + aby * bcy;
                let n1 = (abx * abx + aby * aby).sqrt();
                let n2 = (bcx * bcx + bcy * bcy).sqrt();
                let cos = if n1 > 0.0 && n2 > 0.0 {
                    (dot / (n1 * n2)).clamp(-1.0, 1.0)
                } else {
                    1.0
                };
                // significant turn if angle > ~30 deg (cos < 0.866)
                cos < 0.866
            } else {
                false
            };
            if d >= eps || keep_for_turn {
                out.push(p.clone());
                anchor = last.clone();
            }
            last = p.clone();
        }
        if out
            .last()
            .map(|q| q.x != pts[pts.len() - 1].x || q.y != pts[pts.len() - 1].y)
            .unwrap_or(true)
        {
            out.push(pts[pts.len() - 1].clone());
        }
        out
    }
    fn streaming(&self) -> bool {
        true
    }
}

// ------------- Calibration (match reference mean deviation) -------------

fn calibrate_param(target_mean: f32, f: &mut dyn FnMut(f32) -> f32, lo: f32, hi: f32) -> f32 {
    // binary search for param s.t. mean ~ target
    let mut a = lo;
    let mut b = hi;
    let mut best = b;
    let mut best_err = f32::INFINITY;
    for _ in 0..16 {
        let mid = (a + b) * 0.5;
        let m = f(mid);
        let err = (m - target_mean).abs();
        if err < best_err {
            best = mid;
            best_err = err;
        }
        if m < target_mean {
            // need less simplification -> decrease param
            b = mid;
        } else {
            a = mid;
        }
    }
    best
}

// ------------- Main -------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut reader: Box<dyn DbReader> = match cli.backend {
        DbBackend::Sqlite => Box::new(SqliteReader::new(&cli.db)?),
        DbBackend::Duckdb => Box::new(DuckReader::new(&cli.db)?),
    };

    let compacts = reader.load_all_compact_mouse()?;
    if compacts.is_empty() {
        return Err(anyhow!("No compact mouse_traj events found"));
    }

    // pick indices
    let total = compacts.len();
    let mut selected: Vec<usize> = if let Some(idxs) = cli.indices.clone() {
        idxs
    } else if let Some(r) = cli.range.as_ref() {
        parse_range(r, total)
    } else {
        let n = cli.last.min(total);
        ((total - n)..total).collect()
    };
    if let Some(limit) = cli.limit {
        if selected.len() > limit {
            selected = selected.split_off(selected.len() - limit);
        }
    }

    // Prepare algorithms (names only; instances are created per task to avoid shared state)
    let algo_names = [
        "rdp",
        "radial",
        "reumann_witkam",
        "visvalingam",
        "bottom_up",
        "radial_s",
    ];

    // Results aggregation (add length and PASS counting)
    #[derive(Clone, Debug)]
    struct Metrics {
        name: String,
        pts_sum: usize,
        time_ms_sum: f64,
        mean_dev_sum: f32,
        max_dev_sum: f32,
        // Global-shape oriented aggregates:
        raw_len_sum: f64,  // sum of raw path lengths
        simp_len_sum: f64, // sum of simplified path lengths
        pass_cnt: usize,   // how many events meet PASS criteria
    }
    impl Metrics {
        // English comment: merge another Metrics into self
        fn merge(&mut self, other: &Metrics) {
            self.pts_sum += other.pts_sum;
            self.time_ms_sum += other.time_ms_sum;
            self.mean_dev_sum += other.mean_dev_sum;
            self.max_dev_sum += other.max_dev_sum;
            self.raw_len_sum += other.raw_len_sum;
            self.simp_len_sum += other.simp_len_sum;
            self.pass_cnt += other.pass_cnt;
        }
    }

    // ----- Phase 1: load all required raw data into memory (sequential, DB-friendly) -----
    let load_pb = ProgressBar::new(selected.len() as u64);
    load_pb.set_style(
        ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} load [{elapsed_precise}]")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut all_events: Vec<EventWork> = Vec::with_capacity(selected.len());
    for idx in &selected {
        let row = &compacts[*idx];
        let raws = reader.load_raw_mouse_for_link(row.raw_ref.as_ref())?;
        let raw_pts: Vec<MousePosition> = raws.into_iter().map(|p| p.pos).collect();
        if raw_pts.len() >= 3 {
            all_events.push(EventWork {
                idx: *idx,
                raw_pts,
                ref_path: row.payload.simplified_path.clone(),
            });
        }
        load_pb.inc(1);
    }
    load_pb.finish_with_message("loaded");

    // Early exit if nothing to process
    if all_events.is_empty() {
        println!("No events with >=3 raw points after selection.");
        return Ok(());
    }

    // ----- Phase 2: compute sequential in-memory -----
    let mut totals_map: HashMap<String, Metrics> = HashMap::new();
    let mut total_raw_pts: usize = 0;
    let mut total_events_processed: usize = 0;

    let comp_pb = ProgressBar::new(all_events.len() as u64);
    comp_pb.set_style(
        ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} compute [{elapsed_precise}]")
            .unwrap()
            .progress_chars("=>-"),
    );

    for ev in &all_events {
        let raw_pts = &ev.raw_pts;
        let ref_path = &ev.ref_path;

        // English comment: calibration reference
        let (ref_mean, _ref_max) = polyline_deviation_stats(raw_pts, ref_path);

        // Bound: approximate screen diagonal
        let (minx, miny, maxx, maxy) = bounds(raw_pts);
        let diag = (((maxx - minx) as f32).hypot((maxy - miny) as f32)).max(1.0);

        // English comment: make fresh algorithms for this event
        let mut make_algo = |name: &str| -> Box<dyn Simplifier> {
            match name {
                "rdp" => Box::new(AlgoRdp),
                "radial" => Box::new(AlgoRadial),
                "reumann_witkam" => Box::new(AlgoReumannWitkam),
                "visvalingam" => Box::new(AlgoVisvalingam),
                "bottom_up" => Box::new(AlgoBottomUp),
                "radial_s" => Box::new(AlgoStreamingRadial),
                _ => Box::new(AlgoRdp),
            }
        };

        for name in [
            "rdp",
            "radial",
            "reumann_witkam",
            "visvalingam",
            "bottom_up",
            "radial_s",
        ] {
            let mut algo = make_algo(name);

            // English comment: calibrate parameter
            let mut probe = |param: f32| {
                let s = algo.simplify(raw_pts, param);
                let (m, _) = polyline_deviation_stats(raw_pts, &s);
                m
            };
            let param = calibrate_param(ref_mean, &mut probe, 0.0, diag);

            // English comment: run and accumulate metrics
            let t0 = Instant::now();
            let simp = algo.simplify(raw_pts, param);
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

            let (mean_d, max_d) = polyline_deviation_stats(raw_pts, &simp);
            let raw_len = path_length(raw_pts) as f64;
            let simp_len = path_length(&simp) as f64;
            let drift_pct_this = if raw_len > 0.0 {
                ((simp_len - raw_len).abs() / raw_len) * 100.0
            } else {
                0.0
            };

            let e = totals_map.entry(name.to_string()).or_insert(Metrics {
                name: name.to_string(),
                pts_sum: 0,
                time_ms_sum: 0.0,
                mean_dev_sum: 0.0,
                max_dev_sum: 0.0,
                raw_len_sum: 0.0,
                simp_len_sum: 0.0,
                pass_cnt: 0,
            });
            e.pts_sum += simp.len();
            e.time_ms_sum += elapsed_ms;
            e.mean_dev_sum += mean_d;
            e.max_dev_sum += max_d;
            e.raw_len_sum += raw_len;
            e.simp_len_sum += simp_len;
            if mean_d <= cli.tau && drift_pct_this <= (cli.max_length_drift_pct as f64) {
                e.pass_cnt += 1;
            }
        }

        total_raw_pts += raw_pts.len();
        total_events_processed += 1;
        comp_pb.inc(1);
    }
    comp_pb.finish_with_message("done");

    // ----- Summary (readability-first scoreboard) -----
    let n_events = (total_events_processed).max(1) as f64;
    let total_raw_pts_f = total_raw_pts as f64;

    println!(
        "\nSummary over {} events (raw pts total={}):",
        total_events_processed, total_raw_pts
    );

    // Collect and sort by compression (higher raw/simp is better)
    let mut rows: Vec<Metrics> = totals_map.into_values().collect();
    rows.sort_by(|a, b| {
        // raw/simp = total_raw_pts / simp_pts_sum
        let inv_a = if a.pts_sum == 0 {
            f64::INFINITY
        } else {
            total_raw_pts_f / (a.pts_sum as f64)
        };
        let inv_b = if b.pts_sum == 0 {
            f64::INFINITY
        } else {
            total_raw_pts_f / (b.pts_sum as f64)
        };
        inv_b
            .partial_cmp(&inv_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Header
    println!(
        "{:<18} {:>11} {:>9} {:>9} {:>12} {:>9} {:>6}",
        "Algo", "raw/simp", "avg_pts", "mean(px)", "len_drift(%)", "us/pt", "PASS"
    );

    for v in rows {
        let avg_pts = v.pts_sum as f64 / n_events;
        let avg_mean = v.mean_dev_sum / (n_events as f32);
        let _avg_time_ms = v.time_ms_sum / n_events; // kept for potential future printing
        let us_per_pt = if total_raw_pts == 0 {
            0.0
        } else {
            (v.time_ms_sum * 1000.0) / total_raw_pts_f
        };
        let pass_rate = if total_events_processed == 0 {
            0.0
        } else {
            (v.pass_cnt as f64) * 100.0 / (total_events_processed as f64)
        };

        // Compression: raw/simplified
        let raw_over_simp = if v.pts_sum == 0 {
            f64::INFINITY
        } else {
            total_raw_pts_f / (v.pts_sum as f64)
        };

        // Length drift (aggregate): |sum(simp_len - raw_len)| / sum(raw_len)
        let avg_len_drift_pct = if v.raw_len_sum > 0.0 {
            ((v.simp_len_sum - v.raw_len_sum).abs() / v.raw_len_sum) * 100.0
        } else {
            0.0
        };

        // Streaming tag is determined by algorithm name
        let stream_tag = if v.name == "radial_s" {
            " (stream)"
        } else {
            ""
        };

        println!(
            "{:<18} {:>10.2}x {:>9.2} {:>9.3} {:>12.2} {:>9.2} {:>5.0}%",
            format!("{}{}", v.name, stream_tag),
            raw_over_simp,
            avg_pts,
            avg_mean,
            avg_len_drift_pct,
            us_per_pt,
            pass_rate
        );
    }

    println!(
        "\nPASS rule: mean <= {:.2}px AND length drift <= {:.1}%. Sorted by compression (raw/simp).",
        cli.tau, cli.max_length_drift_pct
    );

    Ok(())
}

fn parse_range(r: &str, total: usize) -> Vec<usize> {
    let parts: Vec<&str> = r.split(':').collect();
    if parts.len() != 2 {
        return vec![];
    }
    let start = parts[0].parse::<usize>().unwrap_or(0);
    let end = parts[1].parse::<usize>().unwrap_or(total.saturating_sub(1));
    (start..=end.min(total.saturating_sub(1))).collect()
}

fn bounds(pts: &[MousePosition]) -> (i32, i32, i32, i32) {
    let mut minx = i32::MAX;
    let mut miny = i32::MAX;
    let mut maxx = i32::MIN;
    let mut maxy = i32::MIN;
    for p in pts {
        minx = minx.min(p.x);
        miny = miny.min(p.y);
        maxx = maxx.max(p.x);
        maxy = maxy.max(p.y);
    }
    (minx, miny, maxx, maxy)
}
