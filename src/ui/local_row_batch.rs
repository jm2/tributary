//! Coalesced per-row changes to the local library projection.
//!
//! The library engine reports single-file changes as `TrackUpserted` and
//! `TrackRemoved` events. Resolving each one against the whole local row list
//! costs the GTK thread a pass over the library per event, so a burst (a
//! reorganized folder, an `rm -r`) is folded into one batch, indexed by URI,
//! and applied in a single pass.

use std::collections::{HashMap, HashSet};

use gtk::gio;
use gtk::prelude::*;

use super::objects::TrackObject;

/// Net effect of one burst of per-row library events.
///
/// Events fold per URI in arrival order, so applying the batch once leaves a
/// row list exactly as applying each event in turn would: an upsert replaces
/// the first row with its URI in place or appends, and a removal drops the
/// row with its URI (local rows have unique URIs).
#[derive(Default)]
pub(super) struct LocalRowBatch {
    changes: HashMap<String, RowChange>,
    upserted: Vec<TrackObject>,
    events: usize,
}

enum RowChange {
    /// Replace the row with this URI in place, or append at `order`.
    Upsert { row: TrackObject, order: usize },
    /// Drop the row with this URI.
    Remove,
    /// Drop the row with this URI, then append `row` at `order`.
    Reinsert { row: TrackObject, order: usize },
}

impl LocalRowBatch {
    pub(super) fn upsert(&mut self, row: TrackObject) {
        let order = self.events;
        self.events += 1;
        self.upserted.push(row.clone());
        let uri = row.uri();
        let change = match self.changes.remove(&uri) {
            None => RowChange::Upsert { row, order },
            // A repeated upsert replaces the row the first one placed, so the
            // first one's append position stands.
            Some(RowChange::Upsert { order, .. }) => RowChange::Upsert { row, order },
            Some(RowChange::Reinsert { order, .. }) => RowChange::Reinsert { row, order },
            Some(RowChange::Remove) => RowChange::Reinsert { row, order },
        };
        self.changes.insert(uri, change);
    }

    pub(super) fn remove(&mut self, uri: String) {
        self.events += 1;
        self.changes.insert(uri, RowChange::Remove);
    }

    /// How many events the batch has folded.
    pub(super) const fn len(&self) -> usize {
        self.events
    }

    /// Every upserted row in arrival order, for consumers that follow tracks
    /// by identity (the playback queue, an open playlist).
    pub(super) fn upserted(&self) -> &[TrackObject] {
        &self.upserted
    }

    /// Apply the batch to a row list in one pass.
    pub(super) fn apply_to_rows(&self, rows: &mut Vec<TrackObject>) {
        let mut placed = HashSet::new();
        rows.retain_mut(|row| match self.change_for(row) {
            None => true,
            Some((_, RowChange::Remove | RowChange::Reinsert { .. })) => false,
            Some((
                uri,
                RowChange::Upsert {
                    row: replacement, ..
                },
            )) => {
                if placed.insert(uri) {
                    *row = replacement.clone();
                }
                true
            }
        });
        rows.extend(self.appended(&placed).cloned());
    }

    /// Apply the batch to the visible store with per-row edits, so rows the
    /// batch does not touch keep their identity and the view keeps its
    /// scroll position.
    pub(super) fn apply_to_store(&self, store: &gio::ListStore) {
        let mut placed = HashSet::new();
        let mut edits = Vec::new();
        for position in 0..store.n_items() {
            let Some(row) = store.item(position).and_downcast::<TrackObject>() else {
                continue;
            };
            if let Some((uri, change)) = self.change_for(&row) {
                if placed.insert(uri) {
                    edits.push((position, change));
                }
            }
        }
        // Back to front, so each edit leaves the positions still to visit
        // valid.
        for &(position, change) in edits.iter().rev() {
            match change {
                RowChange::Upsert { row, .. } => {
                    store.splice(position, 1, std::slice::from_ref(row));
                }
                RowChange::Remove | RowChange::Reinsert { .. } => store.remove(position),
            }
        }
        for row in self.appended(&placed) {
            store.append(row);
        }
    }

    fn change_for<'a>(&'a self, row: &TrackObject) -> Option<(&'a str, &'a RowChange)> {
        row.with_uri(|uri| self.changes.get_key_value(uri))
            .map(|(uri, change)| (uri.as_str(), change))
    }

    /// Rows to append after the pass, in the order their upserts arrived:
    /// every reinsert, and every upsert whose URI had no row to replace.
    fn appended<'a>(&'a self, placed: &HashSet<&str>) -> impl Iterator<Item = &'a TrackObject> {
        let mut rows: Vec<_> = self
            .changes
            .iter()
            .filter_map(|(uri, change)| match change {
                RowChange::Upsert { row, order } if !placed.contains(uri.as_str()) => {
                    Some((*order, row))
                }
                RowChange::Reinsert { row, order } => Some((*order, row)),
                _ => None,
            })
            .collect();
        rows.sort_unstable_by_key(|(order, _)| *order);
        rows.into_iter().map(|(_, row)| row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    enum Event {
        Upsert(TrackObject),
        Remove(String),
    }

    fn row(uri: &str) -> TrackObject {
        TrackObject::new(
            1, "Title", 180, "Artist", "Album", "Genre", "", 2026, "", 320, 48_000, 0, "flac", uri,
        )
    }

    /// The per-event semantics the batch must reproduce on a row list.
    fn apply_one_by_one(rows: &mut Vec<TrackObject>, events: &[Event]) {
        for event in events {
            match event {
                Event::Upsert(new) => {
                    if let Some(position) = rows.iter().position(|row| row.uri() == new.uri()) {
                        rows[position] = new.clone();
                    } else {
                        rows.push(new.clone());
                    }
                }
                Event::Remove(uri) => rows.retain(|row| row.uri() != *uri),
            }
        }
    }

    /// The per-event semantics the batch must reproduce on the store.
    fn apply_one_by_one_to_store(store: &gio::ListStore, events: &[Event]) {
        let position_of = |uri: &str| {
            (0..store.n_items()).find(|&position| {
                store
                    .item(position)
                    .and_downcast::<TrackObject>()
                    .is_some_and(|row| row.uri() == uri)
            })
        };
        for event in events {
            match event {
                Event::Upsert(new) => match position_of(&new.uri()) {
                    Some(position) => store.splice(position, 1, std::slice::from_ref(new)),
                    None => store.append(new),
                },
                Event::Remove(uri) => {
                    if let Some(position) = position_of(uri) {
                        store.remove(position);
                    }
                }
            }
        }
    }

    fn batch_of(events: &[Event]) -> LocalRowBatch {
        let mut batch = LocalRowBatch::default();
        for event in events.iter().cloned() {
            match event {
                Event::Upsert(row) => batch.upsert(row),
                Event::Remove(uri) => batch.remove(uri),
            }
        }
        batch
    }

    fn store_rows(store: &gio::ListStore) -> Vec<TrackObject> {
        (0..store.n_items())
            .filter_map(|position| store.item(position).and_downcast::<TrackObject>())
            .collect()
    }

    /// Deterministic event sequences over a few URIs, starting from every
    /// subset of those URIs, so replace, append, remove, and every
    /// remove-then-upsert ordering are all exercised.
    fn scenarios() -> Vec<(Vec<TrackObject>, Vec<Event>)> {
        const URIS: [&str; 4] = ["file:///a", "file:///b", "file:///c", "file:///d"];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % bound).expect("small bound")
        };
        let mut scenarios = Vec::new();
        for initial_mask in 0..(1_usize << URIS.len()) {
            for _ in 0..16 {
                let initial = URIS
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| initial_mask & (1 << index) != 0)
                    .map(|(_, uri)| row(uri))
                    .collect();
                let events = (0..next(10))
                    .map(|_| {
                        let uri = URIS[next(4)];
                        if next(3) == 0 {
                            Event::Remove(uri.to_string())
                        } else {
                            Event::Upsert(row(uri))
                        }
                    })
                    .collect();
                scenarios.push((initial, events));
            }
        }
        scenarios
    }

    #[test]
    fn a_batch_applies_to_rows_exactly_as_its_events_one_by_one() {
        for (initial, events) in scenarios() {
            let mut expected = initial.clone();
            apply_one_by_one(&mut expected, &events);
            let mut batched = initial;
            batch_of(&events).apply_to_rows(&mut batched);
            assert_eq!(batched, expected);
        }
    }

    #[test]
    fn a_batch_applies_to_the_store_exactly_as_its_events_one_by_one() {
        for (initial, events) in scenarios() {
            let expected = gio::ListStore::new::<TrackObject>();
            expected.extend_from_slice(&initial);
            apply_one_by_one_to_store(&expected, &events);
            let batched = gio::ListStore::new::<TrackObject>();
            batched.extend_from_slice(&initial);
            batch_of(&events).apply_to_store(&batched);
            assert_eq!(store_rows(&batched), store_rows(&expected));
        }
    }

    #[test]
    fn untouched_rows_keep_their_identity_and_upserts_are_kept_in_order() {
        let kept = row("file:///kept");
        let replaced = row("file:///replaced");
        let removed = row("file:///removed");
        let mut rows = vec![kept.clone(), replaced, removed];
        let replacement = row("file:///replaced");
        let added = row("file:///added");

        let mut batch = LocalRowBatch::default();
        batch.upsert(added.clone());
        batch.remove("file:///removed".to_string());
        batch.upsert(replacement.clone());
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.upserted(), [added.clone(), replacement.clone()]);

        batch.apply_to_rows(&mut rows);
        assert_eq!(rows, [kept, replacement, added]);
    }
}
