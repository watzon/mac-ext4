//! Inline data (`inline_data` feature): small files and directories stored
//! in `i_block` plus the `system.data` extended attribute.

use crate::bytes::*;
use crate::dir::{parse_entries, DirEntry};
use crate::error::Result;
use crate::inode::{FileType, Inode, I_BLOCK_LEN};
use crate::volume::Volume;
use crate::xattr::INDEX_SYSTEM;

/// An inline directory: its parent inode and its entries, each with its
/// offset and record length in the combined inline content.
pub(crate) type InlineDir = (u32, Vec<(usize, usize, DirEntry)>);

impl Volume {
    /// The inline content: `i_block` bytes followed by the `system.data` value.
    pub(crate) fn inline_content(&self, ino: u32, inode: &Inode) -> Result<Vec<u8>> {
        let mut data = inode.i_block().to_vec();
        if let Some(extra) = self.get_xattr(ino, inode, INDEX_SYSTEM, b"data")? {
            data.extend_from_slice(&extra);
        }
        Ok(data)
    }

    pub(crate) fn read_inline_file(
        &self,
        ino: u32,
        inode: &Inode,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        let size = inode.size();
        if offset >= size {
            return Ok(0);
        }
        let data = self.inline_content(ino, inode)?;
        let end = (size.min(offset + buf.len() as u64)) as usize;
        let start = offset as usize;
        let avail = end.min(data.len()).saturating_sub(start);
        buf[..avail].copy_from_slice(&data[start..start + avail]);
        // Bytes past the stored data (should not happen) read as zeros.
        buf[avail..end - start].fill(0);
        Ok(end - start)
    }

    /// Parse an inline directory into (parent, entries with positions).
    /// Positions are offsets into the combined inline content.
    pub(crate) fn inline_dir_entries(&self, ino: u32, inode: &Inode) -> Result<InlineDir> {
        let data = self.inline_content(ino, inode)?;
        let parent = le32(&data, 0);
        let ft = self.has_filetype();
        let mut out = Vec::new();
        let mut push = |raws: Vec<crate::dir::RawEntry>, data: &[u8]| -> Result<()> {
            for e in raws {
                if e.inode == 0 {
                    continue;
                }
                let file_type = if ft {
                    FileType::from_dirent_type(e.file_type)
                } else {
                    self.read_inode(e.inode)?.file_type()
                };
                out.push((
                    e.offset,
                    e.rec_len,
                    DirEntry {
                        inode: e.inode,
                        name: e.name(data).to_vec(),
                        file_type,
                    },
                ));
            }
            Ok(())
        };
        let first = parse_entries(&data, 4, I_BLOCK_LEN, ft, self.block_size)?;
        push(first, &data)?;
        if data.len() > I_BLOCK_LEN {
            let rest = parse_entries(&data, I_BLOCK_LEN, data.len(), ft, self.block_size)?;
            push(rest, &data)?;
        }
        Ok((parent, out))
    }

    pub(crate) fn read_inline_dir(
        &self,
        ino: u32,
        inode: &Inode,
        cookie: u64,
        mut f: impl FnMut(DirEntry, u64) -> bool,
    ) -> Result<()> {
        let (parent, entries) = self.inline_dir_entries(ino, inode)?;
        if cookie == 0
            && !f(
                DirEntry {
                    inode: ino,
                    name: b".".to_vec(),
                    file_type: FileType::Directory,
                },
                1,
            )
        {
            return Ok(());
        }
        if cookie <= 1
            && !f(
                DirEntry {
                    inode: parent,
                    name: b"..".to_vec(),
                    file_type: FileType::Directory,
                },
                2,
            )
        {
            return Ok(());
        }
        for (off, rec_len, e) in entries {
            let pos = 2 + off as u64;
            if pos < cookie {
                continue;
            }
            if !f(e, 2 + (off + rec_len) as u64) {
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) fn find_inline_entry(
        &self,
        ino: u32,
        inode: &Inode,
        name: &[u8],
    ) -> Result<Option<(DirEntry, (u64, usize))>> {
        if name == b"." {
            return Ok(Some((
                DirEntry {
                    inode: ino,
                    name: name.to_vec(),
                    file_type: FileType::Directory,
                },
                (0, 0),
            )));
        }
        let (parent, entries) = self.inline_dir_entries(ino, inode)?;
        if name == b".." {
            return Ok(Some((
                DirEntry {
                    inode: parent,
                    name: name.to_vec(),
                    file_type: FileType::Directory,
                },
                (0, 0),
            )));
        }
        Ok(entries
            .into_iter()
            .find(|(_, _, e)| e.name == name)
            .map(|(off, _, e)| (e, (0, off))))
    }

    /// Move inline content into regular blocks so the normal write paths
    /// apply. The caller must write the inode afterwards.
    pub(crate) fn uninline(&mut self, ino: u32, inode: &mut Inode) -> Result<()> {
        if !inode.has_flag(crate::inode::FL_INLINE_DATA) {
            return Ok(());
        }
        self.ensure_writable()?;
        let is_dir = inode.is_dir();
        let content = self.inline_content(ino, inode)?;
        let dir_entries = if is_dir {
            Some(self.inline_dir_entries(ino, inode)?)
        } else {
            None
        };

        // Drop system.data and the flag; start an empty block map.
        let mut attrs = self.read_xattrs(ino, inode)?;
        attrs.retain(|a| !(a.index == INDEX_SYSTEM && a.name == b"data"));
        inode.set_flags(inode.flags() & !crate::inode::FL_INLINE_DATA);
        inode.i_block_mut().fill(0);
        if self.sb.has_incompat(crate::superblock::incompat::EXTENTS) {
            inode.set_flags(inode.flags() | crate::inode::FL_EXTENTS);
            crate::extent::init_extent_root(inode);
        }
        self.store_xattrs(ino, inode, &attrs)?;

        match dir_entries {
            None => {
                let size = inode.size() as usize;
                inode.set_size(0);
                let data = &content[..size.min(content.len())];
                if !data.is_empty() {
                    self.write_file(ino, inode, 0, data)?;
                }
                inode.set_size(size as u64);
            }
            Some((parent, entries)) => {
                let entries: Vec<DirEntry> = entries.into_iter().map(|(_, _, e)| e).collect();
                self.init_dir_block(ino, inode, parent, &entries)?;
            }
        }
        Ok(())
    }
}
