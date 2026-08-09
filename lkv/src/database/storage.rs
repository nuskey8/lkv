use super::state::BaseBytes;
use crate::error::{Error, Result};
use crate::format::segment;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

/// Represents the storage backend for the database
pub enum Storage {
    File(File),
    Memory(MemoryStorage),
}

impl Storage {
    pub fn memory() -> Self {
        Self::Memory(MemoryStorage::default())
    }

    #[cfg(test)]
    pub fn file(&self) -> Option<&File> {
        match self {
            Self::File(file) => Some(file),
            Self::Memory(_) => None,
        }
    }

    pub fn len(&self) -> Result<u64> {
        match self {
            Self::File(file) => Ok(file.metadata()?.len()),
            Self::Memory(memory) => Ok(memory.bytes.len() as u64),
        }
    }

    pub fn set_len(&mut self, len: u64) -> Result<()> {
        match self {
            Self::File(file) => Ok(file.set_len(len)?),
            Self::Memory(memory) => memory.set_len(len),
        }
    }

    pub fn sync_data(&self) -> Result<()> {
        match self {
            Self::File(file) => Ok(file.sync_data()?),
            Self::Memory(_) => Ok(()),
        }
    }

    pub fn sync_all(&self) -> Result<()> {
        match self {
            Self::File(file) => Ok(file.sync_all()?),
            Self::Memory(_) => Ok(()),
        }
    }

    pub fn load_immutable(&self, offset: u64, size: u64) -> Result<BaseBytes> {
        match self {
            Self::File(file) => Ok(BaseBytes::Mapped(segment::map(file, offset, size)?)),
            Self::Memory(memory) => {
                if size == 0 {
                    return Err(Error::from_io(
                        ErrorKind::InvalidData,
                        "cannot load an empty segment",
                    ));
                }
                let start = usize::try_from(offset)
                    .map_err(|_| Error::invalid_base("segment offset is too large"))?;
                let len = usize::try_from(size)
                    .map_err(|_| Error::invalid_base("segment is too large"))?;
                let end = start
                    .checked_add(len)
                    .ok_or_else(|| Error::invalid_base("segment range overflows memory"))?;
                let source = memory
                    .bytes
                    .get(start..end)
                    .ok_or_else(|| Error::invalid_base("segment extends beyond memory storage"))?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(len).map_err(|_| {
                    Error::from_io(ErrorKind::OutOfMemory, "could not allocate Base snapshot")
                })?;
                bytes.extend_from_slice(source);
                Ok(BaseBytes::Memory(Arc::from(bytes)))
            }
        }
    }

    pub fn is_memory(&self) -> bool {
        matches!(self, Self::Memory(_))
    }
}

impl Read for Storage {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(buffer),
            Self::Memory(memory) => memory.read(buffer),
        }
    }
}

impl Write for Storage {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(bytes),
            Self::Memory(memory) => memory.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Memory(memory) => memory.flush(),
        }
    }
}

impl Seek for Storage {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::File(file) => file.seek(position),
            Self::Memory(memory) => memory.seek(position),
        }
    }
}

#[derive(Default)]
pub struct MemoryStorage {
    bytes: Vec<u8>,
    position: u64,
}

impl MemoryStorage {
    fn set_len(&mut self, len: u64) -> Result<()> {
        let len = usize::try_from(len).map_err(|_| {
            Error::from_io(
                ErrorKind::InvalidInput,
                "memory storage length is too large",
            )
        })?;
        if len > self.bytes.len() {
            self.bytes
                .try_reserve_exact(len - self.bytes.len())
                .map_err(|_| {
                    Error::from_io(ErrorKind::OutOfMemory, "could not grow memory storage")
                })?;
        }
        self.bytes.resize(len, 0);
        Ok(())
    }
}

impl Read for MemoryStorage {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let start = usize::try_from(self.position)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "position is too large"))?;
        let Some(source) = self.bytes.get(start..) else {
            return Ok(0);
        };
        let len = source.len().min(buffer.len());
        buffer[..len].copy_from_slice(&source[..len]);
        self.position += len as u64;
        Ok(len)
    }
}

impl Write for MemoryStorage {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = usize::try_from(self.position)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "position is too large"))?;
        let end = start
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "write range overflow"))?;
        if end > self.bytes.len() {
            self.bytes
                .try_reserve_exact(end - self.bytes.len())
                .map_err(|_| io::Error::new(ErrorKind::OutOfMemory, "memory allocation failed"))?;
            self.bytes.resize(end, 0);
        }
        self.bytes[start..end].copy_from_slice(bytes);
        self.position = end as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for MemoryStorage {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let base = match position {
            SeekFrom::Start(position) => {
                self.position = position;
                return Ok(position);
            }
            SeekFrom::End(offset) => (self.bytes.len() as u64, offset),
            SeekFrom::Current(offset) => (self.position, offset),
        };
        let position = i128::from(base.0) + i128::from(base.1);
        if !(0..=i128::from(u64::MAX)).contains(&position) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid seek position",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}
