//! ONC RPC over TCP transport — shared by `NFSv3` and `NFSv4` clients.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use kiseki_gateway::error::GatewayError;

/// Shared TCP connection with ONC RPC framing.
pub struct RpcTransport {
    pub(crate) stream: TcpStream,
    pub(crate) xid: u32,
}

impl RpcTransport {
    /// Connect to an NFS server.
    pub fn connect(addr: SocketAddr) -> Result<Self, GatewayError> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .map_err(|e| GatewayError::ProtocolError(format!("NFS connect to {addr}: {e}")))?;
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
        // Disable Nagle. NFS RPC is strict request/reply with sub-MSS
        // bodies; the kernel's 40 ms delayed-ACK + Nagle on either
        // peer adds 40 ms to every round-trip. The server already
        // sets TCP_NODELAY on its accept side; both sides need it.
        // Measured impact on NFSv4 64 KiB GET: 41 ms/op → ~5 ms/op.
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            xid: 0x4B49_5345, // "KISE"
        })
    }

    /// Send an ONC RPC CALL and return the reply body (after the
    /// 24-byte RPC accept header). RFC 5531 §8-9.
    pub fn call(
        &mut self,
        program: u32,
        version: u32,
        procedure: u32,
        body: &[u8],
    ) -> Result<Vec<u8>, GatewayError> {
        self.xid = self.xid.wrapping_add(1);

        // RPC call header
        let mut rpc = Vec::with_capacity(40 + body.len());
        rpc.extend_from_slice(&self.xid.to_be_bytes());
        rpc.extend_from_slice(&0u32.to_be_bytes()); // CALL
        rpc.extend_from_slice(&2u32.to_be_bytes()); // rpc_vers
        rpc.extend_from_slice(&program.to_be_bytes());
        rpc.extend_from_slice(&version.to_be_bytes());
        rpc.extend_from_slice(&procedure.to_be_bytes());
        // AUTH_NONE credentials + verifier
        for _ in 0..4 {
            rpc.extend_from_slice(&0u32.to_be_bytes());
        }
        rpc.extend_from_slice(body);

        // Record marker: last-fragment flag + length
        let marker = 0x8000_0000 | (rpc.len() as u32);
        self.stream
            .write_all(&marker.to_be_bytes())
            .map_err(|e| GatewayError::ProtocolError(format!("write marker: {e}")))?;
        self.stream
            .write_all(&rpc)
            .map_err(|e| GatewayError::ProtocolError(format!("write rpc: {e}")))?;
        self.stream
            .flush()
            .map_err(|e| GatewayError::ProtocolError(format!("flush: {e}")))?;

        // Read reply record marker
        let mut hdr = [0u8; 4];
        self.stream
            .read_exact(&mut hdr)
            .map_err(|e| GatewayError::ProtocolError(format!("read marker: {e}")))?;
        let reply_len = (u32::from_be_bytes(hdr) & 0x7FFF_FFFF) as usize;

        // Read reply body
        let mut reply = vec![0u8; reply_len];
        self.stream
            .read_exact(&mut reply)
            .map_err(|e| GatewayError::ProtocolError(format!("read body: {e}")))?;

        // Verify RPC accept header (24 bytes): xid + msg_type + reply_stat + verifier + accept_stat
        if reply.len() < 24 {
            return Err(GatewayError::ProtocolError(format!(
                "reply too short: {}",
                reply.len()
            )));
        }
        let accept = u32::from_be_bytes(reply[20..24].try_into().unwrap());
        if accept != 0 {
            return Err(GatewayError::ProtocolError(format!(
                "RPC rejected: accept_stat={accept}"
            )));
        }

        Ok(reply[24..].to_vec())
    }
}
