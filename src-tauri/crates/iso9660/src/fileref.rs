// SPDX-License-Identifier: (MIT OR Apache-2.0)

use std::cell::RefCell;
#[cfg(feature = "nightly")]
use std::fs::File;
use std::io::{Read, Result, Seek, SeekFrom};
use std::rc::Rc;

pub trait ISO9660Reader {
    /// Read the block(s) at a given LBA (logical block address)
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> Result<usize>;
}

#[cfg(not(feature = "nightly"))]
impl<T: Read + Seek> ISO9660Reader for T {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> Result<usize> {
        self.seek(SeekFrom::Start(lba * 2048))?;
        self.read(buf)
    }
}

#[cfg(feature = "nightly")]
impl<T: Read + Seek> ISO9660Reader for T {
    default fn read_at(&mut self, buf: &mut [u8], lba: u64) -> Result<usize> {
        self.seek(SeekFrom::Start(lba * 2048))?;
        self.read(buf)?
    }
}

#[cfg(feature = "nightly")]
impl ISO9660Reader for File {
    fn read_at(&mut self, buf: &mut [u8], lba: u64) -> Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            FileExt::read_at(self, buf, lba * 2048)?
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            self.seek(SeekFrom::Start(lba * 2048))?;
            self.read(buf)?
        }
    }
}

// TODO: Figure out if sane API possible without Rc/RefCell
pub(crate) struct FileRef<T: ISO9660Reader>(Rc<RefCell<T>>);

impl<T: ISO9660Reader> Clone for FileRef<T> {
    fn clone(&self) -> FileRef<T> {
        FileRef(self.0.clone())
    }
}

impl<T: ISO9660Reader> FileRef<T> {
    pub fn new(reader: T) -> FileRef<T> {
        FileRef(Rc::new(RefCell::new(reader)))
    }

    /// Read the block(s) at a given LBA (logical block address)
    pub fn read_at(&self, buf: &mut [u8], lba: u64) -> Result<usize> {
        (*self.0).borrow_mut().read_at(buf, lba)
    }
}
