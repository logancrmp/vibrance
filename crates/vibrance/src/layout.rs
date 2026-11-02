//! Pane layout: the default arrangement, its saved form, and showing/hiding panes (SPEC 6.2).

use iced::widget::pane_grid::{self, Axis, Configuration, Node};

use crate::app::{Pane, Slot};

/// The default layout (SPEC 5.2): Artists | Albums over Songs, Queue over Album Art.
pub fn default() -> pane_grid::State<Slot> {
    pane_grid::State::with_configuration(parse(DEFAULT).expect("default layout parses"))
}

const DEFAULT: &str = "v0.710(h0.385(v0.380(artists,albums),songs),h0.500(queue,art))";

fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::List(Pane::Artists) => "artists",
        Slot::List(Pane::Albums) => "albums",
        Slot::List(Pane::Songs) => "songs",
        Slot::List(Pane::Queue) => "queue",
        Slot::AlbumArt => "art",
    }
}

fn slot_from(name: &str) -> Option<Slot> {
    Some(match name {
        "artists" => Slot::List(Pane::Artists),
        "albums" => Slot::List(Pane::Albums),
        "songs" => Slot::List(Pane::Songs),
        "queue" => Slot::List(Pane::Queue),
        "art" => Slot::AlbumArt,
        _ => return None,
    })
}

pub fn to_state(state: &pane_grid::State<Slot>) -> String {
    fn walk(state: &pane_grid::State<Slot>, node: &Node, out: &mut String) {
        match node {
            Node::Split { axis, ratio, a, b, .. } => {
                out.push(if *axis == Axis::Vertical { 'v' } else { 'h' });
                out.push_str(&format!("{ratio:.3}("));
                walk(state, a, out);
                out.push(',');
                walk(state, b, out);
                out.push(')');
            }
            Node::Pane(pane) => out.push_str(state.get(*pane).map_or("queue", |s| slot_name(*s))),
        }
    }
    let mut out = String::new();
    walk(state, state.layout(), &mut out);
    out
}

/// Restore a saved layout. Anything malformed, or a layout without the Queue, yields `None`.
pub fn from_state(s: &str) -> Option<pane_grid::State<Slot>> {
    let config = parse(s)?;
    let mut seen = Vec::new();
    collect(&config, &mut seen);
    let unique = seen.iter().all(|s| seen.iter().filter(|t| *t == s).count() == 1);
    (unique && seen.contains(&Slot::List(Pane::Queue))).then(|| pane_grid::State::with_configuration(config))
}

fn collect(config: &Configuration<Slot>, out: &mut Vec<Slot>) {
    match config {
        Configuration::Split { a, b, .. } => {
            collect(a, out);
            collect(b, out);
        }
        Configuration::Pane(slot) => out.push(*slot),
    }
}

fn parse(s: &str) -> Option<Configuration<Slot>> {
    fn node(s: &str) -> Option<(Configuration<Slot>, &str)> {
        let axis = match s.as_bytes().first()? {
            b'v' => Some(Axis::Vertical),
            b'h' => Some(Axis::Horizontal),
            _ => None,
        };
        if let Some(axis) = axis.filter(|_| s[1..].starts_with(|c: char| c.is_ascii_digit())) {
            let open = s.find('(')?;
            let ratio: f32 = s[1..open].parse().ok()?;
            let (a, rest) = node(&s[open + 1..])?;
            let rest = rest.strip_prefix(',')?;
            let (b, rest) = node(rest)?;
            let rest = rest.strip_prefix(')')?;
            let ratio = ratio.clamp(0.05, 0.95);
            Some((Configuration::Split { axis, ratio, a: Box::new(a), b: Box::new(b) }, rest))
        } else {
            let end = s.find([',', ')']).unwrap_or(s.len());
            Some((Configuration::Pane(slot_from(&s[..end])?), &s[end..]))
        }
    }
    let (config, rest) = node(s.trim())?;
    rest.is_empty().then_some(config)
}

pub fn is_visible(state: &pane_grid::State<Slot>, slot: Slot) -> bool {
    state.iter().any(|(_, s)| *s == slot)
}

pub fn find(state: &pane_grid::State<Slot>, slot: Slot) -> Option<pane_grid::Pane> {
    state.iter().find(|(_, s)| **s == slot).map(|(p, _)| *p)
}

/// Now Playing and Queue always stay (SPEC 6.2).
pub fn can_hide(slot: Slot) -> bool {
    slot != Slot::List(Pane::Queue)
}

pub fn hide(state: &mut pane_grid::State<Slot>, slot: Slot) {
    if can_hide(slot)
        && let Some(pane) = find(state, slot)
    {
        state.close(pane);
    }
}

/// Bring a pane back next to its natural neighbour.
pub fn show(state: &mut pane_grid::State<Slot>, slot: Slot) {
    if is_visible(state, slot) {
        return;
    }
    use Pane::*;
    // (anchor, axis, new pane goes before the anchor, share of the space it takes)
    let anchors: &[(Slot, Axis, bool, f32)] = match slot {
        Slot::List(Artists) => &[
            (Slot::List(Albums), Axis::Vertical, true, 0.38),
            (Slot::List(Songs), Axis::Horizontal, true, 0.38),
            (Slot::List(Queue), Axis::Vertical, true, 0.3),
        ],
        Slot::List(Albums) => &[
            (Slot::List(Artists), Axis::Vertical, false, 0.62),
            (Slot::List(Songs), Axis::Horizontal, true, 0.38),
            (Slot::List(Queue), Axis::Vertical, true, 0.3),
        ],
        Slot::List(Songs) => &[
            (Slot::List(Albums), Axis::Horizontal, false, 0.6),
            (Slot::List(Artists), Axis::Horizontal, false, 0.6),
            (Slot::List(Queue), Axis::Vertical, true, 0.6),
        ],
        Slot::AlbumArt => &[(Slot::List(Queue), Axis::Horizontal, false, 0.45)],
        Slot::List(Queue) => &[],
    };
    for &(anchor, axis, before, share) in anchors {
        let Some(target) = find(state, anchor) else { continue };
        if let Some((new, split)) = state.split(axis, target, slot) {
            if before {
                state.swap(new, target);
                state.resize(split, share);
            } else {
                state.resize(split, 1.0 - share);
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let state = default();
        assert_eq!(to_state(&state), DEFAULT);
        assert!(from_state("garbage").is_none());
        assert!(from_state("v0.5(artists,songs)").is_none(), "queue is required");
    }

    #[test]
    fn hide_and_show() {
        let mut state = default();
        hide(&mut state, Slot::List(Pane::Artists));
        assert!(!is_visible(&state, Slot::List(Pane::Artists)));
        show(&mut state, Slot::List(Pane::Artists));
        assert!(is_visible(&state, Slot::List(Pane::Artists)));
        hide(&mut state, Slot::List(Pane::Queue));
        assert!(is_visible(&state, Slot::List(Pane::Queue)));
    }
}
