//! Directory modification: adding, removing, and retargeting entries, for
//! linear and hashed-index (htree) directories.

use crate::bytes::*;
use crate::dir::*;
use crate::error::{Error, Result};
use crate::hash;
use crate::inode::{FileType, Inode, FL_CASEFOLD, FL_ENCRYPT, FL_INDEX, FL_INLINE_DATA};
use crate::superblock::compat;
use crate::volume::Volume;

impl Volume {
    fn dir_seed(&self, ino: u32, inode: &Inode) -> u32 {
        Inode::csum_seed(&self.sb, ino, inode.generation())
    }

    fn csum(&self) -> bool {
        self.sb.has_metadata_csum()
    }

    /// End of the entry area of a leaf block (excludes the checksum tail).
    fn leaf_end(&self) -> usize {
        if self.csum() {
            self.block_size - TAIL_SIZE
        } else {
            self.block_size
        }
    }

    /// Physical block of directory logical block `lblk`.
    fn dir_phys(&self, ino: u32, inode: &Inode, lblk: u64) -> Result<u64> {
        match self.map_blocks(ino, inode, lblk, 1)? {
            crate::bmap::Mapping::Data { physical, .. } => Ok(physical),
            _ => Err(Error::corrupt(format!(
                "directory {ino} has a hole at block {lblk}"
            ))),
        }
    }

    /// Recompute the leaf checksum tail of `buf`.
    fn seal_leaf(&self, ino: u32, inode: &Inode, buf: &mut [u8]) {
        if self.csum() && has_tail(buf) {
            let c = leaf_checksum(self.dir_seed(ino, inode), buf);
            let n = buf.len();
            put32(buf, n - 4, c);
        }
    }

    /// Recompute the dx tail checksum of an htree node.
    fn seal_dx(&self, ino: u32, inode: &Inode, buf: &mut [u8], is_root: bool) {
        if !self.csum() {
            return;
        }
        if let Some(co) = dx_count_offset(buf, is_root, self.block_size) {
            let limit = le16(buf, co) as usize;
            let count = le16(buf, co + 2) as usize;
            let tail = co + limit * 8;
            if tail + 8 <= buf.len() {
                let c = dx_checksum(self.dir_seed(ino, inode), buf, co, count, tail);
                put32(buf, tail + 4, c);
            }
        }
    }

    /// Modify directory block `lblk` through `f`, then reseal it. Block 0
    /// of an htree is the dx root, which has a dx tail rather than a leaf
    /// tail (its ".." record can change on rename).
    fn edit_leaf<T>(
        &mut self,
        ino: u32,
        inode: &Inode,
        lblk: u64,
        f: impl FnOnce(&mut Vec<u8>) -> T,
    ) -> Result<T> {
        let phys = self.dir_phys(ino, inode, lblk)?;
        // Read through dir_block for checksum verification.
        self.dir_block(ino, inode, lblk)?;
        let mut buf = self.block_mut(phys)?.clone();
        let r = f(&mut buf);
        if lblk == 0 && inode.has_flag(FL_INDEX) && !has_tail(&buf) {
            self.seal_dx(ino, inode, &mut buf, true);
        } else {
            self.seal_leaf(ino, inode, &mut buf);
        }
        self.block_mut(phys)?.copy_from_slice(&buf);
        Ok(r)
    }

    /// Append a new zeroed block to the directory; returns its logical number.
    fn grow_dir(&mut self, ino: u32, inode: &mut Inode) -> Result<(u64, u64)> {
        let lblk = self.dir_block_count(inode);
        let bs = self.block_size;
        // Write an empty leaf through the normal file path so the block map
        // is updated, then stage it as metadata.
        let mut empty = vec![0u8; bs];
        let end = self.leaf_end();
        put_entry(&mut empty, 0, 0, end, &[], 0, bs);
        if self.csum() {
            put_tail(&mut empty);
        }
        let new_size = (lblk + 1) * bs as u64;
        self.write_file(ino, inode, lblk * bs as u64, &empty)?;
        inode.set_size(new_size);
        let phys = self.dir_phys(ino, inode, lblk)?;
        self.seal_leaf(ino, inode, &mut empty);
        self.new_block_mut(phys)?.copy_from_slice(&empty);
        Ok((lblk, phys))
    }

    /// Try to place an entry in a leaf buffer. Returns true on success.
    fn insert_into_leaf(&self, buf: &mut [u8], name: &[u8], child: u32, ft: u8) -> Result<bool> {
        let need = rec_len_for(name.len());
        let end = if has_tail(buf) {
            buf.len() - TAIL_SIZE
        } else {
            buf.len()
        };
        let entries = parse_entries(buf, 0, end, self.has_filetype(), self.block_size)?;
        for e in entries {
            if e.inode == 0 && e.rec_len >= need {
                put_entry(buf, e.offset, child, e.rec_len, name, ft, self.block_size);
                return Ok(true);
            }
            let used = rec_len_for(e.name_len);
            if e.inode != 0 && e.rec_len >= used + need {
                put16(buf, e.offset + 4, encode_rec_len(used, self.block_size));
                put_entry(
                    buf,
                    e.offset + used,
                    child,
                    e.rec_len - used,
                    name,
                    ft,
                    self.block_size,
                );
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn dirent_ft(&self, ft: FileType) -> u8 {
        if self.has_filetype() {
            ft.dirent_type()
        } else {
            0
        }
    }

    /// Add `name -> child` to directory `ino`. Updates `inode` in memory
    /// (size/blocks may change); the caller writes it.
    pub(crate) fn add_entry(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        name: &[u8],
        child: u32,
        ft: FileType,
    ) -> Result<()> {
        if inode.has_flag(FL_INLINE_DATA) {
            self.uninline(ino, inode)?;
        }
        let ft = self.dirent_ft(ft);
        if inode.has_flag(FL_INDEX) {
            if self.sb.has_compat(compat::DIR_INDEX) {
                return self.dx_add(ino, inode, name, child, ft);
            }
            // Index flag without the feature: Linux treats it as linear and
            // clears the flag on modification.
            inode.set_flags(inode.flags() & !FL_INDEX);
        }
        let nblocks = self.dir_block_count(inode);
        for lblk in 0..nblocks {
            let b = self.dir_block(ino, inode, lblk)?;
            let mut copy = (*b).clone();
            if self.csum() && !has_tail(&copy) {
                continue;
            }
            if self.insert_into_leaf(&mut copy, name, child, ft)? {
                self.edit_leaf(ino, inode, lblk, |buf| buf.copy_from_slice(&copy))?;
                return Ok(());
            }
        }
        if nblocks == 1
            && self.sb.has_compat(compat::DIR_INDEX)
            && self.make_indexed_dir(ino, inode, name, child, ft)?
        {
            return Ok(());
        }
        let (lblk, _) = self.grow_dir(ino, inode)?;
        let ok = self.edit_leaf(ino, inode, lblk, |buf| {
            let bs = buf.len();
            self_insert(buf, name, child, ft, bs)
        })?;
        if !ok {
            return Err(Error::corrupt("new directory block has no room"));
        }
        Ok(())
    }

    /// Remove the entry for `name`; returns the removed entry.
    pub(crate) fn remove_entry(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        name: &[u8],
    ) -> Result<DirEntry> {
        if inode.has_flag(FL_INLINE_DATA) {
            self.uninline(ino, inode)?;
        }
        let (entry, (lblk, off)) = self.find_entry(ino, inode, name)?.ok_or(Error::NotFound)?;
        let filetype = self.has_filetype();
        let bs = self.block_size;
        self.edit_leaf(ino, inode, lblk, |buf| -> Result<()> {
            let end = if has_tail(buf) {
                buf.len() - TAIL_SIZE
            } else {
                buf.len()
            };
            let entries = parse_entries(buf, 0, end, filetype, bs)?;
            let pos = entries
                .iter()
                .position(|e| e.offset == off)
                .ok_or_else(|| Error::corrupt("entry vanished"))?;
            if pos > 0 {
                let prev = entries[pos - 1];
                let merged = prev.rec_len + entries[pos].rec_len;
                put16(buf, prev.offset + 4, encode_rec_len(merged, bs));
            } else {
                put32(buf, off, 0);
            }
            Ok(())
        })??;
        Ok(entry)
    }

    /// Point an existing entry at a different inode (rename over a target).
    pub(crate) fn retarget_entry(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        name: &[u8],
        child: u32,
        ft: FileType,
    ) -> Result<()> {
        if inode.has_flag(FL_INLINE_DATA) {
            self.uninline(ino, inode)?;
        }
        let ft = self.dirent_ft(ft);
        let (_, (lblk, off)) = self.find_entry(ino, inode, name)?.ok_or(Error::NotFound)?;
        self.edit_leaf(ino, inode, lblk, |buf| {
            put32(buf, off, child);
            buf[off + 7] = ft;
        })
    }

    /// Create the first block of a new directory with ".", "..", and
    /// optionally more entries. Sets the size.
    pub(crate) fn init_dir_block(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        parent: u32,
        extra: &[DirEntry],
    ) -> Result<()> {
        let bs = self.block_size;
        let ft_dir = self.dirent_ft(FileType::Directory);
        let mut buf = vec![0u8; bs];
        let end = self.leaf_end();
        put_entry(&mut buf, 0, ino, 12, b".", ft_dir, bs);
        put_entry(&mut buf, 12, parent, end - 12, b"..", ft_dir, bs);
        if self.csum() {
            put_tail(&mut buf);
        }
        for e in extra {
            let ft = self.dirent_ft(e.file_type);
            if !self.insert_into_leaf(&mut buf, &e.name, e.inode, ft)? {
                return Err(Error::NoSpace);
            }
        }
        inode.set_size(0);
        self.write_file(ino, inode, 0, &buf)?;
        inode.set_size(bs as u64);
        let phys = self.dir_phys(ino, inode, 0)?;
        self.seal_leaf(ino, inode, &mut buf);
        self.new_block_mut(phys)?.copy_from_slice(&buf);
        Ok(())
    }

    /// True if the directory holds nothing but "." and "..".
    pub(crate) fn dir_is_empty(&self, ino: u32) -> Result<bool> {
        let mut empty = true;
        self.read_dir(ino, 0, |e, _| {
            if e.name != b"." && e.name != b".." {
                empty = false;
                return false;
            }
            true
        })?;
        Ok(empty)
    }

    // ---- htree insertion ---------------------------------------------

    fn dx_limit(&self, count_off: usize) -> usize {
        let tail = if self.csum() { 8 } else { 0 };
        (self.block_size - count_off - tail) / 8
    }

    fn dx_add(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        name: &[u8],
        child: u32,
        ft: u8,
    ) -> Result<()> {
        let root = self.dir_block(ino, inode, 0)?;
        let info = self.dx_info(&root)?;
        let (hash, _) = hash::dirhash(info.hash_version, name, self.sb.hash_seed())
            .ok_or_else(|| Error::unsupported("hash version"))?;
        let path = self.dx_path(ino, inode, hash, info)?;
        let leaf_lblk = path.last().expect("path has the root").child();
        let leaf = self.dir_block(ino, inode, leaf_lblk)?;
        let mut copy = (*leaf).clone();
        if self.insert_into_leaf(&mut copy, name, child, ft)? {
            return self.edit_leaf(ino, inode, leaf_lblk, |buf| buf.copy_from_slice(&copy));
        }
        self.dx_split_insert(ino, inode, info, path, hash, name, child, ft)
    }

    /// Turn a full one-block linear directory into an htree, as Linux
    /// `make_indexed_dir` does: the entries after "." and ".." move to a new
    /// leaf, block 0 becomes the dx root, and the new leaf is split by hash
    /// before `name` is inserted. Returns false, changing nothing, when the
    /// directory cannot be indexed and should grow linearly instead.
    fn make_indexed_dir(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        name: &[u8],
        child: u32,
        ft: u8,
    ) -> Result<bool> {
        // Casefolded and encrypted names hash differently; leave them linear.
        if inode.flags() & (FL_CASEFOLD | FL_ENCRYPT) != 0 {
            return Ok(false);
        }
        let version = self.sb.def_hash_version();
        if version > hash::TEA {
            return Ok(false);
        }
        let bs = self.block_size;
        let block0 = self.dir_block(ino, inode, 0)?;
        let end = if has_tail(&block0) {
            bs - TAIL_SIZE
        } else {
            bs
        };
        let entries = parse_entries(&block0, 0, end, self.has_filetype(), bs)?;
        let (dot, dotdot) = match entries.as_slice() {
            [d, dd, ..]
                if d.offset == 0
                    && d.rec_len == 12
                    && d.name(&block0) == b"."
                    && dd.offset == 12
                    && dd.name(&block0) == b".." =>
            {
                (*d, *dd)
            }
            _ => return Ok(false),
        };
        let moved: Vec<RawEntry> = entries[2..]
            .iter()
            .filter(|e| e.inode != 0)
            .copied()
            .collect();
        if moved.is_empty() {
            return Ok(false);
        }

        // The new leaf holds the moved entries in their old order.
        let leaf_end = self.leaf_end();
        let mut leaf = vec![0u8; bs];
        let mut off = 0;
        for (i, e) in moved.iter().enumerate() {
            let len = if i + 1 == moved.len() {
                leaf_end - off
            } else {
                rec_len_for(e.name_len)
            };
            put_entry(
                &mut leaf,
                off,
                e.inode,
                len,
                e.name(&block0),
                e.file_type,
                bs,
            );
            off += len;
        }
        if leaf_end < bs {
            put_tail(&mut leaf);
        }

        // Block 0 becomes the root: ".", ".." covering the rest of the
        // block, the dx_root_info, and one index entry for logical block 1.
        let mut root = vec![0u8; bs];
        put_entry(&mut root, 0, dot.inode, 12, b".", dot.file_type, bs);
        put_entry(
            &mut root,
            12,
            dotdot.inode,
            bs - 12,
            b"..",
            dotdot.file_type,
            bs,
        );
        root[24 + 4] = version;
        root[24 + 5] = 8; // info_length
        let co = 32;
        put16(&mut root, co, self.dx_limit(co) as u16);
        put16(&mut root, co + 2, 1);
        put32(&mut root, co + 4, 1);

        let (leaf_lblk, leaf_phys) = self.grow_dir(ino, inode)?;
        debug_assert_eq!(leaf_lblk, 1);
        inode.set_flags(inode.flags() | FL_INDEX);
        self.seal_leaf(ino, inode, &mut leaf);
        self.block_mut(leaf_phys)?.copy_from_slice(&leaf);
        self.seal_dx(ino, inode, &mut root, true);
        let root_phys = self.dir_phys(ino, inode, 0)?;
        self.block_mut(root_phys)?.copy_from_slice(&root);

        let info = self.dx_info(&root)?;
        let (hash, _) = hash::dirhash(info.hash_version, name, self.sb.hash_seed())
            .ok_or_else(|| Error::unsupported("hash version"))?;
        let path = self.dx_path(ino, inode, hash, info)?;
        self.dx_split_insert(ino, inode, info, path, hash, name, child, ft)?;
        Ok(true)
    }

    /// Split the leaf at the end of `path` by hash into itself and a new
    /// block, put `name` into the proper half, and index the new block.
    #[allow(clippy::too_many_arguments)]
    fn dx_split_insert(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        info: DxInfo,
        path: Vec<DxLevel>,
        hash: u32,
        name: &[u8],
        child: u32,
        ft: u8,
    ) -> Result<()> {
        let seed = self.sb.hash_seed();
        let leaf_lblk = path.last().expect("path has the root").child();
        let leaf = self.dir_block(ino, inode, leaf_lblk)?;

        // Split the leaf. Gather its live entries with their hashes.
        let filetype = self.has_filetype();
        let end = if has_tail(&leaf) {
            leaf.len() - TAIL_SIZE
        } else {
            leaf.len()
        };
        let mut items: Vec<(u32, Vec<u8>, u32, u8)> = Vec::new();
        for e in parse_entries(&leaf, 0, end, filetype, self.block_size)? {
            if e.inode == 0 {
                continue;
            }
            let n = e.name(&leaf).to_vec();
            let (h, _) = hash::dirhash(info.hash_version, &n, seed)
                .ok_or_else(|| Error::unsupported("hash version"))?;
            items.push((h, n, e.inode, e.file_type));
        }
        items.sort_by_key(|i| i.0);
        // Split near half of the bytes, never inside a run of equal hashes
        // unless the run fills the block (then mark a continuation).
        let total: usize = items.iter().map(|i| rec_len_for(i.1.len())).sum();
        let mut acc = 0;
        let mut split = items.len() / 2;
        for (i, it) in items.iter().enumerate() {
            acc += rec_len_for(it.1.len());
            if acc >= total / 2 {
                split = i + 1;
                break;
            }
        }
        split = split.clamp(1, items.len().saturating_sub(1).max(1));
        let mut split_hash = items.get(split).map_or(0xFFFF_FFFE, |i| i.0);
        let mut continued = false;
        if split > 0 && items[split - 1].0 == split_hash {
            // Move the split to the start of the equal-hash run if possible.
            let mut s = split;
            while s > 0 && items[s - 1].0 == split_hash {
                s -= 1;
            }
            if s > 0 {
                split = s;
                split_hash = items[split].0;
            } else {
                continued = true;
            }
        }
        let (lower, upper) = items.split_at(split);

        let (new_lblk, new_phys) = self.grow_dir(ino, inode)?;
        let bs = self.block_size;
        let leaf_end = self.leaf_end();
        let build = |entries: &[(u32, Vec<u8>, u32, u8)]| -> Vec<u8> {
            let mut b = vec![0u8; bs];
            let mut off = 0;
            for (i, (_, n, ino_, t)) in entries.iter().enumerate() {
                let len = if i + 1 == entries.len() {
                    leaf_end - off
                } else {
                    rec_len_for(n.len())
                };
                put_entry(&mut b, off, *ino_, len, n, *t, bs);
                off += len;
            }
            if entries.is_empty() {
                put_entry(&mut b, 0, 0, leaf_end, &[], 0, bs);
            }
            if leaf_end < bs {
                put_tail(&mut b);
            }
            b
        };
        let mut low_buf = build(lower);
        let mut high_buf = build(upper);
        // Insert the new entry into the proper half.
        let target_high = hash >= split_hash;
        let placed = if target_high {
            self.insert_into_leaf(&mut high_buf, name, child, ft)?
        } else {
            self.insert_into_leaf(&mut low_buf, name, child, ft)?
        };
        if !placed {
            return Err(Error::NoSpace);
        }
        self.seal_leaf(ino, inode, &mut low_buf);
        self.seal_leaf(ino, inode, &mut high_buf);
        let leaf_phys = self.dir_phys(ino, inode, leaf_lblk)?;
        self.block_mut(leaf_phys)?.copy_from_slice(&low_buf);
        self.block_mut(new_phys)?.copy_from_slice(&high_buf);

        let key = split_hash | u32::from(continued);
        self.dx_insert_index(ino, inode, info, path, key, new_lblk)
    }

    /// Insert `(key, lblk)` after the chosen entry of the deepest node in
    /// `path`, splitting nodes and growing the tree as needed.
    fn dx_insert_index(
        &mut self,
        ino: u32,
        inode: &mut Inode,
        info: DxInfo,
        path: Vec<DxLevel>,
        key: u32,
        lblk: u64,
    ) -> Result<()> {
        let mut level = path.len() - 1;
        let mut key = key;
        let mut lblk = lblk;
        // Work on owned copies of each level's buffer.
        let mut bufs: Vec<(u64, Vec<u8>, usize, usize)> = path
            .iter()
            .map(|l| (l.lblk, (*l.buf).clone(), l.count_off, l.idx))
            .collect();
        loop {
            let (node_lblk, ref mut buf, co, idx) = bufs[level];
            let limit = le16(buf, co) as usize;
            let count = le16(buf, co + 2) as usize;
            if count < limit {
                insert_dx_entry(buf, co, idx + 1, key, lblk as u32);
                let mut b = buf.clone();
                self.seal_dx(ino, inode, &mut b, level == 0);
                let phys = self.dir_phys(ino, inode, node_lblk)?;
                self.block_mut(phys)?.copy_from_slice(&b);
                return Ok(());
            }
            let max_levels = if self.sb.has_incompat(crate::superblock::incompat::LARGEDIR) {
                3
            } else {
                2
            };
            if level == 0 {
                // Root full: push its entries down into a new node.
                let levels = buf[30];
                if levels as usize + 1 >= max_levels {
                    return Err(Error::NoSpace);
                }
                let (child_lblk, child_phys) = self.grow_dir(ino, inode)?;
                let bs = self.block_size;
                let mut node = vec![0u8; bs];
                put32(&mut node, 0, 0);
                put16(&mut node, 4, encode_rec_len(bs, bs));
                node[6] = 0;
                node[7] = 0;
                let nco = 8;
                let nlimit = self.dx_limit(nco);
                node[nco + 4..nco + 4 + (count * 8 - 4)]
                    .copy_from_slice(&buf[co + 4..co + count * 8]);
                put16(&mut node, nco, nlimit as u16);
                put16(&mut node, nco + 2, count as u16);
                // Root now has one entry pointing at the new node.
                for b in &mut buf[co + 4..co + limit * 8] {
                    *b = 0;
                }
                put16(buf, co + 2, 1);
                put32(buf, co + 4, child_lblk as u32);
                buf[30] = levels + 1;
                let mut rb = buf.clone();
                self.seal_dx(ino, inode, &mut rb, true);
                let rphys = self.dir_phys(ino, inode, 0)?;
                self.block_mut(rphys)?.copy_from_slice(&rb);
                // Continue inserting into the new node at the same index.
                bufs[0].3 = 0;
                bufs.insert(1, (child_lblk, node.clone(), nco, idx));
                {
                    let mut nb = node;
                    self.seal_dx(ino, inode, &mut nb, false);
                    self.new_block_mut(child_phys)?.copy_from_slice(&nb);
                }
                level = 1;
                let _ = info;
                continue;
            }
            // Split a full interior node in half and insert into the right half.
            let half = count / 2;
            let (new_lblk, new_phys) = self.grow_dir(ino, inode)?;
            let bs = self.block_size;
            let mut right = vec![0u8; bs];
            put16(&mut right, 4, encode_rec_len(bs, bs));
            let rco = 8;
            let moved = count - half;
            let split_key = le32(buf, co + half * 8);
            // Entry 0 of the right node carries only a block pointer.
            right[rco + 4..rco + 8].copy_from_slice(&buf[co + half * 8 + 4..co + half * 8 + 8]);
            right[rco + 8..rco + moved * 8]
                .copy_from_slice(&buf[co + (half + 1) * 8..co + count * 8]);
            put16(&mut right, rco, self.dx_limit(rco) as u16);
            put16(&mut right, rco + 2, moved as u16);
            for b in &mut buf[co + half * 8..co + count * 8] {
                *b = 0;
            }
            put16(buf, co + 2, half as u16);
            // Place the pending entry.
            if idx < half {
                insert_dx_entry(buf, co, idx + 1, key, lblk as u32);
            } else {
                let ridx = idx + 1 - half;
                insert_dx_entry(&mut right, rco, ridx, key, lblk as u32);
            }
            let mut lb = buf.clone();
            self.seal_dx(ino, inode, &mut lb, false);
            let lphys = self.dir_phys(ino, inode, node_lblk)?;
            self.block_mut(lphys)?.copy_from_slice(&lb);
            self.seal_dx(ino, inode, &mut right, false);
            self.new_block_mut(new_phys)?.copy_from_slice(&right);
            // Now insert the new node into the parent.
            key = split_key;
            lblk = new_lblk;
            level -= 1;
        }
    }
}

/// Insert a dx entry at position `pos` (>= 1), shifting later entries.
fn insert_dx_entry(buf: &mut [u8], co: usize, pos: usize, key: u32, lblk: u32) {
    let count = le16(buf, co + 2) as usize;
    let start = co + pos * 8;
    let end = co + count * 8;
    buf.copy_within(start..end, start + 8);
    put32(buf, start, key);
    put32(buf, start + 4, lblk);
    put16(buf, co + 2, (count + 1) as u16);
}

/// Insert into a fresh leaf whose first entry is an empty record.
fn self_insert(buf: &mut [u8], name: &[u8], child: u32, ft: u8, bs: usize) -> bool {
    let end = if has_tail(buf) {
        buf.len() - TAIL_SIZE
    } else {
        buf.len()
    };
    if le32(buf, 0) == 0 && decode_rec_len(le16(buf, 4), bs) == end {
        put_entry(buf, 0, child, end, name, ft, bs);
        true
    } else {
        false
    }
}
