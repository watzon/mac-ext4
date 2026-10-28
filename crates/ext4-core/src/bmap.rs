//! Logical-to-physical block mapping for both extent and block-map files.

use crate::error::{Error, Result};
use crate::inode::{Inode, FL_EXTENTS, FL_INLINE_DATA};
use crate::journal::Run;
use crate::volume::Volume;

/// The mapping of a run of logical blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// `len` unmapped blocks (read as zeros).
    Hole(u64),
    /// `len` blocks at `physical`.
    Data { physical: u64, len: u64 },
    /// `len` preallocated blocks at `physical` (read as zeros).
    Unwritten { physical: u64, len: u64 },
}

impl Mapping {
    /// Number of logical blocks the mapping covers (never zero).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        match *self {
            Mapping::Hole(l) => l,
            Mapping::Data { len, .. } | Mapping::Unwritten { len, .. } => len,
        }
    }
}

impl Volume {
    /// Map up to `max` logical blocks starting at `lblk`.
    pub fn map_blocks(&self, ino: u32, inode: &Inode, lblk: u64, max: u64) -> Result<Mapping> {
        debug_assert!(max > 0);
        if inode.has_flag(FL_INLINE_DATA) {
            return Err(Error::invalid("inline-data inodes have no block map"));
        }
        if inode.has_flag(FL_EXTENTS) {
            if lblk > u32::MAX as u64 {
                return Ok(Mapping::Hole(max));
            }
            let (found, next) = self.extent_lookup(inode, ino, lblk as u32)?;
            return Ok(match found {
                Some(e) => {
                    let skip = lblk - e.logical as u64;
                    let len = (e.len as u64 - skip).min(max);
                    let physical = e.physical + skip;
                    if e.unwritten {
                        Mapping::Unwritten { physical, len }
                    } else {
                        Mapping::Data { physical, len }
                    }
                }
                None => Mapping::Hole(next.map_or(max, |n| (n as u64 - lblk).min(max))),
            });
        }
        // Block map: coalesce physically contiguous blocks.
        let first = self.blockmap_lookup(inode, lblk)?;
        let mut len = 1;
        while len < max {
            let next = self.blockmap_lookup(inode, lblk + len)?;
            let contiguous = if first == 0 {
                next == 0
            } else {
                next == first + len
            };
            if !contiguous {
                break;
            }
            len += 1;
        }
        Ok(if first == 0 {
            Mapping::Hole(len)
        } else {
            Mapping::Data {
                physical: first,
                len,
            }
        })
    }

    /// Physical runs of every mapped block (used for the journal inode).
    pub fn physical_runs_of(&self, ino: u32, inode: &Inode) -> Result<Vec<Run>> {
        let mut runs: Vec<Run> = Vec::new();
        let mut push = |logical: u64, physical: u64, len: u64| {
            if let Some(last) = runs.last_mut() {
                if last.logical + last.len == logical && last.physical + last.len == physical {
                    last.len += len;
                    return;
                }
            }
            runs.push(Run {
                logical,
                physical,
                len,
            });
        };
        if inode.has_flag(FL_EXTENTS) {
            let (extents, _) = self.read_extents(inode, ino)?;
            for e in extents {
                push(e.logical as u64, e.physical, e.len as u64);
            }
        } else {
            self.blockmap_walk(inode, |l, p| {
                push(l, p, 1);
                Ok(())
            })?;
        }
        Ok(runs)
    }
}

/// Physical runs of the journal inode.
pub fn physical_runs(vol: &Volume, inode: &Inode) -> Result<Vec<Run>> {
    vol.physical_runs_of(vol.sb.journal_inum(), inode)
}
