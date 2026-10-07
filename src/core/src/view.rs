//! Which chunks a player can see, and which of those their client already has.

use std::collections::HashSet;

use bedrockrs_protocol::types::ChunkPos;

/// A player's view of the world: the chunk they stand in, how far they see,
/// and the chunks sent to their client so far.
#[derive(Debug, Clone, Default)]
pub struct ChunkView {
    centre: Option<ChunkPos>,
    radius: i32,
    sent: HashSet<ChunkPos>,
}

impl ChunkView {
    pub fn new() -> Self {
        Self::default()
    }

    /// The chunk the view is centred on, once it has one.
    pub fn centre(&self) -> Option<ChunkPos> {
        self.centre
    }

    pub fn radius(&self) -> i32 {
        self.radius
    }

    /// Whether the view's centre would move to `chunk`.
    pub fn crosses_into(&self, chunk: ChunkPos) -> bool {
        self.centre != Some(chunk)
    }

    /// Recentres the view and returns the chunks in range that the client does
    /// not have yet, nearest first; they count as sent from now on. Chunks that
    /// fell out of range are forgotten, since the client unloads them, so they
    /// are sent again if the player comes back.
    pub fn update(&mut self, centre: ChunkPos, radius: i32) -> Vec<ChunkPos> {
        self.centre = Some(centre);
        self.radius = radius;
        let in_range = |chunk: ChunkPos| chunk.distance_squared(centre) <= i64::from(radius).pow(2);
        self.sent.retain(|chunk| in_range(*chunk));

        let mut missing: Vec<ChunkPos> = (-radius..=radius)
            .flat_map(|dx| {
                (-radius..=radius).map(move |dz| ChunkPos::new(centre.x + dx, centre.z + dz))
            })
            .filter(|chunk| in_range(*chunk) && !self.sent.contains(chunk))
            .collect();
        missing.sort_by_key(|chunk| chunk.distance_squared(centre));
        self.sent.extend(missing.iter().copied());
        missing
    }

    /// How many chunks the client has from this view.
    pub fn sent(&self) -> usize {
        self.sent.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_update_sends_the_whole_circle_nearest_first() {
        let mut view = ChunkView::new();
        let chunks = view.update(ChunkPos::new(0, 0), 4);
        // Chunks within a circle of radius 4: 49 of them.
        assert_eq!(chunks.len(), 49);
        assert_eq!(chunks[0], ChunkPos::new(0, 0));
        let distances: Vec<_> = chunks
            .iter()
            .map(|chunk| chunk.distance_squared(ChunkPos::new(0, 0)))
            .collect();
        assert!(distances.is_sorted());
        assert_eq!(view.sent(), 49);
        assert!(
            view.update(ChunkPos::new(0, 0), 4).is_empty(),
            "nothing new"
        );
    }

    #[test]
    fn crossing_a_boundary_sends_only_the_leading_edge() {
        let mut view = ChunkView::new();
        view.update(ChunkPos::new(0, 0), 8);
        let before = view.sent();
        assert!(view.crosses_into(ChunkPos::new(1, 0)));

        let chunks = view.update(ChunkPos::new(1, 0), 8);
        // One new column along the east edge of the circle: x = 9 at z = 0,
        // plus the edge cells that came into range.
        assert!(chunks.contains(&ChunkPos::new(9, 0)));
        assert!(chunks.iter().all(|chunk| chunk.x >= 1));
        assert!(!chunks.is_empty() && chunks.len() < 30);
        // The trailing edge was dropped, so the circle's size is unchanged.
        assert_eq!(view.sent(), before);
        assert!(!view.crosses_into(ChunkPos::new(1, 0)));
    }

    #[test]
    fn returning_resends_forgotten_chunks_and_radius_changes_apply() {
        let mut view = ChunkView::new();
        view.update(ChunkPos::new(0, 0), 2);
        view.update(ChunkPos::new(10, 0), 2);
        let back = view.update(ChunkPos::new(0, 0), 2);
        assert_eq!(back.len(), 13, "every chunk is needed again");

        let wider = view.update(ChunkPos::new(0, 0), 3);
        assert_eq!(wider.len(), 29 - 13);
        assert_eq!(view.radius(), 3);
        let narrower = view.update(ChunkPos::new(0, 0), 1);
        assert!(narrower.is_empty());
        assert_eq!(view.sent(), 5);
    }
}
