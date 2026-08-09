use super::state::BaseBytes;
use crate::error::{Error, Result};
use crate::format::{segment, superblock::DATA_START};
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
            Self::Memory(memory) => Ok(memory.len),
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

    pub fn load_immutable(&mut self, offset: u64, size: u64) -> Result<BaseBytes> {
        match self {
            Self::File(file) => Ok(BaseBytes::Mapped(segment::map(file, offset, size)?)),
            Self::Memory(memory) => memory.load_immutable(offset, size),
        }
    }

    pub fn replace_memory_base(&mut self, offset: u64, bytes: Arc<Vec<u8>>) -> Result<()> {
        match self {
            Self::Memory(memory) => memory.replace_base(offset, bytes),
            Self::File(_) => Err(Error::from_io(
                ErrorKind::InvalidInput,
                "cannot install an in-memory Base into file storage",
            )),
        }
    }

    pub fn extend_memory_log(&mut self, additional: u64) -> Result<()> {
        match self {
            Self::Memory(memory) => memory.extend_log(additional),
            Self::File(_) => Err(Error::from_io(
                ErrorKind::InvalidInput,
                "cannot extend an in-memory log in file storage",
            )),
        }
    }

    #[cfg(test)]
    pub fn memory_base_mapping(&self) -> Option<&Arc<Vec<u8>>> {
        match self {
            Self::Memory(memory) => memory.base.as_ref().map(|base| &base.bytes),
            Self::File(_) => None,
        }
    }

    #[cfg(test)]
    pub fn memory_materialized_bytes(&self) -> Option<usize> {
        match self {
            Self::Memory(memory) => Some(
                memory.header.len()
                    + memory.base.as_ref().map_or(0, |base| base.bytes.len())
                    + memory.tail.len(),
            ),
            Self::File(_) => None,
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

pub struct MemoryStorage {
    // The active Base is shared with readers. Obsolete generations and the
    // non-durable Overlay log consume logical offsets but no backing bytes.
    header: Vec<u8>,
    base: Option<MemoryRegion>,
    tail_start: u64,
    tail: Vec<u8>,
    len: u64,
    position: u64,
}

struct MemoryRegion {
    offset: u64,
    bytes: Arc<Vec<u8>>,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self {
            header: Vec::new(),
            base: None,
            tail_start: DATA_START,
            tail: Vec::new(),
            len: 0,
            position: 0,
        }
    }
}

impl MemoryStorage {
    fn set_len(&mut self, len: u64) -> Result<()> {
        if self.base.is_some() {
            if len < self.tail_start {
                return Err(Error::from_io(
                    ErrorKind::InvalidInput,
                    "cannot truncate the active in-memory Base",
                ));
            }
            let tail_len = usize::try_from(len - self.tail_start).map_err(|_| {
                Error::from_io(
                    ErrorKind::InvalidInput,
                    "memory storage length is too large",
                )
            })?;
            resize_bytes(&mut self.tail, tail_len)?;
            self.len = len;
            return Ok(());
        }

        if len <= DATA_START {
            resize_bytes(&mut self.header, len as usize)?;
            self.tail.clear();
            self.tail_start = DATA_START;
        } else {
            resize_bytes(&mut self.header, DATA_START as usize)?;
            self.tail_start = DATA_START;
            let tail_len = usize::try_from(len - DATA_START).map_err(|_| {
                Error::from_io(
                    ErrorKind::InvalidInput,
                    "memory storage length is too large",
                )
            })?;
            resize_bytes(&mut self.tail, tail_len)?;
        }
        self.len = len;
        Ok(())
    }

    fn load_immutable(&mut self, offset: u64, size: u64) -> Result<BaseBytes> {
        if size == 0 {
            return Err(Error::from_io(
                ErrorKind::InvalidData,
                "cannot load an empty segment",
            ));
        }
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= self.len)
            .ok_or_else(|| Error::invalid_base("segment extends beyond memory storage"))?;

        if let Some(base) = &self.base {
            let base_end = base.offset + base.bytes.len() as u64;
            if offset >= base.offset && end <= base_end {
                return Ok(BaseBytes::Memory {
                    bytes: Arc::clone(&base.bytes),
                    range: (offset - base.offset) as usize..(end - base.offset) as usize,
                });
            }
        }

        if self.base.is_none() && offset == self.tail_start && end == self.len {
            let bytes = Arc::new(std::mem::take(&mut self.tail));
            self.base = Some(MemoryRegion {
                offset,
                bytes: Arc::clone(&bytes),
            });
            self.tail_start = end;
            return Ok(BaseBytes::Memory {
                range: 0..bytes.len(),
                bytes,
            });
        }

        let len = usize::try_from(size).map_err(|_| Error::invalid_base("segment is too large"))?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).map_err(|_| {
            Error::from_io(ErrorKind::OutOfMemory, "could not allocate immutable bytes")
        })?;
        bytes.resize(len, 0);
        let previous = self.position;
        self.position = offset;
        let result = self.read_exact(&mut bytes);
        self.position = previous;
        result?;
        let bytes = Arc::new(bytes);
        Ok(BaseBytes::Memory {
            range: 0..bytes.len(),
            bytes,
        })
    }

    fn replace_base(&mut self, offset: u64, bytes: Arc<Vec<u8>>) -> Result<()> {
        if offset < DATA_START || bytes.is_empty() {
            return Err(Error::from_io(
                ErrorKind::InvalidInput,
                "invalid in-memory Base range",
            ));
        }
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::invalid_base("in-memory Base range overflows"))?;
        resize_bytes(&mut self.header, DATA_START as usize)?;
        self.base = Some(MemoryRegion { offset, bytes });
        self.tail_start = end;
        self.tail.clear();
        self.len = end;
        self.position = end;
        Ok(())
    }

    fn extend_log(&mut self, additional: u64) -> Result<()> {
        self.len = self
            .len
            .checked_add(additional)
            .ok_or_else(|| Error::from_io(ErrorKind::InvalidInput, "memory log size overflow"))?;
        self.position = self.len;
        Ok(())
    }
}

fn resize_bytes(bytes: &mut Vec<u8>, len: usize) -> Result<()> {
    if len > bytes.len() {
        bytes
            .try_reserve_exact(len - bytes.len())
            .map_err(|_| Error::from_io(ErrorKind::OutOfMemory, "could not grow memory storage"))?;
    }
    bytes.resize(len, 0);
    Ok(())
}

impl Read for MemoryStorage {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position >= self.len {
            return Ok(0);
        }
        let available = ((self.len - self.position).min(buffer.len() as u64)) as usize;
        let mut written = 0usize;
        while written < available {
            let position = self.position;
            if position < self.header.len() as u64 {
                let start = position as usize;
                let len = (self.header.len() - start).min(available - written);
                buffer[written..written + len].copy_from_slice(&self.header[start..start + len]);
                self.position += len as u64;
                written += len;
                continue;
            }
            if let Some(base) = &self.base {
                let base_end = base.offset + base.bytes.len() as u64;
                if position >= base.offset && position < base_end {
                    let start = (position - base.offset) as usize;
                    let len = (base.bytes.len() - start).min(available - written);
                    buffer[written..written + len].copy_from_slice(&base.bytes[start..start + len]);
                    self.position += len as u64;
                    written += len;
                    continue;
                }
            }
            let tail_end = self.tail_start + self.tail.len() as u64;
            if position >= self.tail_start && position < tail_end {
                let start = usize::try_from(position - self.tail_start).map_err(|_| {
                    io::Error::new(ErrorKind::InvalidInput, "read position is too large")
                })?;
                let len = (self.tail.len() - start).min(available - written);
                buffer[written..written + len].copy_from_slice(&self.tail[start..start + len]);
                self.position += len as u64;
                written += len;
                continue;
            }

            let mut next = self.len;
            if let Some(base) = &self.base
                && base.offset > position
            {
                next = next.min(base.offset);
            }
            if self.tail_start > position {
                next = next.min(self.tail_start);
            }
            let len = ((next - position).min((available - written) as u64)) as usize;
            buffer[written..written + len].fill(0);
            self.position += len as u64;
            written += len;
        }
        Ok(written)
    }
}

impl Write for MemoryStorage {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let start = self.position;
        let end = start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "write range overflow"))?;
        if end <= DATA_START {
            let start = start as usize;
            let end = end as usize;
            resize_bytes(&mut self.header, end)
                .map_err(|error| io::Error::new(error.kind(), error))?;
            self.header[start..end].copy_from_slice(bytes);
        } else if start >= self.tail_start {
            let start = usize::try_from(start - self.tail_start).map_err(|_| {
                io::Error::new(ErrorKind::InvalidInput, "write offset is too large")
            })?;
            let end = start
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "write range overflow"))?;
            resize_bytes(&mut self.tail, end)
                .map_err(|error| io::Error::new(error.kind(), error))?;
            self.tail[start..end].copy_from_slice(bytes);
        } else {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "cannot overwrite an immutable in-memory Base",
            ));
        }
        self.position = end;
        self.len = self.len.max(end);
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
            SeekFrom::End(offset) => (self.len, offset),
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
