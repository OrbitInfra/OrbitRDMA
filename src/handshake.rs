//! The fixed control records used by the packaged two-peer example.
#![forbid(unsafe_code)]

use crate::{ConnectionParams, Error, Link, QueuePair, RemoteAccess, RemoteDescriptor};
use std::io::{Read, Write};

fn field<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], Error> {
    let mut field = [0; N];
    bytes.read_exact(&mut field).map_err(Error::ControlIo)?;
    Ok(field)
}

impl ConnectionParams {
    fn encode(self) -> [u8; 30] {
        let mut bytes = [0; 30];
        bytes[..4].copy_from_slice(&self.qpn.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.psn.to_be_bytes());
        bytes[8..10].copy_from_slice(&self.lid.to_be_bytes());
        bytes[10..26].copy_from_slice(&self.gid);
        let link = match self.link {
            Link::InfiniBand => 1,
            Link::RoceV1 => 2,
            Link::RoceV2 => 3,
        };
        bytes[26..].copy_from_slice(&[link, self.mtu, self.initiator_depth, self.responder_depth]);
        bytes
    }

    fn decode(bytes: &[u8; 30]) -> Result<Self, Error> {
        let mut input = bytes.as_slice();
        let qpn = u32::from_be_bytes(field(&mut input)?);
        let psn = u32::from_be_bytes(field(&mut input)?);
        let lid = u16::from_be_bytes(field(&mut input)?);
        let gid = field(&mut input)?;
        let [link, mtu, initiator_depth, responder_depth] = field(&mut input)?;
        let link = match link {
            1 => Link::InfiniBand,
            2 => Link::RoceV1,
            3 => Link::RoceV2,
            _ => return Err(Error::Invalid("unknown RDMA link type")),
        };
        Ok(Self {
            qpn,
            psn,
            lid,
            gid,
            link,
            mtu,
            initiator_depth,
            responder_depth,
        })
    }
}

impl QueuePair {
    /// Exchange one 30-byte connection record in each direction, then connect this RC QP.
    /// Both peers call this method on their own full-duplex control stream. The caller
    /// establishes peer identity and sets I/O deadlines. Each end must be able to
    /// buffer one 30-byte record (as TCP does). After interrupted stream I/O,
    /// start a new exchange on a fresh stream. Custom protocols can use `connect` directly.
    pub fn connect_over(&mut self, stream: &mut (impl Read + Write)) -> Result<(), Error> {
        self.unconnected()?;
        stream
            .write_all(&self.parameters().encode())
            .map_err(Error::ControlIo)?;
        stream.flush().map_err(Error::ControlIo)?;
        let mut peer = [0; 30];
        stream.read_exact(&mut peer).map_err(Error::ControlIo)?;
        self.connect(&ConnectionParams::decode(&peer)?)
    }

    /// Exchange the same records on a Tokio stream. A cancelled exchange may leave
    /// partial bytes on the stream. Start another handshake on a fresh stream.
    /// As with [Self::connect_over], each end must buffer one 30-byte record.
    #[cfg(feature = "tokio")]
    pub async fn connect_over_async(
        &mut self,
        stream: &mut (impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin),
    ) -> Result<(), Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.unconnected()?;
        stream
            .write_all(&self.parameters().encode())
            .await
            .map_err(Error::ControlIo)?;
        stream.flush().await.map_err(Error::ControlIo)?;
        let mut peer = [0; 30];
        stream
            .read_exact(&mut peer)
            .await
            .map_err(Error::ControlIo)?;
        self.connect(&ConnectionParams::decode(&peer)?)
    }
}

impl RemoteDescriptor {
    /// Encode an already published window capability for the application's control stream.
    pub fn to_bytes(self) -> [u8; 25] {
        let mut bytes = [0; 25];
        bytes[..4].copy_from_slice(&self.qpn.to_be_bytes());
        bytes[4..12].copy_from_slice(&self.address.to_be_bytes());
        bytes[12..20].copy_from_slice(&self.length.to_be_bytes());
        bytes[20..24].copy_from_slice(&self.rkey.to_be_bytes());
        bytes[24] = match self.access {
            RemoteAccess::Read => 1,
            RemoteAccess::Write => 2,
        };
        bytes
    }

    /// Decode a peer's 25-byte window record. Submission also checks the connected peer,
    /// requested direction and subrange. The owner coordinates the lifetime of the grant.
    pub fn from_bytes(bytes: [u8; 25]) -> Result<Self, Error> {
        let mut input = bytes.as_slice();
        let qpn = u32::from_be_bytes(field(&mut input)?);
        let address = u64::from_be_bytes(field(&mut input)?);
        let length = u64::from_be_bytes(field(&mut input)?);
        let rkey = u32::from_be_bytes(field(&mut input)?);
        let access = match field(&mut input)? {
            [1] => RemoteAccess::Read,
            [2] => RemoteAccess::Write,
            _ => return Err(Error::Invalid("unknown remote window access")),
        };
        if !(2..=0xff_ffff).contains(&qpn) || rkey == 0 {
            return Err(Error::Invalid("remote window or peer QP"));
        }
        let descriptor = Self {
            qpn,
            address,
            length,
            rkey,
            access,
        };
        descriptor.checked_address(0, length, access)?;
        Ok(descriptor)
    }
}

#[cfg(test)]
#[path = "../tests/unit/handshake.rs"]
mod tests;
