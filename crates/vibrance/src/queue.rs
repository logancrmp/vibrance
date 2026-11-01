//! Queue rules (SPEC 4.2). Pure logic; the app turns `Step`s into engine calls.

use vibrance_library::TrackId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    Queue,
    Song,
}

impl Repeat {
    pub fn cycle(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::Queue,
            Repeat::Queue => Repeat::Song,
            Repeat::Song => Repeat::Off,
        }
    }
}

/// What playback should do after a queue operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Play(TrackId),
    Restart,
    Stop,
}

#[derive(Debug, Clone, Default)]
pub struct Queue {
    pub items: Vec<TrackId>,
    /// Index of the playing item while it is still in the list.
    pub current: Option<usize>,
    /// Index of the item that plays when the current one ends.
    cursor: usize,
    /// The audible track. Survives Clear and removal of its row.
    pub now: Option<TrackId>,
    pub shuffle: bool,
    /// Order before shuffling, restored when shuffle is turned off.
    original: Option<Vec<TrackId>>,
    pub repeat: Repeat,
    rng: u64,
}

impl Queue {
    /// Play from the library: the queue becomes `ids` and the first one plays.
    pub fn replace(&mut self, ids: Vec<TrackId>) -> Step {
        if ids.is_empty() {
            return Step::Stop;
        }
        if self.shuffle {
            self.original = Some(ids.clone());
            self.items = ids;
            self.shuffle_from(0);
        } else {
            self.original = None;
            self.items = ids;
        }
        self.jump(0)
    }

    pub fn jump(&mut self, index: usize) -> Step {
        let Some(&id) = self.items.get(index) else { return Step::Stop };
        self.current = Some(index);
        self.cursor = index + 1;
        self.now = Some(id);
        Step::Play(id)
    }

    /// The Next button.
    pub fn next(&mut self) -> Step {
        if self.cursor < self.items.len() {
            return self.jump(self.cursor);
        }
        if self.repeat == Repeat::Queue && !self.items.is_empty() {
            self.wrap();
            return self.jump(0);
        }
        self.current = None;
        Step::Stop
    }

    /// The current track ended on its own.
    pub fn finished(&mut self) -> Step {
        if self.repeat == Repeat::Song && self.now.is_some() {
            return Step::Restart;
        }
        self.next()
    }

    /// The Previous button, once the app has decided not to restart the track.
    pub fn prev(&mut self) -> Step {
        let before = match self.current {
            Some(i) => i.checked_sub(1),
            None => self.cursor.checked_sub(1),
        };
        match before {
            Some(i) => self.jump(i),
            None => Step::Restart,
        }
    }

    /// The track the engine should join gaplessly when the current one ends.
    pub fn upcoming(&self) -> Option<TrackId> {
        if self.repeat == Repeat::Song {
            return self.now;
        }
        self.items
            .get(self.cursor)
            .copied()
            .or_else(|| (self.repeat == Repeat::Queue).then(|| self.items.first().copied()).flatten())
    }

    /// The engine joined `to` gaplessly; commit the step `upcoming` predicted.
    pub fn advanced_to(&mut self, to: TrackId) {
        match self.finished() {
            Step::Play(id) if id == to => {}
            Step::Restart => {}
            _ => {
                // The queue changed after the engine preloaded; follow what is audible.
                self.now = Some(to);
                self.current = self.items.iter().position(|&t| t == to);
                self.cursor = self.current.map_or(self.cursor, |i| i + 1);
            }
        }
    }

    /// Remove rows. A removed current item keeps playing; its successor plays next.
    pub fn remove(&mut self, indices: &[usize]) {
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        for &r in sorted.iter().rev() {
            if r >= self.items.len() {
                continue;
            }
            self.items.remove(r);
            self.current = match self.current {
                Some(c) if c == r => None,
                Some(c) if c > r => Some(c - 1),
                other => other,
            };
            if r < self.cursor {
                self.cursor -= 1;
            }
        }
        if let Some(original) = &mut self.original {
            original.retain(|id| self.items.contains(id));
        }
    }

    /// Empty the queue without stopping the audible track (SPEC 4.2).
    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.cursor = 0;
        self.original = None;
    }

    pub fn append(&mut self, ids: &[TrackId]) {
        self.items.extend_from_slice(ids);
        if let Some(original) = &mut self.original {
            original.extend_from_slice(ids);
        }
    }

    pub fn insert_next(&mut self, ids: &[TrackId]) {
        let at = self.cursor.min(self.items.len());
        self.items.splice(at..at, ids.iter().copied());
        if let Some(original) = &mut self.original {
            original.extend_from_slice(ids);
        }
    }

    pub fn toggle_shuffle(&mut self) {
        if self.shuffle {
            self.shuffle = false;
            if let Some(original) = self.original.take() {
                let played = &self.items[..self.cursor.min(self.items.len())];
                let rest: Vec<TrackId> = original
                    .into_iter()
                    .filter(|id| !played.contains(id) && self.items.contains(id))
                    .collect();
                self.items.truncate(self.cursor.min(self.items.len()));
                self.items.extend(rest);
            }
        } else {
            self.shuffle = true;
            self.original = Some(self.items.clone());
            self.shuffle_from(self.cursor);
        }
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.cycle();
    }

    /// Item count and the index the next track comes from, for display.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Repeat-queue wrap. The first item stays first so the gapless preload matches.
    fn wrap(&mut self) {
        if self.shuffle && self.items.len() > 2 {
            self.shuffle_from(1);
        }
    }

    fn shuffle_from(&mut self, start: usize) {
        if self.rng == 0 {
            self.rng = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
                | 1;
        }
        let len = self.items.len();
        for i in (start + 1..len).rev() {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            let j = start + (self.rng % (i - start + 1) as u64) as usize;
            self.items.swap(i, j);
        }
    }

    /// Compact text form for the UI-state table.
    pub fn to_state(&self) -> String {
        let ids: Vec<String> = self.items.iter().map(i64::to_string).collect();
        format!(
            "{};{};{};{};{};{}",
            match self.repeat {
                Repeat::Off => 0,
                Repeat::Queue => 1,
                Repeat::Song => 2,
            },
            u8::from(self.shuffle),
            self.current.map_or(-1, |c| c as i64),
            self.cursor,
            self.now.unwrap_or(-1),
            ids.join(","),
        )
    }

    pub fn from_state(s: &str) -> Option<Queue> {
        let mut parts = s.splitn(6, ';');
        let repeat = match parts.next()? {
            "1" => Repeat::Queue,
            "2" => Repeat::Song,
            _ => Repeat::Off,
        };
        let shuffle = parts.next()? == "1";
        let current: i64 = parts.next()?.parse().ok()?;
        let cursor: usize = parts.next()?.parse().ok()?;
        let now: i64 = parts.next()?.parse().ok()?;
        let items: Vec<TrackId> = parts.next()?.split(',').filter_map(|x| x.parse().ok()).collect();
        Some(Queue {
            current: usize::try_from(current).ok().filter(|&c| c < items.len()),
            cursor: cursor.min(items.len()),
            now: (now >= 0).then_some(now),
            items,
            shuffle,
            original: None,
            repeat,
            rng: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(ids: &[TrackId]) -> Queue {
        let mut q = Queue::default();
        q.replace(ids.to_vec());
        q
    }

    #[test]
    fn replace_plays_first() {
        let mut q = Queue::default();
        assert_eq!(q.replace(vec![1, 2, 3]), Step::Play(1));
        assert_eq!(q.upcoming(), Some(2));
    }

    #[test]
    fn end_of_queue_stops_unless_repeat() {
        let mut q = q(&[1, 2]);
        assert_eq!(q.finished(), Step::Play(2));
        assert_eq!(q.finished(), Step::Stop);
        let mut q2 = self::q(&[1, 2]);
        q2.repeat = Repeat::Queue;
        q2.finished();
        assert_eq!(q2.upcoming(), Some(1));
        assert_eq!(q2.finished(), Step::Play(1));
    }

    #[test]
    fn repeat_song_restarts() {
        let mut q = q(&[1, 2]);
        q.repeat = Repeat::Song;
        assert_eq!(q.upcoming(), Some(1));
        assert_eq!(q.finished(), Step::Restart);
        assert_eq!(q.next(), Step::Play(2));
    }

    #[test]
    fn clear_keeps_playing_then_continues_with_new_items() {
        let mut q = q(&[1, 2, 3]);
        q.clear();
        assert_eq!(q.now, Some(1));
        assert_eq!(q.upcoming(), None);
        q.append(&[9]);
        assert_eq!(q.upcoming(), Some(9));
        assert_eq!(q.finished(), Step::Play(9));
    }

    #[test]
    fn removing_current_continues_with_successor() {
        let mut q = q(&[1, 2, 3]);
        q.jump(1);
        q.remove(&[1]);
        assert_eq!(q.current, None);
        assert_eq!(q.now, Some(2));
        assert_eq!(q.upcoming(), Some(3));
        assert_eq!(q.prev(), Step::Play(1));
    }

    #[test]
    fn shuffle_keeps_played_and_restores_order() {
        let ids: Vec<TrackId> = (1..=20).collect();
        let mut q = q(&ids);
        q.jump(4);
        q.toggle_shuffle();
        assert_eq!(&q.items[..5], &ids[..5]);
        let mut upcoming = q.items[5..].to_vec();
        upcoming.sort();
        assert_eq!(upcoming, ids[5..].to_vec());
        q.toggle_shuffle();
        assert_eq!(q.items, ids);
    }

    #[test]
    fn gapless_advance_commits_prediction() {
        let mut q = q(&[1, 2, 3]);
        let next = q.upcoming().unwrap();
        q.advanced_to(next);
        assert_eq!(q.now, Some(2));
        assert_eq!(q.current, Some(1));
    }

    #[test]
    fn state_round_trip() {
        let mut q = q(&[5, 6, 7]);
        q.jump(1);
        q.repeat = Repeat::Queue;
        let back = Queue::from_state(&q.to_state()).unwrap();
        assert_eq!(back.items, vec![5, 6, 7]);
        assert_eq!(back.current, Some(1));
        assert_eq!(back.now, Some(6));
        assert_eq!(back.repeat, Repeat::Queue);
    }
}
