use crate::daemon::events::{MousePosition, RawEvent, WheelAxis};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

use kronical_core::compression::{
    self, CompactEvent, CompactFocusEvent, CompactKeyboardActivity, CompactMouseTrajectory,
    CompactScrollSequence, MouseTrajectoryType, ScrollDirection, StringId,
};

#[derive(Debug, Clone)]
pub struct StringInterner {
    strings: Vec<String>,
    lookup: HashMap<String, StringId>,
    max_strings: usize,
}

impl StringInterner {
    pub fn new() -> Self {
        Self::with_cap(4096)
    }

    pub fn with_cap(max_strings: usize) -> Self {
        Self {
            strings: Vec::new(),
            lookup: HashMap::new(),
            max_strings: max_strings.max(1),
        }
    }

    pub fn intern(&mut self, s: &str) -> StringId {
        if let Some(id) = self.lookup.get(s) {
            return *id;
        }
        // Reset (cheap) when capacity exceeded; IDs restart. Safe for our current usage.
        if self.strings.len() >= self.max_strings {
            self.strings.clear();
            self.lookup.clear();
            self.strings.shrink_to_fit();
            self.lookup.shrink_to_fit();
        }
        let id = self.strings.len() as StringId;
        self.strings.push(s.to_string());
        self.lookup.insert(s.to_string(), id);
        id
    }
}

pub trait EventCompressor {
    type Input;
    type Output;

    fn name(&self) -> &'static str;
    fn can_compress(&self, events: &[Self::Input]) -> bool;
    fn compress(&mut self, events: Vec<Self::Input>) -> Vec<Self::Output>;
}

pub struct ScrollCompressor {
    max_gap_ms: i64,
}

impl ScrollCompressor {
    pub fn new() -> Self {
        Self {
            max_gap_ms: 500, // 500ms max gap between scroll events to group them
        }
    }

    fn extract_scroll_events(
        &self,
        events: &[RawEvent],
    ) -> Vec<(u64, DateTime<Utc>, MousePosition, i32, i32, WheelAxis)> {
        events
            .iter()
            .filter_map(|event| {
                if let RawEvent::MouseInput {
                    timestamp,
                    event_id,
                    data,
                } = event
                {
                    // Only accept events that came from uiohook WheelEvent with explicit fields
                    if let (Some(amount), Some(rotation), Some(axis)) =
                        (data.wheel_amount, data.wheel_rotation, data.wheel_axis)
                    {
                        let signed_amount = if rotation >= 0 { amount } else { -amount };
                        Some((
                            *event_id,
                            *timestamp,
                            data.position.clone(),
                            signed_amount,
                            rotation,
                            axis,
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    fn determine_direction(&self, signed_amount: i32, axis: WheelAxis) -> ScrollDirection {
        compression::infer_scroll_direction(signed_amount, axis)
    }
}

impl EventCompressor for ScrollCompressor {
    type Input = RawEvent;
    type Output = CompactScrollSequence;

    fn can_compress(&self, events: &[Self::Input]) -> bool {
        self.extract_scroll_events(events).len() > 1
    }

    fn compress(&mut self, events: Vec<Self::Input>) -> Vec<Self::Output> {
        let scroll_events = self.extract_scroll_events(&events);
        if scroll_events.is_empty() {
            return Vec::new();
        }

        let mut sequences = Vec::new();
        let mut current_sequence: Option<CompactScrollSequence> = None;

        for (event_id, timestamp, position, signed_amount, rotation, axis) in scroll_events {
            let scroll_direction = self.determine_direction(signed_amount, axis);

            match &mut current_sequence {
                Some(seq)
                    if seq.direction == scroll_direction
                        && (timestamp - seq.end_time).num_milliseconds() < self.max_gap_ms =>
                {
                    // Extend current sequence
                    seq.end_time = timestamp;
                    seq.total_amount += signed_amount as i32;
                    seq.total_rotation += rotation;
                    seq.scroll_count += 1;
                    seq.raw_event_ids.push(event_id);
                }
                _ => {
                    // Start new sequence
                    if let Some(seq) = current_sequence.take() {
                        sequences.push(seq);
                    }
                    current_sequence = Some(CompactScrollSequence {
                        start_time: timestamp,
                        end_time: timestamp,
                        direction: scroll_direction,
                        total_amount: signed_amount as i32,
                        total_rotation: rotation,
                        scroll_count: 1,
                        position,
                        raw_event_ids: vec![event_id],
                    });
                }
            }
        }

        if let Some(seq) = current_sequence {
            sequences.push(seq);
        }

        sequences
    }

    fn name(&self) -> &'static str {
        "ScrollCompressor"
    }
}

pub struct MouseTrajectoryCompressor {
    max_gap_ms: i64,
    min_distance: f32,
    douglas_peucker_epsilon: f32,
}

impl MouseTrajectoryCompressor {
    pub fn new() -> Self {
        Self {
            max_gap_ms: 200,              // 200ms max gap
            min_distance: 5.0,            // Minimum 5 pixel movement
            douglas_peucker_epsilon: 2.0, // 2 pixel tolerance for simplification
        }
    }

    fn extract_mouse_events(
        &self,
        events: &[RawEvent],
    ) -> Vec<(u64, DateTime<Utc>, MousePosition, MouseTrajectoryType)> {
        events
            .iter()
            .filter_map(|event| {
                if let RawEvent::MouseInput {
                    timestamp,
                    event_id,
                    data,
                } = event
                {
                    // Only include movement/dragged events; ignore clicks/press/release
                    if let Some(kind) = data.event_type {
                        match kind {
                            crate::daemon::events::MouseEventKind::Moved => Some((
                                *event_id,
                                *timestamp,
                                data.position.clone(),
                                MouseTrajectoryType::Movement,
                            )),
                            crate::daemon::events::MouseEventKind::Dragged => Some((
                                *event_id,
                                *timestamp,
                                data.position.clone(),
                                MouseTrajectoryType::Drag,
                            )),
                            _ => None,
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    fn calculate_distance(&self, p1: &MousePosition, p2: &MousePosition) -> f32 {
        let dx = (p2.x - p1.x) as f32;
        let dy = (p2.y - p1.y) as f32;
        (dx * dx + dy * dy).sqrt()
    }

    fn douglas_peucker(&self, points: &[MousePosition], epsilon: f32) -> Vec<MousePosition> {
        if points.len() <= 2 {
            return points.to_vec();
        }

        let mut max_distance = 0.0;
        let mut max_index = 0;

        for i in 1..points.len() - 1 {
            let distance =
                self.perpendicular_distance(&points[i], &points[0], &points[points.len() - 1]);
            if distance > max_distance {
                max_distance = distance;
                max_index = i;
            }
        }

        if max_distance > epsilon {
            let mut left = self.douglas_peucker(&points[0..=max_index], epsilon);
            let right = self.douglas_peucker(&points[max_index..], epsilon);
            left.pop();
            left.extend(right);
            left
        } else {
            vec![points[0].clone(), points[points.len() - 1].clone()]
        }
    }

    fn perpendicular_distance(
        &self,
        point: &MousePosition,
        line_start: &MousePosition,
        line_end: &MousePosition,
    ) -> f32 {
        let a = (line_end.y - line_start.y) as f32;
        let b = (line_start.x - line_end.x) as f32;
        let c = (line_end.x * line_start.y - line_start.x * line_end.y) as f32;

        let numerator = (a * point.x as f32 + b * point.y as f32 + c).abs();
        let denominator = (a * a + b * b).sqrt();

        if denominator == 0.0 {
            0.0
        } else {
            numerator / denominator
        }
    }

    fn calculate_path_distance(&self, positions: &[MousePosition]) -> f32 {
        positions
            .windows(2)
            .map(|pair| self.calculate_distance(&pair[0], &pair[1]))
            .sum()
    }

    fn calculate_max_velocity(&self, path: &[(DateTime<Utc>, MousePosition)]) -> f32 {
        path.windows(2)
            .map(|pair| {
                let distance = self.calculate_distance(&pair[0].1, &pair[1].1);
                let time_ms = (pair[1].0 - pair[0].0).num_milliseconds().max(1) as f32;
                distance / time_ms * 1000.0 // pixels per second
            })
            .fold(0.0, f32::max)
    }
}

impl EventCompressor for MouseTrajectoryCompressor {
    type Input = RawEvent;
    type Output = CompactMouseTrajectory;

    fn can_compress(&self, events: &[Self::Input]) -> bool {
        self.extract_mouse_events(events).len() > 2
    }

    fn compress(&mut self, events: Vec<Self::Input>) -> Vec<Self::Output> {
        let mouse_events = self.extract_mouse_events(&events);
        if mouse_events.len() < 3 {
            return Vec::new();
        }

        let mut trajectories = Vec::new();
        let mut current_path: Vec<(u64, DateTime<Utc>, MousePosition)> = Vec::new();
        let mut current_type: Option<MouseTrajectoryType> = None;

        for (event_id, timestamp, position, trajectory_type) in mouse_events {
            if let Some((_, last_time, last_pos)) = current_path.last() {
                let time_gap = (timestamp - *last_time).num_milliseconds();
                let distance = self.calculate_distance(last_pos, &position);
                let type_changed = current_type.is_some_and(|t| t != trajectory_type);

                if time_gap > self.max_gap_ms || distance < self.min_distance || type_changed {
                    // Finalize current trajectory
                    if current_path.len() > 2 {
                        trajectories.push(
                            self.build_trajectory(current_path.clone(), current_type.unwrap()),
                        );
                    }
                    current_path.clear();
                }
            }

            current_path.push((event_id, timestamp, position));
            current_type = Some(trajectory_type);
        }

        // Finalize last trajectory
        if current_path.len() > 2 {
            trajectories.push(self.build_trajectory(current_path, current_type.unwrap()));
        }

        trajectories
    }

    fn name(&self) -> &'static str {
        "MouseTrajectoryCompressor"
    }
}

impl MouseTrajectoryCompressor {
    fn build_trajectory(
        &self,
        path: Vec<(u64, DateTime<Utc>, MousePosition)>,
        trajectory_type: MouseTrajectoryType,
    ) -> CompactMouseTrajectory {
        let start_time = path.first().unwrap().1;
        let end_time = path.last().unwrap().1;
        let start_position = path.first().unwrap().2.clone();
        let end_position = path.last().unwrap().2.clone();

        let positions: Vec<MousePosition> = path.iter().map(|(_, _, pos)| pos.clone()).collect();
        let simplified_path = self.douglas_peucker(&positions, self.douglas_peucker_epsilon);
        let total_distance = self.calculate_path_distance(&positions);
        let path_ts_pos: Vec<(DateTime<Utc>, MousePosition)> =
            path.iter().map(|(_, ts, pos)| (*ts, pos.clone())).collect();
        let max_velocity = self.calculate_max_velocity(&path_ts_pos);
        let raw_event_ids: Vec<u64> = path.into_iter().map(|(id, _, _)| id).collect();

        CompactMouseTrajectory {
            start_time,
            end_time,
            event_type: trajectory_type,
            start_position,
            end_position,
            simplified_path,
            total_distance,
            max_velocity,
            raw_event_ids,
        }
    }
}

pub struct KeyboardActivityCompressor {
    max_gap_ms: i64,
}

impl KeyboardActivityCompressor {
    pub fn new() -> Self {
        // Default gap to group bursts of typing (30s)
        Self { max_gap_ms: 30_000 }
    }

    #[allow(dead_code)]
    pub fn with_gap_ms(max_gap_ms: i64) -> Self {
        Self { max_gap_ms }
    }

    fn extract_keyboard_events(&self, events: &[RawEvent]) -> Vec<(u64, DateTime<Utc>)> {
        events
            .iter()
            .filter_map(|event| match event {
                RawEvent::KeyboardInput {
                    timestamp,
                    event_id,
                    ..
                } => Some((*event_id, *timestamp)),
                _ => None,
            })
            .collect()
    }
}

impl EventCompressor for KeyboardActivityCompressor {
    type Input = RawEvent;
    type Output = CompactKeyboardActivity;

    fn can_compress(&self, events: &[Self::Input]) -> bool {
        events
            .iter()
            .any(|e| matches!(e, RawEvent::KeyboardInput { .. }))
    }

    fn compress(&mut self, events: Vec<Self::Input>) -> Vec<Self::Output> {
        let mut keys: Vec<(u64, DateTime<Utc>)> = self.extract_keyboard_events(&events);
        if keys.is_empty() {
            return Vec::new();
        }
        // Ensure temporal order
        keys.sort_by_key(|(_, ts)| *ts);

        let mut out = Vec::new();
        let mut group_ids: Vec<u64> = Vec::new();
        let mut group_start = keys[0].1;
        let mut group_end = keys[0].1;
        group_ids.push(keys[0].0);

        for (id, ts) in keys.into_iter().skip(1) {
            let gap = (ts - group_end).num_milliseconds();
            if gap <= self.max_gap_ms {
                // Continue group
                group_end = ts;
                group_ids.push(id);
            } else {
                // Flush current group
                let duration_secs = (group_end - group_start).num_seconds().max(1) as f32;
                let keystrokes = group_ids.len() as u32;
                let kpm = (keystrokes as f32) / (duration_secs / 60.0);
                let density = (keystrokes as f32) / duration_secs; // keys per second
                out.push(CompactKeyboardActivity {
                    start_time: group_start,
                    end_time: group_end,
                    keystrokes,
                    keys_per_minute: kpm,
                    density_per_sec: density,
                    raw_event_ids: std::mem::take(&mut group_ids),
                });

                // Start new group
                group_start = ts;
                group_end = ts;
                group_ids.push(id);
            }
        }
        // Flush final group
        let duration_secs = (group_end - group_start).num_seconds().max(1) as f32;
        let keystrokes = group_ids.len() as u32;
        let kpm = (keystrokes as f32) / (duration_secs / 60.0);
        let density = (keystrokes as f32) / duration_secs;
        out.push(CompactKeyboardActivity {
            start_time: group_start,
            end_time: group_end,
            keystrokes,
            keys_per_minute: kpm,
            density_per_sec: density,
            raw_event_ids: group_ids,
        });

        out
    }

    fn name(&self) -> &'static str {
        "KeyboardActivityCompressor"
    }
}

pub struct FocusEventProcessor {
    string_interner: StringInterner,
}

impl FocusEventProcessor {
    pub fn new() -> Self {
        Self {
            string_interner: StringInterner::new(),
        }
    }

    pub fn with_cap(max_strings: usize) -> Self {
        Self {
            string_interner: StringInterner::with_cap(max_strings),
        }
    }

    pub fn get_string_interner(&self) -> &StringInterner {
        &self.string_interner
    }
}

impl EventCompressor for FocusEventProcessor {
    type Input = RawEvent;
    type Output = CompactFocusEvent;

    fn can_compress(&self, events: &[Self::Input]) -> bool {
        events
            .iter()
            .any(|e| matches!(e, RawEvent::WindowFocusChange { .. }))
    }

    fn compress(&mut self, events: Vec<Self::Input>) -> Vec<Self::Output> {
        events
            .into_iter()
            .filter_map(|event| {
                if let RawEvent::WindowFocusChange {
                    timestamp,
                    event_id,
                    focus_info,
                } = event
                {
                    let app_name_id = self.string_interner.intern(&focus_info.app_name);
                    let window_title_id = self.string_interner.intern(&focus_info.window_title);

                    Some(CompactFocusEvent {
                        timestamp,
                        app_name_id,
                        window_title_id,
                        pid: focus_info.pid,
                        window_position: focus_info.window_position,
                        event_id,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    fn name(&self) -> &'static str {
        "FocusEventProcessor"
    }
}

pub struct CompressionEngine {
    scroll_compressor: ScrollCompressor,
    mouse_compressor: MouseTrajectoryCompressor,
    keyboard_compressor: KeyboardActivityCompressor,
    focus_processor: FocusEventProcessor,
    total_compact_events: usize,
    total_compact_events_bytes: usize,
}

impl CompressionEngine {
    pub fn new() -> Self {
        Self::with_focus_cap(4096)
    }

    pub fn with_focus_cap(max_strings: usize) -> Self {
        Self {
            scroll_compressor: ScrollCompressor::new(),
            mouse_compressor: MouseTrajectoryCompressor::new(),
            keyboard_compressor: KeyboardActivityCompressor::new(),
            focus_processor: FocusEventProcessor::with_cap(max_strings),
            total_compact_events: 0,
            total_compact_events_bytes: 0,
        }
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub fn compress_events(
        &mut self,
        raw_events: Vec<RawEvent>,
    ) -> Result<(Vec<RawEvent>, Vec<CompactEvent>)> {
        if raw_events.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let mut compact_events = Vec::new();

        if self.scroll_compressor.can_compress(&raw_events) {
            let scroll_sequences = self.scroll_compressor.compress(raw_events.clone());
            for seq in scroll_sequences {
                compact_events.push(CompactEvent::Scroll(seq));
            }
        }

        if self.mouse_compressor.can_compress(&raw_events) {
            let trajectories = self.mouse_compressor.compress(raw_events.clone());
            for trajectory in trajectories {
                compact_events.push(CompactEvent::MouseTrajectory(trajectory));
            }
        }

        if self.keyboard_compressor.can_compress(&raw_events) {
            let keyboard_events = self.keyboard_compressor.compress(raw_events.clone());
            for kb_event in keyboard_events {
                compact_events.push(CompactEvent::Keyboard(kb_event));
            }
        }

        if self.focus_processor.can_compress(&raw_events) {
            let focus_events = self.focus_processor.compress(raw_events.clone());
            for focus_event in focus_events {
                compact_events.push(CompactEvent::Focus(focus_event));
            }
        }

        for event in &compact_events {
            self.total_compact_events_bytes += event.memory_size();
        }
        self.total_compact_events += compact_events.len();

        Ok((raw_events, compact_events))
    }
}
