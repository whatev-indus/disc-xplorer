// SPDX-License-Identifier: (MIT OR Apache-2.0)

#![cfg_attr(feature = "nightly", feature(read_initializer, specialization))]

extern crate time;
#[macro_use]
extern crate bitflags;
extern crate nom;

use std::result;

pub use directory_entry::{
    DirectoryEntry, ISODirectory, ISODirectoryIterator, ISOFile, ISOFileReader,
};
pub use error::ISOError;
pub(crate) use fileref::FileRef;
pub use fileref::ISO9660Reader;
use parse::VolumeDescriptor;

pub type Result<T> = result::Result<T, ISOError>;

mod directory_entry;
mod error;
mod fileref;
mod parse;

pub struct ISO9660<T: ISO9660Reader> {
    _file: FileRef<T>,
    pub root: ISODirectory<T>,
    primary: VolumeDescriptor,
}

macro_rules! primary_prop_str {
    ($name:ident) => {
        pub fn $name(&self) -> &str {
            if let VolumeDescriptor::Primary { $name, .. } = &self.primary {
                &$name
            } else {
                unreachable!()
            }
        }
    };
}

impl<T: ISO9660Reader> ISO9660<T> {
    pub fn new(mut reader: T) -> Result<ISO9660<T>> {
        let mut buf: [u8; 2048] = [0; 2048];
        let mut root = None;
        let mut primary = None;

        // Skip the "system area"
        let mut lba = 16;

        // Read volume descriptors
        loop {
            let count = reader.read_at(&mut buf, lba)?;

            if count != 2048 {
                return Err(ISOError::ReadSize(2048, count));
            }

            let descriptor = VolumeDescriptor::parse(&buf)?;
            match &descriptor {
                Some(VolumeDescriptor::Primary {
                    logical_block_size,
                    root_directory_entry,
                    root_directory_entry_identifier,
                    ..
                }) => {
                    // CD-i (Green Book) stores block size as LE=0, BE=2048;
                    // accept 0 as equivalent to 2048.
                    if *logical_block_size != 2048 && *logical_block_size != 0 {
                        return Err(ISOError::InvalidFs("Block size not 2048"));
                    }

                    root = Some((
                        root_directory_entry.clone(),
                        root_directory_entry_identifier.clone(),
                    ));
                    primary = descriptor;
                }
                Some(VolumeDescriptor::VolumeDescriptorSetTerminator) => break,
                _ => {}
            }

            lba += 1;
        }

        let file = FileRef::new(reader);
        let file2 = file.clone();

        let (root, primary) = match (root, primary) {
            (Some(root), Some(primary)) => (root, primary),
            _ => {
                return Err(ISOError::InvalidFs("No primary volume descriptor"));
            }
        };

        Ok(ISO9660 {
            _file: file,
            root: ISODirectory::new(root.0, root.1, file2),
            primary,
        })
    }

    pub fn open(&self, path: &str) -> Result<Option<DirectoryEntry<T>>> {
        // TODO: avoid clone()
        let mut entry = DirectoryEntry::Directory(self.root.clone());
        for segment in path.split('/').filter(|x| !x.is_empty()) {
            let parent = match entry {
                DirectoryEntry::Directory(dir) => dir,
                _ => return Ok(None),
            };

            entry = match parent.find(segment)? {
                Some(entry) => entry,
                None => return Ok(None),
            };
        }

        Ok(Some(entry))
    }

    pub fn block_size(&self) -> u16 {
        2048 // XXX
    }

    primary_prop_str!(volume_set_identifier);
    primary_prop_str!(publisher_identifier);
    primary_prop_str!(data_preparer_identifier);
    primary_prop_str!(application_identifier);
    primary_prop_str!(copyright_file_identifier);
    primary_prop_str!(abstract_file_identifier);
    primary_prop_str!(bibliographic_file_identifier);
}
