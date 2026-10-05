//! Read-only tar archive cursor: the ramdisk's file format.
//!
//! The runner packs user programs into an uncompressed USTAR archive
//! (512-byte headers, no PathnameLong extensions for short names); this
//! cursor walks it. Allocation-free, `no_std`, host-testable.

/// Entries come in whole blocks.
const BLOCK: usize = 512;

/// Walks a tar archive's regular files. Unknown entry types (extensions,
/// directories) are skipped; two zero blocks end the archive.
#[derive(Debug)]
pub struct TarCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> TarCursor<'a> {
    /// Starts at the archive's beginning.
    pub fn new(data: &'a [u8]) -> Self {
        TarCursor { data, pos: 0 }
    }

    /// The next regular file as `(name, contents)`, or `None` at the end.
    pub fn next_file(&mut self) -> Option<(&'a str, &'a [u8])> {
        while self.remaining() >= BLOCK {
            let header = &self.data[self.pos..self.pos + BLOCK];
            // End of archive: name block of zeros (checked via the name's
            // first byte; a size-0 empty name at this point is terminal).
            if header[0] == 0 {
                return None;
            }
            let name = read_name(header)?;
            let size = read_size(header)?;
            let typeflag = header[156];
            let entry_end = self.pos + BLOCK + size;
            let (produced, advance): (Option<(&'a str, &'a [u8])>, usize) = match typeflag {
                b'0' | 0 => {
                    let body = self
                        .data
                        .get(self.pos + BLOCK..entry_end.min(self.data.len()))?;
                    (
                        Some((name, body)),
                        BLOCK
                            + usize::from((size % BLOCK != 0) as u8) * (BLOCK - size % BLOCK)
                            + size,
                    )
                }
                // Extensions (pax 'x'/'g'), directories '5', long names
                // 'L': skip header + body wholesale.
                _ => (None, BLOCK + ceil_to_block(size)),
            };
            self.pos += advance;
            if let Some(entry) = produced {
                return Some(entry);
            }
        }
        None
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
}

/// Parses a NUL-terminated name from the header's first 100 bytes.
fn read_name(header: &[u8]) -> Option<&str> {
    let end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
    core::str::from_utf8(&header[..end]).ok()
}

/// Parses a 12-byte octal size field (digits then NULs/spaces).
fn read_size(header: &[u8]) -> Option<usize> {
    let field = &header[124..136];
    let mut size = 0usize;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                size = size * 8 + (b - b'0') as usize;
            }
            0 | b' ' => break,
            _ => return None,
        }
    }
    Some(size)
}

/// Round up to the 512-byte block boundary.
fn ceil_to_block(size: usize) -> usize {
    size.div_ceil(BLOCK) * BLOCK
}
