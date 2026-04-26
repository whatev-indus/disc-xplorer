// SPDX-License-Identifier: (MIT OR Apache-2.0)

use std::{fmt, str};

use time::OffsetDateTime;

use crate::parse::{DirectoryEntryHeader, FileFlags};
use crate::{DirectoryEntry, FileRef, ISO9660Reader, ISOError, Result};

pub struct ISODirectory<T: ISO9660Reader> {
    pub(crate) header: DirectoryEntryHeader,
    pub identifier: String,
    file: FileRef<T>,
}

impl<T: ISO9660Reader> Clone for ISODirectory<T> {
    fn clone(&self) -> ISODirectory<T> {
        ISODirectory {
            header: self.header.clone(),
            identifier: self.identifier.clone(),
            file: self.file.clone(),
        }
    }
}

impl<T: ISO9660Reader> fmt::Debug for ISODirectory<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("ISOFile")
            .field("header", &self.header)
            .field("identifier", &self.identifier)
            .finish()
    }
}

impl<T: ISO9660Reader> ISODirectory<T> {
    pub(crate) fn new(
        header: DirectoryEntryHeader,
        mut identifier: String,
        file: FileRef<T>,
    ) -> ISODirectory<T> {
        if &identifier == "\u{0}" {
            identifier = ".".to_string();
        } else if &identifier == "\u{1}" {
            identifier = "..".to_string();
        }

        ISODirectory {
            header,
            identifier,
            file,
        }
    }

    pub fn block_count(&self) -> u32 {
        let len = self.header.extent_length;
        (len + 2048 - 1) / 2048 // ceil(len / 2048)
    }

    pub fn read_entry_at(
        &self,
        block: &mut [u8; 2048],
        buf_block_num: &mut Option<u64>,
        offset: u64,
    ) -> Result<(DirectoryEntry<T>, Option<u64>)> {
        let mut block_num = offset / 2048;
        let mut block_pos = (offset % 2048) as usize;

        if buf_block_num != &Some(block_num) {
            let lba = self.header.extent_loc as u64 + block_num;
            let count = self.file.read_at(block, lba)?;

            if count != 2048 {
                *buf_block_num = None;
                return Err(ISOError::ReadSize(2048, count));
            }

            *buf_block_num = Some(block_num);
        }

        let (header, identifier) = DirectoryEntryHeader::parse(&block[block_pos..])?;
        block_pos += header.length as usize;

        let entry = DirectoryEntry::new(header, identifier, self.file.clone())?;

        // All bytes after the last directory entry are zero.
        if block_pos >= (2048 - 33) || block[block_pos] == 0 {
            block_num += 1;
            block_pos = 0;
        }

        let next_offset = if block_num < self.block_count() as u64 {
            Some(2048 * block_num + block_pos as u64)
        } else {
            None
        };

        Ok((entry, next_offset))
    }

    pub fn contents(&self) -> ISODirectoryIterator<T> {
        ISODirectoryIterator {
            directory: self,
            block: [0; 2048],
            block_num: None,
            next_offset: Some(0),
        }
    }

    pub fn time(&self) -> OffsetDateTime {
        self.header.time
    }

    pub fn find(&self, identifier: &str) -> Result<Option<DirectoryEntry<T>>> {
        for entry in self.contents() {
            let entry = entry?;
            if entry
                .header()
                .file_flags
                .contains(FileFlags::ASSOCIATEDFILE)
            {
                continue;
            }
            if entry.identifier().eq_ignore_ascii_case(identifier) {
                return Ok(Some(entry));
            }
        }

        Ok(None)
    }
}

pub struct ISODirectoryIterator<'a, T: ISO9660Reader> {
    directory: &'a ISODirectory<T>,
    next_offset: Option<u64>,
    block: [u8; 2048],
    block_num: Option<u64>,
}

impl<'a, T: ISO9660Reader> Iterator for ISODirectoryIterator<'a, T> {
    type Item = Result<DirectoryEntry<T>>;

    fn next(&mut self) -> Option<Result<DirectoryEntry<T>>> {
        let offset = self.next_offset?;
        match self
            .directory
            .read_entry_at(&mut self.block, &mut self.block_num, offset)
        {
            Ok((entry, next_offset)) => {
                self.next_offset = next_offset;
                Some(Ok(entry))
            }
            Err(err) => Some(Err(err)),
        }
    }
}
