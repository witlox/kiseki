//! Minimal Portmapper / `RPCBIND` service (`RFC 1057`, `RFC 1833`).
//!
//! Bug 10 (GCP 2026-05-04): the Linux `NFSv3` client first contacts
//! portmapper on TCP/111 to discover the port for the MOUNT (100005)
//! and NFS (100003) programs. Without a portmapper, `mount -t nfs
//! -o vers=3 server:/path /mnt` fails with "Connection refused"
//! before any NFS/MOUNT RPC is issued — even though kiseki's NFS
//! server already dispatches both programs off port 2049 by program
//! number.
//!
//! This module implements the bare minimum portmapper protocol the
//! Linux kernel client needs: NULL (procedure 0) and GETPORT
//! (procedure 3). Both are mapped to the well-known kiseki NFS port,
//! since both NFS3 and MOUNT3 are dispatched from the same listener.
//!
//! Program: 100000, Version: 2 (TCP). Conventional bind port: 111.

use std::io;
use std::net::TcpListener;
use std::sync::Arc;

use crate::nfs_xdr::{
    encode_reply_accepted, read_rm_message, write_rm_message, RpcCallHeader, XdrReader, XdrWriter,
};

/// Portmapper program number per IANA RPC program registry.
pub const PORTMAP_PROGRAM: u32 = 100_000;
/// Portmapper protocol version 2 (`RFC 1057` §A.1). Version 2 is what
/// the Linux kernel `NFSv3` client speaks.
pub const PORTMAP_VERSION: u32 = 2;

/// Portmapper procedure numbers (`RFC 1057` §A.1).
pub mod proc {
    /// `PMAPPROC_NULL` — health check, void args/reply.
    pub const NULL: u32 = 0;
    /// `PMAPPROC_SET` — register a (prog, vers, prot, port) mapping. Refused.
    pub const SET: u32 = 1;
    /// `PMAPPROC_UNSET` — drop a mapping. Refused.
    pub const UNSET: u32 = 2;
    /// `PMAPPROC_GETPORT` — resolve (prog, vers, prot) to a port.
    pub const GETPORT: u32 = 3;
    /// `PMAPPROC_DUMP` — list all mappings.
    pub const DUMP: u32 = 4;
    /// `PMAPPROC_CALLIT` — proxy an RPC call. Not supported.
    pub const CALLIT: u32 = 5;
}

/// `IPPROTO_TCP` per `RFC 1057` §A.1.
pub const IPPROTO_TCP: u32 = 6;
/// `IPPROTO_UDP` per `RFC 1057` §A.1.
pub const IPPROTO_UDP: u32 = 17;

/// NFS program number per IANA RPC program registry.
pub const NFS_PROGRAM: u32 = 100_003;
/// MOUNT program number per IANA RPC program registry.
pub const MOUNT_PROGRAM: u32 = 100_005;

/// Process a single portmap message. Returns the reply bytes.
///
/// `nfs_port` is the port on which both NFS3 and MOUNT3 are
/// dispatched (kiseki co-locates them on a single TCP listener).
#[must_use]
pub fn handle_portmap_message(header: &RpcCallHeader, raw_msg: &[u8], nfs_port: u16) -> Vec<u8> {
    let mut reader = XdrReader::new(raw_msg);
    let _ = RpcCallHeader::decode(&mut reader);
    dispatch_portmap(header, &mut reader, nfs_port)
}

/// Long-lived portmap connection handler. Linux kernel mount.nfs
/// keeps the TCP socket open across multiple GETPORT calls
/// (typically one for MOUNT3 then one for NFS3), so loop until EOF.
pub fn handle_portmap_connection<S: io::Read + io::Write>(
    mut stream: S,
    nfs_port: u16,
) -> io::Result<()> {
    loop {
        let msg = match read_rm_message(&mut stream) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut reader = XdrReader::new(&msg);
        let header = RpcCallHeader::decode(&mut reader)?;
        if header.program != PORTMAP_PROGRAM || header.version != PORTMAP_VERSION {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 2); // PROG_MISMATCH
            w.write_u32(PORTMAP_VERSION); // low
            w.write_u32(PORTMAP_VERSION); // high
            write_rm_message(&mut stream, &w.into_bytes())?;
            continue;
        }
        let reply = dispatch_portmap(&header, &mut reader, nfs_port);
        write_rm_message(&mut stream, &reply)?;
    }
}

/// Spawn worker threads to serve portmap on the given listener.
/// Mirrors the per-connection-thread shape of the NFS server.
pub fn serve_portmap_listener(listener: &Arc<TcpListener>, nfs_port: u16) {
    while let Ok((stream, addr)) = listener.accept() {
        tracing::debug!(peer = %addr, "portmap: accepted connection");
        std::thread::spawn(move || {
            if let Err(e) = handle_portmap_connection(stream, nfs_port) {
                tracing::debug!(peer = %addr, error = %e, "portmap: connection closed");
            }
        });
    }
}

fn dispatch_portmap(header: &RpcCallHeader, reader: &mut XdrReader<'_>, nfs_port: u16) -> Vec<u8> {
    tracing::debug!(
        xid = header.xid,
        procedure = header.procedure,
        "portmap dispatch"
    );
    match header.procedure {
        proc::NULL => reply_null(header.xid),
        proc::GETPORT => reply_getport(header.xid, reader, nfs_port),
        proc::DUMP => reply_empty_dump(header.xid),
        proc::SET | proc::UNSET => {
            // Refuse to mutate the (static) mapping table. Linux clients
            // never invoke these against a remote portmapper anyway.
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 0); // SUCCESS, body=false
            w.write_bool(false);
            w.into_bytes()
        }
        _ => {
            let mut w = XdrWriter::new();
            encode_reply_accepted(&mut w, header.xid, 3); // PROC_UNAVAIL
            w.into_bytes()
        }
    }
}

fn reply_null(xid: u32) -> Vec<u8> {
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS, void body
    w.into_bytes()
}

/// GETPORT — args: { prog, vers, prot, port } reply: { port }.
fn reply_getport(xid: u32, reader: &mut XdrReader<'_>, nfs_port: u16) -> Vec<u8> {
    let prog = reader.read_u32().unwrap_or(0);
    let vers = reader.read_u32().unwrap_or(0);
    let proto = reader.read_u32().unwrap_or(0);
    let _port_in = reader.read_u32().unwrap_or(0);

    let port = if proto == IPPROTO_TCP
        && (prog == NFS_PROGRAM || prog == MOUNT_PROGRAM)
        && (vers == 3 || vers == 4)
    {
        u32::from(nfs_port)
    } else {
        0
    };

    tracing::debug!(prog, vers, proto, resolved_port = port, "portmap GETPORT");

    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS
    w.write_u32(port);
    w.into_bytes()
}

fn reply_empty_dump(xid: u32) -> Vec<u8> {
    // `PMAPPROC_`DUMP — list-of-mappings encoding: each entry is
    // preceded by a bool marker (true = entry follows). We always
    // emit "no entries" because kiseki doesn't accept dynamic
    // registrations.
    let mut w = XdrWriter::new();
    encode_reply_accepted(&mut w, xid, 0); // SUCCESS
    w.write_bool(false); // end-of-list
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_call(xid: u32, procedure: u32, body: &[u8]) -> Vec<u8> {
        let mut w = XdrWriter::new();
        // RPC call header.
        w.write_u32(xid);
        w.write_u32(0); // CALL
        w.write_u32(2); // RPC version 2
        w.write_u32(PORTMAP_PROGRAM);
        w.write_u32(PORTMAP_VERSION);
        w.write_u32(procedure);
        // AUTH_NONE credential + verifier.
        w.write_u32(0);
        w.write_u32(0);
        w.write_u32(0);
        w.write_u32(0);
        // Body.
        for chunk in body.chunks(4) {
            let mut buf = [0u8; 4];
            buf[..chunk.len()].copy_from_slice(chunk);
            w.write_u32(u32::from_be_bytes(buf));
        }
        w.into_bytes()
    }

    #[test]
    fn null_returns_success() {
        let raw = build_call(0xDEAD, proc::NULL, &[]);
        let mut reader = XdrReader::new(&raw);
        let header = RpcCallHeader::decode(&mut reader).unwrap();
        let reply = handle_portmap_message(&header, &raw, 2049);
        // RPC reply: xid + REPLY(=1) + MSG_ACCEPTED(=0) + verifier(2 u32) + accept_stat(=0 SUCCESS)
        let mut r = XdrReader::new(&reply);
        assert_eq!(r.read_u32().unwrap(), 0xDEAD);
        assert_eq!(r.read_u32().unwrap(), 1); // REPLY
        assert_eq!(r.read_u32().unwrap(), 0); // MSG_ACCEPTED
        let _verf_flavor = r.read_u32().unwrap();
        let _verf_len = r.read_u32().unwrap();
        assert_eq!(r.read_u32().unwrap(), 0); // SUCCESS
    }

    #[test]
    fn getport_for_mount3_tcp_returns_nfs_port() {
        let mut body = XdrWriter::new();
        body.write_u32(MOUNT_PROGRAM);
        body.write_u32(3);
        body.write_u32(IPPROTO_TCP);
        body.write_u32(0);
        let body_bytes = body.into_bytes();

        let raw = build_call(0xBEEF, proc::GETPORT, &body_bytes);
        let mut reader = XdrReader::new(&raw);
        let header = RpcCallHeader::decode(&mut reader).unwrap();
        let reply = handle_portmap_message(&header, &raw, 2049);

        let mut r = XdrReader::new(&reply);
        let _xid = r.read_u32().unwrap();
        let _msg_type = r.read_u32().unwrap();
        let _accept = r.read_u32().unwrap();
        let _verf_flavor = r.read_u32().unwrap();
        let _verf_len = r.read_u32().unwrap();
        let _stat = r.read_u32().unwrap();
        let port = r.read_u32().unwrap();
        assert_eq!(port, 2049, "MOUNT3/TCP must resolve to the NFS port");
    }

    #[test]
    fn getport_for_nfs3_tcp_returns_nfs_port() {
        let mut body = XdrWriter::new();
        body.write_u32(NFS_PROGRAM);
        body.write_u32(3);
        body.write_u32(IPPROTO_TCP);
        body.write_u32(0);
        let body_bytes = body.into_bytes();

        let raw = build_call(0xCAFE, proc::GETPORT, &body_bytes);
        let mut reader = XdrReader::new(&raw);
        let header = RpcCallHeader::decode(&mut reader).unwrap();
        let reply = handle_portmap_message(&header, &raw, 2049);

        let mut r = XdrReader::new(&reply);
        for _ in 0..6 {
            let _ = r.read_u32().unwrap();
        }
        let port = r.read_u32().unwrap();
        assert_eq!(port, 2049);
    }

    /// End-to-end wire test: bind the listener on a random
    /// non-privileged port, connect a TCP client, send GETPORT for
    /// MOUNT3/TCP, and verify the response carries the configured
    /// NFS port. Proves the listener wiring (not just the in-process
    /// dispatch) so the Bug 10 fix is testable without root.
    #[test]
    fn tcp_round_trip_getport_returns_nfs_port() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bound = listener.local_addr().unwrap();
        let listener = std::sync::Arc::new(listener);
        let listener_for_thread = std::sync::Arc::clone(&listener);
        std::thread::spawn(move || {
            serve_portmap_listener(&listener_for_thread, 2049);
        });

        let mut stream = TcpStream::connect(bound).unwrap();

        // Build a GETPORT(MOUNT3, vers=3, TCP) call and frame it with
        // the ONC RPC record-marking header (length | 0x80000000).
        let mut body = XdrWriter::new();
        body.write_u32(MOUNT_PROGRAM);
        body.write_u32(3);
        body.write_u32(IPPROTO_TCP);
        body.write_u32(0);
        let body_bytes = body.into_bytes();
        let raw_call = build_call(0xABCD, proc::GETPORT, &body_bytes);
        let len_marker = (u32::try_from(raw_call.len()).unwrap()) | 0x8000_0000;
        stream.write_all(&len_marker.to_be_bytes()).unwrap();
        stream.write_all(&raw_call).unwrap();
        stream.flush().unwrap();

        // Read back the framed reply.
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).unwrap();
        let reply_len = (u32::from_be_bytes(header) & 0x7FFF_FFFF) as usize;
        let mut reply = vec![0u8; reply_len];
        stream.read_exact(&mut reply).unwrap();

        let mut r = XdrReader::new(&reply);
        for _ in 0..6 {
            let _ = r.read_u32().unwrap();
        }
        let port = r.read_u32().unwrap();
        assert_eq!(
            port, 2049,
            "TCP-bound portmapper must resolve MOUNT3/TCP → NFS port",
        );
    }

    #[test]
    fn getport_for_unknown_program_returns_zero() {
        let mut body = XdrWriter::new();
        body.write_u32(999_999); // unknown program
        body.write_u32(3);
        body.write_u32(IPPROTO_TCP);
        body.write_u32(0);
        let body_bytes = body.into_bytes();

        let raw = build_call(0x1234, proc::GETPORT, &body_bytes);
        let mut reader = XdrReader::new(&raw);
        let header = RpcCallHeader::decode(&mut reader).unwrap();
        let reply = handle_portmap_message(&header, &raw, 2049);

        let mut r = XdrReader::new(&reply);
        for _ in 0..6 {
            let _ = r.read_u32().unwrap();
        }
        assert_eq!(
            r.read_u32().unwrap(),
            0,
            "unmapped programs must return port 0"
        );
    }
}
