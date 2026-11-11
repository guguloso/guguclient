//! Little-endian cursor over a byte slice (`BinaryReader`).

pub type ReadResult<T> = Result<T, String>;

pub struct BinaryReader<'a> {
    buffer: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    pub fn new(buffer: &'a [u8]) -> Self {
        Self { buffer, offset: 0 }
    }

    pub fn position(&self) -> usize {
        self.offset
    }

    pub fn remaining(&self) -> usize {
        self.buffer.len() - self.offset
    }

    pub fn seek(&mut self, offset: usize) -> ReadResult<()> {
        if offset > self.buffer.len() {
            return Err(format!("Seek to {offset} outside buffer of {} bytes", self.buffer.len()));
        }
        self.offset = offset;
        Ok(())
    }

    fn ensure(&self, count: usize) -> ReadResult<()> {
        if self.offset.checked_add(count).is_none_or(|end| end > self.buffer.len()) {
            return Err(format!("Read of {count} bytes at offset {} exceeds buffer of {} bytes", self.offset, self.buffer.len()));
        }
        Ok(())
    }

    pub fn skip(&mut self, count: usize) -> ReadResult<()> {
        self.ensure(count)?;
        self.offset += count;
        Ok(())
    }

    pub fn bytes(&mut self, count: usize) -> ReadResult<&'a [u8]> {
        self.ensure(count)?;
        let slice = &self.buffer[self.offset..self.offset + count];
        self.offset += count;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> ReadResult<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> ReadResult<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn i8(&mut self) -> ReadResult<i8> {
        Ok(self.array::<1>()?[0] as i8)
    }

    pub fn u16(&mut self) -> ReadResult<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> ReadResult<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn f32(&mut self) -> ReadResult<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    pub fn f64(&mut self) -> ReadResult<f64> {
        Ok(f64::from_le_bytes(self.array()?))
    }

    /// Fixed-length UTF-8 string (lossy like `TextDecoder`).
    pub fn string(&mut self, len: usize) -> ReadResult<String> {
        Ok(String::from_utf8_lossy(self.bytes(len)?).into_owned())
    }

    /// NUL-terminated UTF-8 string, consuming the terminator.
    pub fn cstring(&mut self) -> ReadResult<String> {
        let start = self.offset;
        let Some(len) = self.buffer[start..].iter().position(|b| *b == 0) else {
            return Err(format!("Unterminated string starting at offset {start}"));
        };
        let value = String::from_utf8_lossy(&self.buffer[start..start + len]).into_owned();
        self.offset = start + len + 1;
        Ok(value)
    }
}
