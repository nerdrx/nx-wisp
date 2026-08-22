//! Vector paths: a verb list plus a flat point array.
//!
//! The split matters for deformation. Skinning transforms *every* point —
//! anchors and Bézier control points alike — so the verbs never change and the
//! point array is the only thing written per frame. That is what keeps the
//! per-frame path allocation-free (F71) and what lets a curve bend with a bone
//! instead of being re-fitted.
//!
//! Path data is authored as an SVG-subset string (`M L H V C Q Z`, absolute and
//! relative). It is *data*, not code: there is no arithmetic, no reference to
//! anything outside the string, and nothing a skin can express here can escape
//! into behaviour (SPEC §3.6).

use crate::math::{Rect, Vec2};

/// What each verb consumes from the point array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// 1 point — starts a new subpath.
    Move,
    /// 1 point.
    Line,
    /// 2 points — control, end.
    Quad,
    /// 3 points — control 1, control 2, end.
    Cubic,
    /// 0 points — closes the current subpath.
    Close,
}

impl Verb {
    #[inline]
    pub fn point_count(self) -> usize {
        match self {
            Verb::Move | Verb::Line => 1,
            Verb::Quad => 2,
            Verb::Cubic => 3,
            Verb::Close => 0,
        }
    }
    pub fn letter(self) -> char {
        match self {
            Verb::Move => 'M',
            Verb::Line => 'L',
            Verb::Quad => 'Q',
            Verb::Cubic => 'C',
            Verb::Close => 'Z',
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    pub verbs: Vec<Verb>,
    pub points: Vec<Vec2>,
}

impl Path {
    pub fn new() -> Self {
        Path::default()
    }

    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    pub fn move_to(&mut self, p: Vec2) -> &mut Self {
        self.verbs.push(Verb::Move);
        self.points.push(p);
        self
    }
    pub fn line_to(&mut self, p: Vec2) -> &mut Self {
        self.verbs.push(Verb::Line);
        self.points.push(p);
        self
    }
    pub fn quad_to(&mut self, c: Vec2, p: Vec2) -> &mut Self {
        self.verbs.push(Verb::Quad);
        self.points.push(c);
        self.points.push(p);
        self
    }
    pub fn cubic_to(&mut self, c1: Vec2, c2: Vec2, p: Vec2) -> &mut Self {
        self.verbs.push(Verb::Cubic);
        self.points.push(c1);
        self.points.push(c2);
        self.points.push(p);
        self
    }
    pub fn close(&mut self) -> &mut Self {
        self.verbs.push(Verb::Close);
        self
    }

    pub fn bounds(&self) -> Rect {
        let mut r = Rect::EMPTY;
        for p in &self.points {
            r.union_point(*p);
        }
        r
    }

    /// Number of points a deformed copy of this path needs.
    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    /// Walk the path as `(Verb, &[Vec2])` pairs against a supplied point
    /// array. The array is usually the *deformed* points, not `self.points`,
    /// which is exactly why this takes it as an argument.
    pub fn segments<'a>(&'a self, points: &'a [Vec2]) -> Segments<'a> {
        Segments { verbs: &self.verbs, points, vi: 0, pi: 0 }
    }

    /// Flatten to polylines — one `Vec<Vec2>` per subpath — using a fixed
    /// number of samples per curve. Used by the contour tracer and by hit
    /// testing; the renderer flattens curves itself.
    ///
    /// `out` is cleared and reused so this stays off the allocation path when
    /// called every frame.
    pub fn flatten_into(&self, points: &[Vec2], per_curve: usize, out: &mut Vec<Vec<Vec2>>) {
        flatten_into(&self.verbs, points, per_curve, out)
    }

    /// Convenience wrapper around [`Path::flatten_into`] for tests and setup.
    pub fn flatten(&self, per_curve: usize) -> Vec<Vec<Vec2>> {
        let mut out = Vec::new();
        self.flatten_into(&self.points, per_curve, &mut out);
        out
    }

    /// Parse the SVG path subset. See [`parse_path`].
    pub fn parse(s: &str) -> Result<Path, PathError> {
        parse_path(s)
    }

    /// Render back to the same subset, for round-tripping a skin file.
    pub fn to_svg(&self) -> String {
        let mut s = String::new();
        for (verb, pts) in self.segments(&self.points) {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push(verb.letter());
            for p in pts {
                s.push_str(&format!(" {} {}", fmt(p.x), fmt(p.y)));
            }
        }
        s
    }
}

/// Walk a verb list against any point array. The frame's deformed shapes hold
/// verbs and points separately, so this is the form they use.
pub fn segments<'a>(verbs: &'a [Verb], points: &'a [Vec2]) -> Segments<'a> {
    Segments { verbs, points, vi: 0, pi: 0 }
}

/// Flatten a verb list against any point array. See [`Path::flatten_into`].
pub fn flatten_into(
    verbs: &[Verb],
    points: &[Vec2],
    per_curve: usize,
    out: &mut Vec<Vec<Vec2>>,
) {
    {
        for sub in out.iter_mut() {
            sub.clear();
        }
        let mut sub_idx = 0usize;
        let per_curve = per_curve.max(1);
        let mut cur = Vec2::ZERO;
        let mut start = Vec2::ZERO;
        let mut open = false;

        // Grab (or create) the subpath buffer at `sub_idx`.
        macro_rules! buf {
            () => {{
                while out.len() <= sub_idx {
                    out.push(Vec::new());
                }
                &mut out[sub_idx]
            }};
        }

        for (verb, pts) in segments(verbs, points) {
            match verb {
                Verb::Move => {
                    if open && !buf!().is_empty() {
                        sub_idx += 1;
                    }
                    cur = pts[0];
                    start = cur;
                    open = true;
                    let b = buf!();
                    b.clear();
                    b.push(cur);
                }
                Verb::Line => {
                    cur = pts[0];
                    buf!().push(cur);
                }
                Verb::Quad => {
                    let (c, e) = (pts[0], pts[1]);
                    let b = buf!();
                    for i in 1..=per_curve {
                        let t = i as f32 / per_curve as f32;
                        b.push(eval_quad(cur, c, e, t));
                    }
                    cur = e;
                }
                Verb::Cubic => {
                    let (c1, c2, e) = (pts[0], pts[1], pts[2]);
                    let b = buf!();
                    for i in 1..=per_curve {
                        let t = i as f32 / per_curve as f32;
                        b.push(eval_cubic(cur, c1, c2, e, t));
                    }
                    cur = e;
                }
                Verb::Close => {
                    let b = buf!();
                    if b.last().map(|l| l.dist(start) > 1e-4).unwrap_or(false) {
                        b.push(start);
                    }
                    cur = start;
                    if !b.is_empty() {
                        sub_idx += 1;
                        open = false;
                    }
                }
            }
        }
        // Drop any trailing buffers left over from a previous, longer path.
        let used = if open && out.get(sub_idx).map(|b| !b.is_empty()).unwrap_or(false) {
            sub_idx + 1
        } else {
            sub_idx
        };
        out.truncate(used);
        out.retain(|s| s.len() >= 2);
    }
}

fn fmt(v: f32) -> String {
    // Locale-independent, no trailing ".0" noise (DESIGN.md §7).
    let r = (v * 1000.0).round() / 1000.0;
    if r == r.trunc() {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

pub struct Segments<'a> {
    verbs: &'a [Verb],
    points: &'a [Vec2],
    vi: usize,
    pi: usize,
}

impl<'a> Iterator for Segments<'a> {
    type Item = (Verb, &'a [Vec2]);
    fn next(&mut self) -> Option<Self::Item> {
        let v = *self.verbs.get(self.vi)?;
        self.vi += 1;
        let n = v.point_count();
        let end = (self.pi + n).min(self.points.len());
        let slice = &self.points[self.pi.min(end)..end];
        self.pi += n;
        if slice.len() < n {
            return None;
        }
        Some((v, slice))
    }
}

#[inline]
pub fn eval_quad(a: Vec2, c: Vec2, b: Vec2, t: f32) -> Vec2 {
    let it = 1.0 - t;
    a * (it * it) + c * (2.0 * it * t) + b * (t * t)
}

#[inline]
pub fn eval_cubic(a: Vec2, c1: Vec2, c2: Vec2, b: Vec2, t: f32) -> Vec2 {
    let it = 1.0 - t;
    a * (it * it * it) + c1 * (3.0 * it * it * t) + c2 * (3.0 * it * t * t) + b * (t * t * t)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path data is empty")]
    Empty,
    #[error("unknown path command '{0}' at byte {1}")]
    UnknownCommand(char, usize),
    #[error("path must start with a move ('M' or 'm'), found '{0}'")]
    NoLeadingMove(char),
    #[error("expected a number at byte {0}, found {1:?}")]
    ExpectedNumber(usize, String),
    #[error("command '{0}' near byte {1} is missing arguments")]
    TruncatedCommand(char, usize),
    #[error("non-finite coordinate at byte {0}")]
    NonFinite(usize),
}

/// Parse the SVG path subset: `M m L l H h V v C c Q q Z z`.
///
/// Arcs (`A`), smooth continuations (`S`, `T`) and any other SVG feature are
/// rejected rather than silently ignored — a skin that uses them must be told,
/// not quietly rendered wrong.
pub fn parse_path(s: &str) -> Result<Path, PathError> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut path = Path::new();
    let mut cur = Vec2::ZERO;
    let mut start = Vec2::ZERO;
    let mut last_cmd: Option<u8> = None;
    let mut seen_move = false;

    skip_sep(bytes, &mut i);
    if i >= bytes.len() {
        return Err(PathError::Empty);
    }

    while i < bytes.len() {
        skip_sep(bytes, &mut i);
        if i >= bytes.len() {
            break;
        }
        let b = bytes[i];
        let cmd = if b.is_ascii_alphabetic() {
            i += 1;
            last_cmd = Some(b);
            b
        } else {
            // Implicit repeat of the previous command, as SVG allows. A repeated
            // moveto becomes a lineto, which is the SVG rule.
            match last_cmd {
                Some(b'M') => b'L',
                Some(b'm') => b'l',
                Some(c) => c,
                None => return Err(PathError::NoLeadingMove(b as char)),
            }
        };

        if !seen_move && !matches!(cmd, b'M' | b'm') {
            return Err(PathError::NoLeadingMove(cmd as char));
        }

        let rel = cmd.is_ascii_lowercase();
        let base = |cur: Vec2| if rel { cur } else { Vec2::ZERO };

        match cmd.to_ascii_uppercase() {
            b'M' => {
                let p = base(cur) + read_point(bytes, &mut i, cmd)?;
                path.move_to(p);
                cur = p;
                start = p;
                seen_move = true;
            }
            b'L' => {
                let p = base(cur) + read_point(bytes, &mut i, cmd)?;
                path.line_to(p);
                cur = p;
            }
            b'H' => {
                let x = read_num(bytes, &mut i, cmd)?;
                let p = Vec2::new(if rel { cur.x + x } else { x }, cur.y);
                path.line_to(p);
                cur = p;
            }
            b'V' => {
                let y = read_num(bytes, &mut i, cmd)?;
                let p = Vec2::new(cur.x, if rel { cur.y + y } else { y });
                path.line_to(p);
                cur = p;
            }
            b'Q' => {
                let c = base(cur) + read_point(bytes, &mut i, cmd)?;
                let p = base(cur) + read_point(bytes, &mut i, cmd)?;
                path.quad_to(c, p);
                cur = p;
            }
            b'C' => {
                let c1 = base(cur) + read_point(bytes, &mut i, cmd)?;
                let c2 = base(cur) + read_point(bytes, &mut i, cmd)?;
                let p = base(cur) + read_point(bytes, &mut i, cmd)?;
                path.cubic_to(c1, c2, p);
                cur = p;
            }
            b'Z' => {
                path.close();
                cur = start;
            }
            _ => return Err(PathError::UnknownCommand(cmd as char, i)),
        }
    }

    if path.verbs.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(path)
}

fn skip_sep(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\r' | b'\n' | b',') {
        *i += 1;
    }
}

fn read_point(b: &[u8], i: &mut usize, cmd: u8) -> Result<Vec2, PathError> {
    let x = read_num(b, i, cmd)?;
    let y = read_num(b, i, cmd)?;
    Ok(Vec2::new(x, y))
}

fn read_num(b: &[u8], i: &mut usize, cmd: u8) -> Result<f32, PathError> {
    skip_sep(b, i);
    let start = *i;
    if start >= b.len() {
        return Err(PathError::TruncatedCommand(cmd as char, start));
    }
    if matches!(b[*i], b'+' | b'-') {
        *i += 1;
    }
    let mut saw_digit = false;
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
        saw_digit = true;
    }
    if *i < b.len() && b[*i] == b'.' {
        *i += 1;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
            saw_digit = true;
        }
    }
    if saw_digit && *i < b.len() && matches!(b[*i], b'e' | b'E') {
        let save = *i;
        *i += 1;
        if *i < b.len() && matches!(b[*i], b'+' | b'-') {
            *i += 1;
        }
        let mut exp_digit = false;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
            exp_digit = true;
        }
        if !exp_digit {
            *i = save;
        }
    }
    if !saw_digit {
        let snippet: String = b[start..(start + 8).min(b.len())]
            .iter()
            .map(|c| *c as char)
            .collect();
        return Err(PathError::ExpectedNumber(start, snippet));
    }
    let text = std::str::from_utf8(&b[start..*i]).unwrap_or("");
    let v: f32 = text
        .parse()
        .map_err(|_| PathError::ExpectedNumber(start, text.to_string()))?;
    if !v.is_finite() {
        return Err(PathError::NonFinite(start));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_closed_polygon() {
        let p = parse_path("M 0 0 L 10 0 L 10 10 L 0 10 Z").unwrap();
        assert_eq!(
            p.verbs,
            vec![Verb::Move, Verb::Line, Verb::Line, Verb::Line, Verb::Close]
        );
        assert_eq!(p.points.len(), 4);
        assert_eq!(p.points[2], Vec2::new(10.0, 10.0));
    }

    #[test]
    fn relative_commands_accumulate() {
        let p = parse_path("m 5 5 l 10 0 l 0 10 z").unwrap();
        assert_eq!(p.points[0], Vec2::new(5.0, 5.0));
        assert_eq!(p.points[1], Vec2::new(15.0, 5.0));
        assert_eq!(p.points[2], Vec2::new(15.0, 15.0));
    }

    #[test]
    fn implicit_repeat_after_moveto_becomes_lineto() {
        let p = parse_path("M 0 0 1 1 2 2").unwrap();
        assert_eq!(p.verbs, vec![Verb::Move, Verb::Line, Verb::Line]);
        assert_eq!(p.points[2], Vec2::new(2.0, 2.0));
    }

    #[test]
    fn horizontal_and_vertical_shorthands() {
        let p = parse_path("M 0 0 H 10 V 20 h -5 v -5 Z").unwrap();
        assert_eq!(p.points[1], Vec2::new(10.0, 0.0));
        assert_eq!(p.points[2], Vec2::new(10.0, 20.0));
        assert_eq!(p.points[3], Vec2::new(5.0, 20.0));
        assert_eq!(p.points[4], Vec2::new(5.0, 15.0));
    }

    #[test]
    fn curves_carry_their_control_points() {
        let p = parse_path("M 0 0 C 1 2 3 4 5 6 Q 7 8 9 10").unwrap();
        assert_eq!(p.verbs, vec![Verb::Move, Verb::Cubic, Verb::Quad]);
        assert_eq!(p.points.len(), 1 + 3 + 2);
        assert_eq!(p.points[3], Vec2::new(5.0, 6.0));
    }

    #[test]
    fn scientific_notation_and_signs_parse() {
        let p = parse_path("M -1.5e1 +2 L .5 -.25").unwrap();
        assert_eq!(p.points[0], Vec2::new(-15.0, 2.0));
        assert_eq!(p.points[1], Vec2::new(0.5, -0.25));
    }

    #[test]
    fn commas_and_newlines_are_separators() {
        let p = parse_path("M0,0\n  L10,0\r\nL10,10Z").unwrap();
        assert_eq!(p.points.len(), 3);
    }

    #[test]
    fn rejects_arcs_and_smooth_curves() {
        assert!(matches!(
            parse_path("M 0 0 A 1 1 0 0 1 10 10"),
            Err(PathError::UnknownCommand('A', _))
        ));
        assert!(matches!(
            parse_path("M 0 0 S 1 1 2 2"),
            Err(PathError::UnknownCommand('S', _))
        ));
    }

    #[test]
    fn rejects_a_path_that_does_not_start_with_a_move() {
        assert!(matches!(
            parse_path("L 10 10"),
            Err(PathError::NoLeadingMove('L'))
        ));
    }

    #[test]
    fn rejects_truncated_and_garbage_arguments() {
        assert!(matches!(
            parse_path("M 0 0 L 10"),
            Err(PathError::TruncatedCommand('L', _))
        ));
        // A letter where a number belongs is reported as a bad number, with the
        // offending text quoted, not as a mystery command.
        match parse_path("M 0 0 L x y") {
            Err(PathError::ExpectedNumber(_, snippet)) => assert!(snippet.starts_with('x')),
            other => panic!("expected ExpectedNumber, got {other:?}"),
        }
        // A letter where a command belongs is reported verbatim, in the case
        // the author actually typed.
        assert!(matches!(
            parse_path("M 0 0 x 1 1"),
            Err(PathError::UnknownCommand('x', _))
        ));
        assert!(matches!(parse_path("   "), Err(PathError::Empty)));
    }

    #[test]
    fn svg_round_trips_through_parse() {
        let src = "M 0 0 L 10 0 C 12 2 12 8 10 10 Q 5 12 0 10 Z";
        let a = parse_path(src).unwrap();
        let b = parse_path(&a.to_svg()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn flatten_produces_one_ring_per_subpath() {
        let p = parse_path("M 0 0 L 10 0 L 10 10 Z M 20 20 L 30 20 L 30 30 Z").unwrap();
        let rings = p.flatten(8);
        assert_eq!(rings.len(), 2);
        // Closed rings come back with the start point repeated.
        assert_eq!(rings[0].first(), rings[0].last());
        assert_eq!(rings[1][0], Vec2::new(20.0, 20.0));
    }

    #[test]
    fn flatten_samples_curves() {
        let p = parse_path("M 0 0 C 0 10 10 10 10 0 Z").unwrap();
        let rings = p.flatten(16);
        assert_eq!(rings.len(), 1);
        assert!(rings[0].len() >= 17);
        // The cubic's midpoint sits at y = 7.5 for these control points.
        let mid = rings[0][8];
        assert!((mid.y - 7.5).abs() < 0.5, "midpoint was {mid:?}");
    }

    #[test]
    fn flatten_into_reuses_its_buffers() {
        let big = parse_path("M 0 0 L 1 0 L 1 1 Z M 5 5 L 6 5 L 6 6 Z").unwrap();
        let small = parse_path("M 0 0 L 1 0 L 1 1 Z").unwrap();
        let mut buf = Vec::new();
        big.flatten_into(&big.points, 4, &mut buf);
        assert_eq!(buf.len(), 2);
        small.flatten_into(&small.points, 4, &mut buf);
        assert_eq!(buf.len(), 1, "stale subpath survived the reuse");
    }

    #[test]
    fn segments_walk_a_supplied_point_array() {
        // The whole point of the verb/point split: deform the points, keep the
        // verbs, and the path is still walkable.
        let p = parse_path("M 0 0 L 10 0 Q 15 5 10 10 Z").unwrap();
        let moved: Vec<Vec2> = p.points.iter().map(|q| *q + Vec2::new(100.0, 0.0)).collect();
        let segs: Vec<_> = p.segments(&moved).collect();
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[1].1[0], Vec2::new(110.0, 0.0));
        assert_eq!(segs[2].0, Verb::Quad);
        assert_eq!(segs[2].1.len(), 2);
    }

    #[test]
    fn bounds_cover_every_point_including_controls() {
        let p = parse_path("M 0 0 Q 50 -20 10 10").unwrap();
        let b = p.bounds();
        assert_eq!(b.min, Vec2::new(0.0, -20.0));
        assert_eq!(b.max, Vec2::new(50.0, 10.0));
    }
}
