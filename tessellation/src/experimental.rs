use std::mem;
use std::cmp::Ordering;

use {FillOptions, FillRule, Side};
use geom::math::*;
use geom::{LineSegment, QuadraticBezierSegment, CubicBezierSegment, Arc};
use geom::cubic_to_quadratic::cubic_to_monotonic_quadratics;
use geometry_builder::{GeometryBuilder, VertexId};
use std::ops::Range;
use path_fill::MonotoneTessellator;
use path::builder::*;
use path::PathEvent;
use path::default::{Path, PathSlice};
use std::{u16, u32, f32};
use std::env;
use std::mem::swap;

#[cfg(feature="debugger")]
use debugger::*;
#[cfg(feature="debugger")]
use path_fill::dbg;

pub type Vertex = Point;

macro_rules! tess_log {
    ($obj:ident, $fmt:expr) => (
        if $obj.log {
            println!($fmt);
        }
    );
    ($obj:ident, $fmt:expr, $($arg:tt)*) => (
        if $obj.log {
            println!($fmt, $($arg)*);
        }
    );
}

pub struct FillTessellator {
    current_position: Point,
    active: ActiveEdges,
    edges_below: Vec<PendingEdge>,
    fill_rule: FillRule,
    fill: Spans,
    log: bool,

    #[cfg(feature="debugger")]
    debugger: Option<Box<dyn Debugger2D>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Transition {
    In,
    Out,
    None,
}

#[derive(Copy, Clone, Debug)]
struct WindingState {
    span_index: SpanIdx,
    number: i16,
    transition: Transition,
}

impl FillRule {
    fn is_in(&self, winding_number: i16) -> bool {
        match *self {
            FillRule::EvenOdd => { winding_number % 2 != 0 }
            FillRule::NonZero => { winding_number != 0 }
        }
    }

    fn transition(&self, prev_winding: i16, new_winding: i16) -> Transition {
        match (self.is_in(prev_winding), self.is_in(new_winding)) {
            (false, true) => Transition::In,
            (true, false) => Transition::Out,
            _ => Transition::None,
        }
    }

    fn update_winding(&self, winding: &mut WindingState, edge_winding: i16) {
        let prev_winding_number = winding.number;
        winding.number += edge_winding;
        winding.transition = self.transition(prev_winding_number, winding.number);
        if winding.transition == Transition::In {
            winding.span_index += 1;
        }
    }
}

struct ActiveEdge {
    from: Point,
    to: Point,
    ctrl: Point,

    winding: i16,
    is_merge: bool,

    from_id: VertexId,
    ctrl_id: VertexId,
    to_id: VertexId,
}

struct ActiveEdges {
    edges: Vec<ActiveEdge>,
}

type SpanIdx = i32;

struct Span {
    tess: MonotoneTessellator,
    remove: bool,
}

struct Spans {
    spans: Vec<Span>,
}

impl Spans {
    fn begin_span(&mut self, span_idx: SpanIdx, position: &Point, vertex: VertexId) {
        let idx = span_idx as usize;
        self.spans.insert(
            idx,
            Span {
                tess: MonotoneTessellator::new().begin(*position, vertex),
                remove: false,
            }
        );
    }

    fn end_span(
        &mut self,
        span_idx: SpanIdx,
        position: &Point,
        id: VertexId,
        output: &mut dyn GeometryBuilder<Vertex>,
    ) {
        let idx = span_idx as usize;

        let span = &mut self.spans[idx];
        span.remove = true;
        span.tess.end(*position, id);
        span.tess.flush_experimental(output);
    }

    fn split_span(
        &mut self,
        span_idx: SpanIdx,
        split_position: &Point,
        split_id: VertexId,
        a_position: &Point,
        a_id: VertexId
    ) {
        let idx = span_idx as usize;

        //        /....
        // a --> x.....
        //      /.\....
        //     /...x... <-- current split vertex
        //    /.../ \..

        self.spans.insert(
            idx,
            Span {
                tess: MonotoneTessellator::new().begin(*a_position, a_id),
                remove: false,
            }
        );

        self.spans[idx].tess.vertex(*split_position, split_id, Side::Right);
        self.spans[idx + 1].tess.vertex(*split_position, split_id, Side::Left);
    }

    fn merge_spans(
        &mut self,
        span_idx: SpanIdx,
        current_position: &Point,
        current_vertex: VertexId,
        merge_position: &Point,
        merge_vertex: VertexId,
        output: &mut dyn GeometryBuilder<Vertex>,
    ) {
        //  \...\ /.
        //   \...x..  <-- merge vertex
        //    \./...  <-- active_edge
        //     x....  <-- current vertex

        let idx = span_idx as usize;
        if self.spans.len() <= idx + 1 {
            // TODO: we can only run into this if the order of the sweep line
            // is invalid. Need to re-sort it.
            return
        }

        self.spans[idx].tess.vertex(
            *merge_position,
            merge_vertex,
            Side::Right,
        );

        self.spans[idx + 1].tess.vertex(
            *merge_position,
            merge_vertex,
            Side::Left,
        );

        self.end_span(
            span_idx,
            current_position,
            current_vertex,
            output,
        );
    }

    fn cleanup_spans(&mut self) {
        // Get rid of the spans that were marked for removal.
        self.spans.retain(|span|{ !span.remove });
    }
}

#[derive(Debug)]
struct PendingEdge {
    to: Point,
    ctrl: Point,

    angle: f32,

    from_id: VertexId,
    ctrl_id: VertexId,
    to_id: VertexId,

    winding: i16,
}

impl ActiveEdge {
    fn solve_x_for_y(&self, y: f32) -> f32 {
        // TODO: curves.
        LineSegment {
            from: self.from,
            to: self.to,
        }.solve_x_for_y(y)
    }
}

impl FillTessellator {
    pub fn new() -> Self {
        FillTessellator {
            current_position: point(f32::MIN, f32::MIN),
            active: ActiveEdges {
                edges: Vec::new(),
            },
            edges_below: Vec::new(),
            fill_rule: FillRule::EvenOdd,
            fill: Spans {
                spans: Vec::new(),
            },
            log: env::var("LYON_FORCE_LOGGING").is_ok(),

            #[cfg(feature="debugger")]
            debugger: None,
        }
    }

    pub fn tessellate_path(
        &mut self,
        path: &Path,
        options: &FillOptions,
        builder: &mut dyn GeometryBuilder<Vertex>
    ) {
        self.fill_rule = options.fill_rule;

        let mut tx_builder = TraversalBuilder::with_capacity(128);
        tx_builder.set_path(path.as_slice());
        let (mut events, mut edge_data) = tx_builder.build();

        builder.begin_geometry();

        self.tessellator_loop(path, &mut events, &mut edge_data, builder);

        builder.end_geometry();

        //assert!(self.active.edges.is_empty());
        //assert!(self.fill.spans.is_empty());

        tess_log!(self, "\n ***************** \n");
    }

    pub fn enable_logging(&mut self) {
        self.log = true;
    }

    fn tessellator_loop(
        &mut self,
        path: &Path,
        events: &mut Traversal,
        edge_data: &[EdgeData],
        output: &mut dyn GeometryBuilder<Vertex>
    ) {
        let mut current_event = events.first_id();
        while events.valid_id(current_event) {
            self.current_position = events.position(current_event);
            let vertex_id = output.add_vertex(self.current_position);

            let mut current_sibling = current_event;
            while events.valid_id(current_sibling) {
                let edge = &edge_data[current_sibling];
                // We insert "fake" edges when there are end events
                // to make sure we process that vertex even if it has
                // no edge below.
                if edge.to == VertexId::INVALID {
                    current_sibling = events.next_sibling_id(current_sibling);
                    continue;
                }
                let to = path[edge.to];
                let ctrl = if edge.ctrl != VertexId::INVALID {
                    path[edge.ctrl]
                } else {
                    point(f32::NAN, f32::NAN)
                };
                self.edges_below.push(PendingEdge {
                    ctrl,
                    to,
                    angle: (to - self.current_position).angle_from_x_axis().radians,
                    // TODO: To use the real vertices in the Path we have to stop
                    // using GeometryBuilder::add_vertex.
                    //from_id: edge.from,
                    //ctrl_id: edge.ctrl,
                    //to_id: edge.to,
                    from_id: vertex_id,
                    ctrl_id: VertexId::INVALID,
                    to_id: VertexId::INVALID,

                    winding: edge.winding,
                });

                current_sibling = events.next_sibling_id(current_sibling);
            }

            self.process_events(
                vertex_id,
                output,
            );

            current_event = events.next_id(current_event);
        }
    }

    fn process_events(
        &mut self,
        current_vertex: VertexId,
        output: &mut dyn GeometryBuilder<Vertex>,
    ) {
        debug_assert!(!self.current_position.x.is_nan() && !self.current_position.y.is_nan());
        tess_log!(self, "\n --- events at [{}, {}] {:?}         {} edges below",
            self.current_position.x, self.current_position.y,
            current_vertex,
            self.edges_below.len(),
        );

        // The span index starts at -1 so that entering the first span (of index 0) increments
        // it to zero.
        let mut winding = WindingState {
            span_index: -1,
            number: 0,
            transition: Transition::None,
        };
        let mut winding_before_point: Option<WindingState> = None;
        let mut above = self.active.edges.len()..self.active.edges.len();
        let mut connecting_edges = false;
        let mut first_transition_above = true;
        let mut pending_merge = None;
        let mut pending_right = None;
        let mut prev_transition_in = None;

        let mut merges_to_resolve: Vec<(SpanIdx, usize)> = Vec::new();
        let mut spans_to_end = Vec::new();
        let mut edges_to_split = Vec::new();

        // First go through the sweep line and visit all edges that end at the
        // current position.

        // TODO: maybe split this loop in one that traverses the active edges until
        // the current point and a second one one that traverses the edges that
        // connect with the the current point.

        for (i, active_edge) in self.active.edges.iter_mut().enumerate() {
            // First deal with the merge case.
            if active_edge.is_merge {
                if connecting_edges {
                    merges_to_resolve.push((winding.span_index, i));
                    active_edge.to = self.current_position;
                    // This is probably not necessary but it's confusing to have the two
                    // not matching.
                    active_edge.to_id = current_vertex;
                    winding.span_index += 1;
                } else {
                    // \.....\ /...../
                    //  \.....x...../   <--- merge vertex
                    //   \....:..../
                    // ---\---:---/----  <-- sweep line
                    //     \..:../

                    // An unresolved merge vertex implies the left and right spans are
                    // adjacent and there is no transition between the two which means
                    // we need to bump the span index manually.
                    winding.span_index += 1;
                }

                continue;
            }

            // From there on we can assume the active edge is not a merge.

            let was_connecting_edges = connecting_edges;

            if points_are_equal(self.current_position, active_edge.to) {
                if !connecting_edges {
                    connecting_edges = true;
                }
            } else {
                let ex = active_edge.solve_x_for_y(self.current_position.y);
                tess_log!(self, "ex: {}", ex);

                if ex == self.current_position.x && !active_edge.is_merge {
                    tess_log!(self, " -- vertex on an edge!");
                    edges_to_split.push(i);

                    connecting_edges = true;
                }

                if ex > self.current_position.x {
                    above.end = i;
                    break;
                }
            }

            if !was_connecting_edges && connecting_edges {
                // We just started connecting edges above the current point.
                // Remember the current winding state because this is what we will
                // start from when handling the pending edges below the current point.
                winding_before_point = Some(winding.clone());
                above.start = i;
            }

            self.fill_rule.update_winding(&mut winding, active_edge.winding);

            tess_log!(self, "edge {} span {:?} transition {:?}", i, winding.span_index, winding.transition);

            if !connecting_edges {
                continue;
            }

            tess_log!(self, "{:?}", winding.transition);

            match (winding.transition, first_transition_above) {
                (Transition::In, _) => {
                    prev_transition_in = Some(i);
                }
                (Transition::Out, true) => {
                    if self.edges_below.is_empty() {
                        // Merge event.
                        pending_merge = Some(i);
                    } else {
                        // Right event.
                        pending_right = Some(i);
                    }
                }
                (Transition::Out, false) => {
                    let in_idx = prev_transition_in.unwrap();
                    tess_log!(self, " ** end ** edges: [{}, {}] span: {}",
                        in_idx, i,
                        winding.span_index
                    );

                    if winding.span_index < self.fill.spans.len() as i32 {
                        spans_to_end.push(winding.span_index);
                        winding.span_index += 1; // not sure
                    } else {
                        // error!
                    }
                }
                (Transition::None, _) => {}
            }

            if winding.transition != Transition::None {
                first_transition_above = false;
            }
        }

        for (span_index, edge_idx) in merges_to_resolve {
            //  \...\ /.
            //   \...x..  <-- merge vertex
            //    \./...  <-- active_edge
            //     x....  <-- current vertex
            let active_edge: &mut ActiveEdge = &mut self.active.edges[edge_idx];
            let merge_vertex: VertexId = active_edge.from_id;
            let merge_position = active_edge.from;
            //println!("merge vertex {:?} -> {:?}", merge_vertex, current_vertex);
            self.fill.merge_spans(
                span_index,
                &self.current_position,
                current_vertex,
                &merge_position,
                merge_vertex,
                output,
            );

            active_edge.is_merge = false;

            tess_log!(self, " Resolve merge event {} at {:?} ending span {}", edge_idx, active_edge.to, span_index);
            #[cfg(feature="debugger")]
            debugger_monotone_split(&self.debugger, &merge_position, &self.current_position);
        }

        for span_index in spans_to_end {
            self.fill.end_span(
                span_index,
                &self.current_position,
                current_vertex,
                output,
            );
        }

        self.fill.cleanup_spans();

        for edge_idx in edges_to_split {
            let to = self.active.edges[edge_idx].to;
            self.edges_below.push(PendingEdge {
                ctrl: point(f32::NAN, f32::NAN),
                to,

                angle: (to - self.current_position).angle_from_x_axis().radians,

                from_id: current_vertex,
                ctrl_id: VertexId::INVALID,
                to_id: self.active.edges[edge_idx].to_id,

                winding: self.active.edges[edge_idx].winding,
            });

            self.active.edges[edge_idx].to = self.current_position;
            self.active.edges[edge_idx].to_id = current_vertex;
        }

        // Fix up above index range in case there was no connecting edges.
        above.start = usize::min(above.start, above.end);

        winding = winding_before_point.unwrap_or(winding);

        tess_log!(self, "connecting edges: {}..{} {:?}", above.start, above.end, winding.transition);

        self.sort_edges_below();

        if let Some(in_idx) = pending_merge {
            // Merge event.
            //
            //  ...\   /...
            //  ....\ /....
            //  .....x.....
            //

            tess_log!(self, " ** merge ** edges: [{}, {}] span: {}",
                in_idx, above.end - 1,
                winding.span_index
            );

            let e = &mut self.active.edges[in_idx];
            e.is_merge = true;
            e.from = e.to;
            e.ctrl = e.to;
            e.winding = 0;
            e.from_id = current_vertex;
            e.ctrl_id = VertexId::INVALID;
            e.to_id = VertexId::INVALID;
        }

        // The range of pending edges below the current vertex to look at in the
        // last loop (not always the full range if we process split events).
        let mut below = 0..self.edges_below.len();

        if self.fill_rule.is_in(winding.number)
            && above.start == above.end
            && self.edges_below.len() >= 2 {

            // Split event.
            //
            //  ...........
            //  .....x.....
            //  ..../ \....
            //  .../   \...
            //

            let edge_above = above.start - 1;

            let upper_pos = self.active.edges[edge_above].from;
            let upper_id = self.active.edges[edge_above].from_id;
            tess_log!(self, " ** split ** edge {} span: {} upper {:?}", edge_above, winding.span_index, upper_pos);

            if self.active.edges[edge_above].is_merge {
                // Split vertex under a merge vertex
                //
                //  ...\ /...
                //  ....x....   <-- merge vertex (upper)
                //  ....:....
                //  ----x----   <-- current split vertex
                //  .../ \...
                //
                tess_log!(self, "   -> merge+split");
                let span_index = winding.span_index as usize;

                self.fill.spans[span_index - 1].tess.vertex(
                    upper_pos,
                    upper_id,
                    Side::Right,
                );
                self.fill.spans[span_index - 1].tess.vertex(
                    self.current_position,
                    current_vertex,
                    Side::Right,
                );

                self.fill.spans[span_index].tess.vertex(
                    upper_pos,
                    upper_id,
                    Side::Left,
                );
                self.fill.spans[span_index].tess.vertex(
                    self.current_position,
                    current_vertex,
                    Side::Left,
                );

                self.active.edges.remove(edge_above);
                above.start -= 1;
                above.end -= 1;
            } else {
                self.fill.split_span(
                    winding.span_index,
                    &self.current_position,
                    current_vertex,
                    &upper_pos,
                    upper_id,
                );
            }

            #[cfg(feature="debugger")]
            debugger_monotone_split(&self.debugger, &upper_pos, &self.current_position);

            winding.span_index += 1;

            below.start += 1;
            below.end -= 1;
        }

        // Go through the edges starting at the current point and emit
        // start events.

        let mut prev_transition_in = None;

        for i in below {
            let pending_edge = &self.edges_below[i];

            self.fill_rule.update_winding(&mut winding, pending_edge.winding);

            if let Some(idx) = pending_right {
                // Right event.
                //
                //  ..\
                //  ...x
                //  ../
                //
                debug_assert!(winding.transition == Transition::Out);
                tess_log!(self, " ** right ** edge: {} span: {}", idx, winding.span_index);

                self.fill.spans[winding.span_index as usize].tess.vertex(
                    self.current_position,
                    current_vertex,
                    Side::Right,
                );

                pending_right = None;

                continue;
            }

            match winding.transition {
                Transition::In => {
                    if i == self.edges_below.len() - 1 {
                        // Left event.
                        //
                        //     /...
                        //    x....
                        //     \...
                        //
                        tess_log!(self, " ** left ** edge {} span: {}", above.start, winding.span_index);

                        self.fill.spans[winding.span_index as usize].tess.vertex(
                            self.current_position,
                            current_vertex,
                            Side::Left,
                        );
                    } else {
                        prev_transition_in = Some(i);
                    }
                }
                Transition::Out => {
                    if let Some(in_idx) = prev_transition_in {

                        tess_log!(self, " ** start ** edges: [{}, {}] span: {}", in_idx, i, winding.span_index);

                        // Start event.
                        //
                        //      x
                        //     /.\
                        //    /...\
                        //

                        // TODO: if this is an intersection we must create a vertex
                        // and use it instead of the upper endpoint of the edge.
                        // TODO: we should use from_id but right now it points to the
                        // vertex in the path object and not the one we added with
                        // add_vertex
                        //let vertex = self.edges_below[in_idx].from_id;
                        let vertex = current_vertex;
                        tess_log!(self, " begin span {} ({})", winding.span_index, self.fill.spans.len());
                        self.fill.begin_span(
                            winding.span_index,
                            &self.current_position,
                            vertex
                        );
                    }
                }
                Transition::None => {}
            }
        }

        self.update_active_edges(above);

        tess_log!(self, "sweep line: {}", self.active.edges.len());
        for e in &self.active.edges {
            if e.is_merge {
                tess_log!(self, "| (merge) {}", e.from);
            } else {
                tess_log!(self, "| {} -> {}", e.from, e.to);
            }
        }
        tess_log!(self, "spans: {}", self.fill.spans.len());
    }

    fn update_active_edges(&mut self, above: Range<usize>) {
        // Remove all edges from the "above" range except merge
        // vertices.
        tess_log!(self, " remove {} edges ({}..{})", above.end - above.start, above.start, above.end);
        let mut rm_index = above.start;
        for _ in 0..(above.end - above.start) {
            if self.active.edges[rm_index].is_merge {
                rm_index += 1
            } else {
                self.active.edges.remove(rm_index);
            }
        }

        // Insert the pending edges.
        let from = self.current_position;
        let first_edge_below = above.start;
        for (i, edge) in self.edges_below.drain(..).enumerate() {
            let idx = first_edge_below + i;
            self.active.edges.insert(idx, ActiveEdge {
                from,
                to: edge.to,
                ctrl: edge.ctrl,
                winding: edge.winding,
                is_merge: false,
                from_id: edge.from_id,
                to_id: edge.to_id,
                ctrl_id: edge.ctrl_id,
            });
        }
    }

    fn sort_edges_below(&mut self) {
        // TODO: we'll need a better criterion than the tangent angle with quadratic béziers.
        self.edges_below.sort_by(|a, b| {
            b.angle.partial_cmp(&a.angle).unwrap_or(Ordering::Equal)
        });
    }

    #[cfg(feature="debugger")]
    pub fn install_debugger(&mut self, dbg: Box<dyn Debugger2D>) {
        self.debugger = Some(dbg)
    }

}

#[cfg(feature="debugger")]
fn debugger_monotone_split(debugger: &Option<Box<dyn Debugger2D>>, a: &Point, b: &Point) {
    if let Some(ref dbg) = debugger {
        dbg.edge(a, b, DARK_RED, dbg::MONOTONE_SPLIT);
    }
}

fn points_are_equal(a: Point, b: Point) -> bool {
    // TODO: Use the tolerance threshold.
    a == b
}


fn compare_positions(a: Point, b: Point) -> Ordering {
    if a.y > b.y {
        return Ordering::Greater;
    }
    if a.y < b.y {
        return Ordering::Less;
    }
    if a.x > b.x {
        return Ordering::Greater;
    }
    if a.x < b.x {
        return Ordering::Less;
    }
    return Ordering::Equal;
}

#[inline]
fn is_after(a: Point, b: Point) -> bool {
    a.y > b.y || (a.y == b.y && a.x > b.x)
}

pub struct TraversalEvent {
    next_sibling: usize,
    next_event: usize,
    position: Point,
}

#[derive(Clone, Debug)]
struct EdgeData {
    from: VertexId,
    ctrl: VertexId,
    to: VertexId,
    winding: i16,
}

pub struct Traversal {
    events: Vec<TraversalEvent>,
    first: usize,
    sorted: bool,
}

use std::usize;

impl Traversal {
    pub fn new() -> Self {
        Traversal {
            events: Vec::new(),
            first: 0,
            sorted: false,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Traversal {
            events: Vec::with_capacity(cap),
            first: 0,
            sorted: false,
        }
    }

    pub fn reserve(&mut self, n: usize) {
        self.events.reserve(n);
    }

    pub fn push(&mut self, position: Point) {
        let next_event = self.events.len() + 1;
        self.events.push(TraversalEvent {
            position,
            next_sibling: usize::MAX,
            next_event,
        });
        self.sorted = false;
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.first = 0;
        self.sorted = false;
    }

    pub fn first_id(&self) -> usize { self.first }

    pub fn next_id(&self, id: usize) -> usize { self.events[id].next_event }

    pub fn next_sibling_id(&self, id: usize) -> usize { self.events[id].next_sibling }

    pub fn valid_id(&self, id: usize) -> bool { id < self.events.len() }

    pub fn position(&self, id: usize) -> Point { self.events[id].position }

    pub fn sort(&mut self) {
        // This is more or less a bubble-sort, the main difference being that elements with the same
        // position are grouped in a "sibling" linked list.

        if self.sorted {
            return;
        }
        self.sorted = true;

        if self.events.len() <= 1 {
            return;
        }

        let mut current = 0;
        let mut prev = 0;
        let mut last = self.events.len() - 1;
        let mut swapped = false;

        #[cfg(test)]
        let mut iter_count = self.events.len() * self.events.len();

        loop {
            #[cfg(test)] {
                assert!(iter_count > 0);
                iter_count -= 1;
            }

            let rewind = current == last ||
                !self.valid_id(current) ||
                !self.valid_id(self.next_id(current));

            if rewind {
                last = prev;
                prev = self.first;
                current = self.first;
                if !swapped || last == self.first {
                    return;
                }
                swapped = false;
            }

            let next = self.next_id(current);
            let a = self.events[current].position;
            let b = self.events[next].position;
            match compare_positions(a, b) {
                Ordering::Less => {
                    // Already ordered.
                    prev = current;
                    current = next;
                }
                Ordering::Greater => {
                    // Need to swap current and next.
                    if prev != current && prev != next {
                        self.events[prev].next_event = next;
                    }
                    if current == self.first {
                        self.first = next;
                    }
                    if next == last {
                        last = current;
                    }
                    let next_next = self.next_id(next);
                    self.events[current].next_event = next_next;
                    self.events[next].next_event = current;
                    swapped = true;
                    prev = next;
                }
                Ordering::Equal => {
                    // Append next to current's sibling list.
                    let next_next = self.next_id(next);
                    self.events[current].next_event = next_next;
                    let mut current_sibling = current;
                    let mut next_sibling = self.next_sibling_id(current);
                    while self.valid_id(next_sibling) {
                        current_sibling = next_sibling;
                        next_sibling = self.next_sibling_id(current_sibling);
                    }
                    self.events[current_sibling].next_sibling = next;
                }
            }
        }
    }

    fn log(&self) {
        let mut iter_count = self.events.len() * self.events.len();

        println!("--");
        let mut current = self.first;
        while current < self.events.len() {
            assert!(iter_count > 0);
            iter_count -= 1;

            print!("[");
            let mut current_sibling = current;
            while current_sibling < self.events.len() {
                print!("{:?},", self.events[current_sibling].position);
                current_sibling = self.events[current_sibling].next_sibling;
            }
            print!("]  ");
            current = self.events[current].next_event;
        }
        println!("\n--");
    }

    fn assert_sorted(&self) {
        let mut current = self.first;
        let mut pos = point(f32::MIN, f32::MIN);
        while self.valid_id(current) {
            assert!(is_after(self.events[current].position, pos));
            pos = self.events[current].position;
            let mut current_sibling = current;
            while self.valid_id(current_sibling) {
                assert_eq!(self.events[current_sibling].position, pos);
                current_sibling = self.next_sibling_id(current_sibling);
            }
            current = self.next_id(current);
        }
    }
}

struct TraversalBuilder {
    current: Point,
    current_id: VertexId,
    first: Point,
    first_id: VertexId,
    prev: Point,
    second: Point,
    nth: u32,
    tx: Traversal,
    edge_data: Vec<EdgeData>,
}

impl TraversalBuilder {
    fn with_capacity(cap: usize) -> Self {
        TraversalBuilder {
            current: point(f32::NAN, f32::NAN),
            first: point(f32::NAN, f32::NAN),
            prev: point(f32::NAN, f32::NAN),
            second: point(f32::NAN, f32::NAN),
            current_id: VertexId::INVALID,
            first_id: VertexId::INVALID,
            nth: 0,
            tx: Traversal::with_capacity(cap),
            edge_data: Vec::with_capacity(cap),
        }
    }

    fn set_path(&mut self, path: PathSlice) {
        if path.is_empty() {
            return;
        }
        let mut cursor = path.start();
        loop {
            match path.event_at_cursor(cursor) {
                PathEvent::MoveTo(to) => {
                    self.move_to(to, cursor.vertex);
                }
                PathEvent::LineTo(to) => {
                    self.line_to(to, cursor.vertex);
                }
                PathEvent::QuadraticTo(ctrl, to) => {
                    self.quad_to(to, cursor.vertex, cursor.vertex + 1);
                }
                PathEvent::Close => {
                    self.close();
                }
                _ => { unimplemented!(); }
            }

            if let Some(next) = path.next_cursor(cursor) {
                cursor = next;
            } else {
                break;
            }
        }
        self.close();
    }

    fn vertex_event(&mut self, at: Point) {
        self.tx.push(at);
        self.edge_data.push(EdgeData {
            from: VertexId::INVALID,
            ctrl: VertexId::INVALID,
            to: VertexId::INVALID,
            winding: 0,
        });
    }

    fn close(&mut self) {
        if self.nth == 0 {
            return;
        }

        // Unless we are already back to the first point we no need to
        // to insert an edge.
        let first = self.first;
        if self.current != self.first {
            let first_id = self.first_id;
            self.line_to(first, first_id)
        }

        // Since we can only check for the need of a vertex event when
        // we have a previous edge, we skipped it for the first edge
        // and have to do it now.
        if is_after(self.first, self.prev) && is_after(self.first, self.second) {
            self.vertex_event(first);
        }

        self.nth = 0;
    }

    fn move_to(&mut self, to: Point, to_id: VertexId) {
        if self.nth > 0 {
            self.close();
        }

        self.nth = 0;
        self.first = to;
        self.current = to;
        self.first_id = to_id;
        self.current_id = to_id;
    }

    fn line_to(&mut self, to: Point, to_id: VertexId) {
        self.quad_to(to, VertexId::INVALID, to_id);
    }

    fn quad_to(&mut self, to: Point, ctrl_id: VertexId, mut to_id: VertexId) {
        if self.current == to {
            return;
        }

        let next_id = to_id;
        let mut from = self.current;
        let mut from_id = self.current_id;
        let mut winding = 1;
        if is_after(from, to) {
            if self.nth > 0 && is_after(from, self.prev) {
                self.vertex_event(from);
            }

            from = to;
            swap(&mut from_id, &mut to_id);
            winding = -1;
        }

        //println!("Edge {:?}/{:?} {:?} ->", from_id, to_id, from);
        debug_assert!(from_id != VertexId::INVALID);
        debug_assert!(to_id != VertexId::INVALID);
        self.tx.push(from);
        self.edge_data.push(EdgeData {
            from: from_id,
            ctrl: ctrl_id,
            to: to_id,
            winding,
        });

        if self.nth == 0 {
            self.second = to;
        }

        self.nth += 1;
        self.prev = self.current;
        self.current = to;
        self.current_id = next_id;
    }

    fn build(mut self) -> (Traversal, Vec<EdgeData>) {
        self.close();
        self.tx.sort();

        (self.tx, self.edge_data)
    }
}

#[test]
fn test_traversal_sort_1() {
    let mut tx = Traversal::new();
    tx.push(point(0.0, 0.0));
    tx.push(point(4.0, 0.0));
    tx.push(point(2.0, 0.0));
    tx.push(point(3.0, 0.0));
    tx.push(point(4.0, 0.0));
    tx.push(point(0.0, 0.0));
    tx.push(point(6.0, 0.0));

    tx.sort();
    tx.assert_sorted();
}

#[test]
fn test_traversal_sort_2() {
    let mut tx = Traversal::new();
    tx.push(point(0.0, 0.0));
    tx.push(point(0.0, 0.0));
    tx.push(point(0.0, 0.0));
    tx.push(point(0.0, 0.0));

    tx.sort();
    tx.assert_sorted();
}

#[test]
fn test_traversal_sort_3() {
    let mut tx = Traversal::new();
    tx.push(point(0.0, 0.0));
    tx.push(point(1.0, 0.0));
    tx.push(point(2.0, 0.0));
    tx.push(point(3.0, 0.0));
    tx.push(point(4.0, 0.0));
    tx.push(point(5.0, 0.0));

    tx.sort();
    tx.assert_sorted();
}

#[test]
fn test_traversal_sort_4() {
    let mut tx = Traversal::new();
    tx.push(point(5.0, 0.0));
    tx.push(point(4.0, 0.0));
    tx.push(point(3.0, 0.0));
    tx.push(point(2.0, 0.0));
    tx.push(point(1.0, 0.0));
    tx.push(point(0.0, 0.0));

    tx.sort();
    tx.assert_sorted();
}

#[test]
fn test_traversal_sort_5() {
    let mut tx = Traversal::new();
    tx.push(point(5.0, 0.0));
    tx.push(point(5.0, 0.0));
    tx.push(point(4.0, 0.0));
    tx.push(point(4.0, 0.0));
    tx.push(point(3.0, 0.0));
    tx.push(point(3.0, 0.0));
    tx.push(point(2.0, 0.0));
    tx.push(point(2.0, 0.0));
    tx.push(point(1.0, 0.0));
    tx.push(point(1.0, 0.0));
    tx.push(point(0.0, 0.0));
    tx.push(point(0.0, 0.0));

    tx.sort();
    tx.assert_sorted();
}

#[cfg(test)]
use geometry_builder::{VertexBuffers, simple_builder};

#[test]
fn new_tess_triangle() {
    let mut builder = Path::builder();
    builder.move_to(point(0.0, 0.0));
    builder.line_to(point(5.0, 1.0));
    builder.line_to(point(3.0, 5.0));
    builder.close();

    let path = builder.build();

    let mut tess = FillTessellator::new();
    tess.enable_logging();

    let mut buffers: VertexBuffers<Vertex, u16> = VertexBuffers::new();

    tess.tessellate_path(
        &path,
        &FillOptions::default(),
        &mut simple_builder(&mut buffers),
    );
}

#[test]
fn new_tess0() {
    let mut builder = Path::builder();
    builder.move_to(point(0.0, 0.0));
    builder.line_to(point(5.0, 0.0));
    builder.line_to(point(5.0, 5.0));
    builder.line_to(point(0.0, 5.0));
    builder.close();
    builder.move_to(point(1.0, 1.0));
    builder.line_to(point(4.0, 1.0));
    builder.line_to(point(4.0, 4.0));
    builder.line_to(point(1.0, 4.0));
    builder.close();

    let path = builder.build();

    let mut tess = FillTessellator::new();

    let mut buffers: VertexBuffers<Vertex, u16> = VertexBuffers::new();

    tess.tessellate_path(
        &path,
        &FillOptions::default(),
        &mut simple_builder(&mut buffers),
    );
}

#[test]
fn new_tess1() {

    let mut builder = Path::builder();
    builder.move_to(point(0.0, 0.0));
    builder.line_to(point(5.0, -5.0));
    builder.line_to(point(10.0, 0.0));
    builder.line_to(point(9.0, 5.0));
    builder.line_to(point(10.0, 10.0));
    builder.line_to(point(5.0, 6.0));
    builder.line_to(point(0.0, 10.0));
    builder.line_to(point(1.0, 5.0));
    builder.close();

    builder.move_to(point(20.0, -1.0));
    builder.line_to(point(25.0, 1.0));
    builder.line_to(point(25.0, 9.0));
    builder.close();


    let path = builder.build();

    let mut tess = FillTessellator::new();

    let mut buffers: VertexBuffers<Vertex, u16> = VertexBuffers::new();

    tess.tessellate_path(
        &path,
        &FillOptions::default(),
        &mut simple_builder(&mut buffers),
    );
}

#[test]
fn new_tess_merge() {

    let mut builder = Path::builder();
    builder.move_to(point(0.0, 0.0));  // start
    builder.line_to(point(5.0, 5.0));  // merge
    builder.line_to(point(5.0, 1.0));  // start
    builder.line_to(point(10.0, 6.0)); // merge
    builder.line_to(point(11.0, 2.0)); // start
    builder.line_to(point(11.0, 10.0));// end
    builder.line_to(point(0.0, 9.0));  // left
    builder.close();

    let path = builder.build();

    let mut tess = FillTessellator::new();

    let mut buffers: VertexBuffers<Vertex, u16> = VertexBuffers::new();

    tess.tessellate_path(
        &path,
        &FillOptions::default(),
        &mut simple_builder(&mut buffers),
    );

    // "M 0 0 L 5 5 L 5 1 L 10 6 L 11 2 L 11 10 L 0 9 Z"
}

// cargo run --features=experimental -- show "M 0 0 L 1 1 0 2 Z M 2 0 1 1 2 2 Z" --tessellator experimental -fs
