//! XDR (External Data Representation) codec for NFS wire protocol.
//!
//! Shared by NFSv3 and NFSv4.2. Implements RFC 4506 encoding for
//! the subset of types needed by the NFS procedures.
//!
//! XDR is big-endian, 4-byte aligned. Strings and opaque data are
//! length-prefixed and padded to 4-byte boundaries.

use std::io::{self, Read, Write};

/// Maximum NFS frame size (16 MB) to prevent OOM from malicious headers.
const MAX_NFS_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// XDR encoder — writes big-endian, 4-byte-aligned data.
pub struct XdrWriter {
    buf: Vec<u8>,
}

impl XdrWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1024),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn write_bool(&mut self, v: bool) {
        self.write_u32(u32::from(v));
    }

    /// Write opaque data with length prefix, padded to 4-byte boundary.
    pub fn write_opaque(&mut self, data: &[u8]) {
        self.write_u32(data.len() as u32);
        self.buf.extend_from_slice(data);
        let pad = (4 - (data.len() % 4)) % 4;
        for _ in 0..pad {
            self.buf.push(0);
        }
    }

    /// Write a fixed-length opaque (no length prefix, still padded).
    pub fn write_opaque_fixed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        let pad = (4 - (data.len() % 4)) % 4;
        for _ in 0..pad {
            self.buf.push(0);
        }
    }

    /// Write a string (same as opaque in XDR).
    pub fn write_string(&mut self, s: &str) {
        self.write_opaque(s.as_bytes());
    }
}

/// XDR decoder — reads big-endian, 4-byte-aligned data.
pub struct XdrReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> XdrReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn read_u32(&mut self) -> io::Result<u32> {
        if self.remaining() < 4 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "xdr: u32"));
        }
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_u64(&mut self) -> io::Result<u64> {
        let hi = self.read_u32()? as u64;
        let lo = self.read_u32()? as u64;
        Ok((hi << 32) | lo)
    }

    pub fn read_i32(&mut self) -> io::Result<i32> {
        self.read_u32().map(|v| v as i32)
    }

    pub fn read_bool(&mut self) -> io::Result<bool> {
        self.read_u32().map(|v| v != 0)
    }

    /// Read variable-length opaque data.
    pub fn read_opaque(&mut self) -> io::Result<Vec<u8>> {
        let len = self.read_u32()? as usize;
        if self.remaining() < len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "xdr: opaque"));
        }
        let data = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        let pad = (4 - (len % 4)) % 4;
        self.pos += pad;
        Ok(data)
    }

    /// Read fixed-length opaque data.
    pub fn read_opaque_fixed(&mut self, len: usize) -> io::Result<Vec<u8>> {
        if self.remaining() < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "xdr: fixed opaque",
            ));
        }
        let data = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        let pad = (4 - (len % 4)) % 4;
        self.pos += pad;
        Ok(data)
    }

    /// Read a string.
    pub fn read_string(&mut self) -> io::Result<String> {
        let bytes = self.read_opaque()?;
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// ONC RPC message header (RFC 5531).
#[derive(Debug, Clone)]
pub struct RpcCallHeader {
    pub xid: u32,
    pub program: u32,
    pub version: u32,
    pub procedure: u32,
}

/// ONC RPC reply status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcReplyStatus {
    Accepted,
    Denied,
}

impl RpcCallHeader {
    /// Decode an ONC RPC call header from XDR.
    pub fn decode(r: &mut XdrReader<'_>) -> io::Result<Self> {
        let xid = r.read_u32()?;
        let msg_type = r.read_u32()?;
        if msg_type != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected RPC CALL (0)",
            ));
        }
        let rpc_version = r.read_u32()?;
        if rpc_version != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported RPC version {rpc_version}"),
            ));
        }
        let program = r.read_u32()?;
        let version = r.read_u32()?;
        let procedure = r.read_u32()?;

        // Skip auth (credential + verifier).
        let _cred_flavor = r.read_u32()?;
        let _cred_body = r.read_opaque()?;
        let _verf_flavor = r.read_u32()?;
        let _verf_body = r.read_opaque()?;

        Ok(Self {
            xid,
            program,
            version,
            procedure,
        })
    }
}

/// Encode an ONC RPC accepted reply header.
pub fn encode_reply_accepted(w: &mut XdrWriter, xid: u32, accept_stat: u32) {
    w.write_u32(xid); // XID
    w.write_u32(1); // REPLY
    w.write_u32(0); // MSG_ACCEPTED
                    // NULL verifier
    w.write_u32(0); // AUTH_NONE
    w.write_u32(0); // verifier body length
    w.write_u32(accept_stat); // SUCCESS=0, PROG_UNAVAIL=1, etc.
}

/// Read a Record Marking (RM) framed message from a TCP stream.
pub fn read_rm_message<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        let mut hdr = [0u8; 4];
        reader.read_exact(&mut hdr)?;
        let fragment_header = u32::from_be_bytes(hdr);
        let last = (fragment_header & 0x8000_0000) != 0;
        let len = (fragment_header & 0x7FFF_FFFF) as usize;

        // Cap frame size to prevent OOM from malicious headers (C-ADV-1).
        if buf.len() + len > MAX_NFS_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NFS frame exceeds 16MB limit",
            ));
        }

        let start = buf.len();
        buf.resize(start + len, 0);
        reader.read_exact(&mut buf[start..])?;

        if last {
            break;
        }
    }
    Ok(buf)
}

/// Write a Record Marking framed message to a TCP stream.
pub fn write_rm_message<W: Write>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    let header = 0x8000_0000 | (data.len() as u32);
    writer.write_all(&header.to_be_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdr_u32_roundtrip() {
        let mut w = XdrWriter::new();
        w.write_u32(42);
        w.write_u32(0xDEAD_BEEF);

        let bytes = w.into_bytes();
        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_u32().unwrap(), 42);
        assert_eq!(r.read_u32().unwrap(), 0xDEAD_BEEF);
    }

    #[test]
    fn xdr_string_roundtrip() {
        let mut w = XdrWriter::new();
        w.write_string("hello");

        let bytes = w.into_bytes();
        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_string().unwrap(), "hello");
    }

    #[test]
    fn xdr_opaque_padded() {
        let mut w = XdrWriter::new();
        w.write_opaque(&[1, 2, 3]); // 3 bytes → padded to 4

        let bytes = w.into_bytes();
        assert_eq!(bytes.len(), 4 + 4); // 4 length + 3 data + 1 pad

        let mut r = XdrReader::new(&bytes);
        assert_eq!(r.read_opaque().unwrap(), vec![1, 2, 3]);
    }
}
