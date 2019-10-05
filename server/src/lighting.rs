//! Calculation of block and sky light.
//!
//! # Algorithms: block light
//! For block light calculation, we define four types of block
//! updates for which to perform lighting:
//!
//! * Creation of a light-emitting block. We simply propagate
//! the light update using flood fill.
//!
//! * Removal of a light-emitting block. We first perform flood fill
//! and set any blocks which were previously affected by this block's
//! light to 0. Then, we recalculate those blocks' values based on the
//! blocks bordering the flood fill area.
//!
//! * Creation of an opaque, non-emitting block. We first set the created
//! block to air temporarily. We then query for nearby lights
//! within a range of 15 (the maximum distance travelled by light) and perform
//! algorithm #2 on them. Finally, we set the created block back to the correct
//! value and perform algorithm #1 on all lights.
//!
//! * Removal of an opaque, non-emitting block. In this case,
//! we set the new air block's light to the highest value of an
//! adjacent block minus 1. We then perform algorithm #1 on this new block.
//!
//! Each algorithm is implemented in a separate function, and `LightingSystem`
//! determines which to use based on the values of the block update event.
//!
//! If we are recalculating light for an entire chunk, e.g. when a chunk is generated,
//! we first zero out light, then find all light sources in the chunk and perform
//! algorithm #1 on them as if they had just been placed.

use arrayvec::ArrayVec;
use failure::_core::marker::PhantomData;
use feather_blocks::{Block, BlockExt};
use feather_core::prelude::ChunkMap;
use feather_core::world::chunk_relative_pos;
use feather_core::{BlockPosition, Chunk, ChunkPosition};
use hashbrown::HashSet;
use std::collections::VecDeque;

/// Lighter context, used to cache things during
/// a lighting iteration.
struct Context<'a> {
    /// Reference to the current cached chunk.
    /// This is used to avoid repetitive hashmap
    /// accesses in the chunk map when groups
    /// of clustered blocks are queried for.
    current_chunk: *mut Chunk,
    /// Chunk map. Raw pointers are used to bypass the borrow
    /// checker, since `current_chunk` refers to the chunk map,
    /// which isn't allowed.
    chunk_map: *mut ChunkMap,
    _phantom: PhantomData<&'a ()>,
}

impl<'a> Context<'a> {
    fn new(chunk_map: &'a mut ChunkMap, start_chunk: ChunkPosition) -> Option<Self> {
        let chunk_map = chunk_map as *mut ChunkMap;

        // Safety: `chunk_map` is a valid pointer
        // made from a mutable reference.
        // It has not been modified since.
        let current_chunk = unsafe { (*chunk_map).chunk_at_mut(start_chunk)? as *mut Chunk };

        Some(Self {
            current_chunk,
            chunk_map,
            _phantom: PhantomData,
        })
    }

    fn chunk_at_mut(&mut self, pos: ChunkPosition) -> Option<&'a mut Chunk> {
        if pos == (unsafe { &*self.current_chunk }).position() {
            Some(unsafe { &mut *self.current_chunk })
        } else {
            // Safety: While `self.current_chunk` refers to the chunk map,
            // it is never accessed between mutations of the chunk
            // map itself, since `Context` holds a unique reference to the
            // map and never mutates it.
            self.current_chunk = unsafe { (*self.chunk_map).chunk_at_mut(pos)? };
            Some(unsafe { &mut *self.current_chunk })
        }
    }

    fn block_light_at(&mut self, pos: BlockPosition) -> u8 {
        match self.chunk_at_mut(pos.chunk_pos()) {
            Some(chunk) => {
                let (x, y, z) = chunk_relative_pos(pos);
                chunk.block_light_at(x, y, z)
            }
            None => 0,
        }
    }

    fn set_block_light_at(&mut self, pos: BlockPosition, value: u8) {
        if let Some(chunk) = self.chunk_at_mut(pos.chunk_pos()) {
            let (x, y, z) = chunk_relative_pos(pos);
            chunk.set_block_light_at(x, y, z, value);
        }
    }

    fn block_at(&mut self, pos: BlockPosition) -> Block {
        match self.chunk_at_mut(pos.chunk_pos()) {
            Some(chunk) => {
                let (x, y, z) = chunk_relative_pos(pos);
                chunk.block_at(x, y, z)
            }
            None => Block::Air,
        }
    }
}

/// Algorithm #4, as described in the module-level docs.
fn opaque_non_emitting_removal(context: &mut Context, position: BlockPosition) {
    // Find highest light value of 6 adjacent blocks.
    let adjacent = adjacent_blocks(position);
    let mut value = adjacent
        .into_iter()
        .map(|pos| context.block_light_at(pos))
        .max()
        .unwrap();

    if value > 0 {
        value -= 1;
    }

    context.set_block_light_at(position, value);
}

/// Performs flood fill starting at `start` and travelling up
/// to `max_dist` blocks.
///
/// For each block iterated over, the provided closure will be invoked.
/// No block will be iterated more than once.
fn flood_fill<F>(ctx: &mut Context, start: BlockPosition, max_dist: u8, mut func: F)
where
    F: FnMut(&mut Context, BlockPosition),
{
    // Don't iterate over same block more than once
    let mut touched = HashSet::with_capacity(64);
    touched.insert(start);

    // We use a queue-based algorithm rather than a recursive
    // one.
    let mut queue = VecDeque::with_capacity(64);

    queue.push_back(start);

    let mut finished = false;

    while let Some(pos) = queue.pop_front() {
        if finished {
            break;
        }

        let blocks = adjacent_blocks(pos);

        blocks.into_iter().for_each(|pos| {
            if pos.manhattan_distance(start) > max_dist as i32 {
                // Finished
                finished = true;
                return;
            }

            // Skip if we already went over this block
            if !touched.insert(pos) {
                return;
            }

            let block = ctx.block_at(pos);
            if block.is_opaque() {
                return; // Stop iterating
            }

            // Call closure
            func(ctx, pos);

            // Add block to queue
            queue.push_back(pos);
        });
    }
}

/// Returns the up to six adjacent blocks to a given block position.
fn adjacent_blocks(to: BlockPosition) -> ArrayVec<[BlockPosition; 6]> {
    let offsets = [
        (-1, 0, 0),
        (1, 0, 0),
        (0, -1, 0),
        (0, 1, 0),
        (0, 0, -1),
        (0, 0, 1),
    ];
    offsets
        .iter()
        .map(|(x, y, z)| BlockPosition::new(to.x + *x, to.y + *y, to.z + *z))
        .filter(|pos| pos.y >= 0 && pos.y <= 256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashbrown::HashMap;

    #[test]
    fn test_context() {
        let mut chunk_map = ChunkMap::new();

        let pos = ChunkPosition::new(0, 0);
        chunk_map.set_chunk_at(pos, Chunk::new(pos));
        let pos2 = ChunkPosition::new(0, 1);
        chunk_map.set_chunk_at(pos2, Chunk::new(pos2));

        let mut ctx = Context::new(&mut chunk_map, pos).unwrap();

        assert_eq!(ctx.chunk_at_mut(pos).unwrap().position(), pos);
        assert_eq!(ctx.chunk_at_mut(pos2).unwrap().position(), pos2);
        assert_eq!(ctx.chunk_at_mut(pos).unwrap().position(), pos);
    }

    #[test]
    fn test_opaque_non_emitting_removal() {
        let mut chunk_map = chunk_map();
        let mut ctx = Context::new(&mut chunk_map, ChunkPosition::new(0, 0)).unwrap();

        ctx.set_block_light_at(BlockPosition::new(0, 0, 0), 10);
        ctx.set_block_light_at(BlockPosition::new(0, 2, 0), 9);
        ctx.set_block_light_at(BlockPosition::new(1, 1, 0), 8);
        ctx.set_block_light_at(BlockPosition::new(-1, 1, 0), 11);
        ctx.set_block_light_at(BlockPosition::new(0, 1, 1), 0);
        ctx.set_block_light_at(BlockPosition::new(0, 1, -1), 12);
        ctx.set_block_light_at(BlockPosition::new(0, 1, 0), 15);

        opaque_non_emitting_removal(&mut ctx, BlockPosition::new(0, 1, 0));

        assert_eq!(ctx.block_light_at(BlockPosition::new(0, 1, 0)), 11);
    }

    #[test]
    fn test_flood_fill() {
        let mut chunk_map = chunk_map();
        let mut ctx = Context::new(&mut chunk_map, ChunkPosition::new(0, 0)).unwrap();

        let mut count = 0;

        flood_fill(&mut ctx, BlockPosition::new(100, 100, 100), 1, |_, _| {
            count += 1
        });

        assert_eq!(count, 6);
    }

    fn chunk_map() -> ChunkMap {
        let mut chunk_map = ChunkMap::new();

        for x in -1..=1 {
            for z in -1..=1 {
                let pos = ChunkPosition::new(x, z);
                chunk_map.set_chunk_at(pos, Chunk::new(pos));
            }
        }

        chunk_map
    }
}
