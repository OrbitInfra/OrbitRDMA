//! UD queue, address handle and message ownership. Receives reserve 40 bytes for the GRH.
use super::*;
use crate::{Device, Link, RegisteredRange};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatagramAddress {
    pub qpn: u32,
    pub qkey: u32,
    pub lid: u16,
    pub gid: [u8; 16],
    pub link: Link,
    pub mtu: u8,
}
pub struct DatagramQueue {
    qp: QueuePair,
    address: DatagramAddress,
}
impl Device {
    pub fn datagram_queue(&self, config: QueueConfig, qkey: u32) -> Result<DatagramQueue> {
        if qkey == 0 || qkey > 0x7fff_ffff {
            return Err(Error::Invalid("UD qkey"));
        }
        let qp = self.create_qp(config, 1, false, None, Some(qkey), None)?;
        let p = qp.parameters();
        Ok(DatagramQueue {
            qp,
            address: DatagramAddress {
                qpn: p.qpn,
                qkey,
                lid: p.lid,
                gid: p.gid,
                link: p.link,
                mtu: p.mtu,
            },
        })
    }
}
impl DatagramQueue {
    pub fn address(&self) -> DatagramAddress {
        self.address
    }
    /// Set the destination of future sends. The current AH remains live through all outstanding work.
    pub fn set_destination(&mut self, peer: DatagramAddress) -> Result<()> {
        if !(2..=0xff_ffff).contains(&peer.qpn)
            || !(1..=0x7fff_ffff).contains(&peer.qkey)
            || !(1..=5).contains(&peer.mtu)
            || peer.link != self.address.link
            || (peer.link == Link::InfiniBand && peer.lid == 0)
            || (peer.link != Link::InfiniBand && (peer.gid == [0; 16] || peer.gid[0] == 0xff))
        {
            return Err(Error::Invalid("UD destination"));
        }
        let d = self.qp.owner.get_mut()?;
        if d.payload.fenced {
            return Err(Error::Closed);
        }
        if d.payload.pending.len() != d.payload.receive_count {
            return Err(Error::Busy);
        }
        self.qp
            .owner
            .cleanup_step(|d| {
                if let Some(route) = &mut d.route {
                    if let Err(fault) = route.close() {
                        d.payload.quarantine(fault);
                        return Err(fault);
                    }
                    Ok(())
                } else {
                    Ok(())
                }
            })
            .map_err(Fault::unquiesced)?;
        let d = self.qp.owner.get_mut()?;
        d.route = None;
        let (api, _, pd) = d.domain.native()?;
        d.route = Some(api.ud_route(pd, d.domain.port, peer)?);
        Ok(())
    }
    /// SEND success proves local completion; UD supplies no peer delivery acknowledgement.
    pub fn send(&mut self, buffer: &RegisteredRange) -> Result<SingleSubmission> {
        self.post(buffer, false)
    }
    /// Supply space for a 40-byte GRH reservation followed by the payload; the CQE counts both.
    pub fn receive(&mut self, buffer: &RegisteredRange) -> Result<SingleSubmission> {
        self.post(buffer, true)
    }
    fn post(&mut self, buffer: &RegisteredRange, receive: bool) -> Result<SingleSubmission> {
        let local = self.qp.parameters();
        let d = self.qp.owner.get_mut()?;
        let length = buffer.bounds().len();
        if receive && length < 40 {
            return Err(Error::Invalid("UD receive GRH reservation"));
        }
        if !receive
            && length
                > d.route
                    .as_ref()
                    .ok_or(Error::Invalid("missing UD destination"))?
                    .mtu
        {
            return Err(Error::Invalid("UD payload exceeds MTU"));
        }
        d.reserve_pending(1, receive)?;
        let qp = d.qp.ok_or(Error::Closed)?;
        let id = reserve_ids(1)?;
        let r = Request::new(
            buffer.clone(),
            if receive {
                Transfer::Receive
            } else {
                Transfer::Send
            },
        );
        let (pending, mut wr) = d.prepare(
            &r,
            &local,
            &local,
            id,
            Signaling {
                group: id,
                signaled: true,
                fence: false,
            },
        )?;
        if !receive {
            wr.datagram(d.route.as_ref().ok_or(Error::Closed)?)?;
        }
        let receipt = d.api.post_payload(qp, std::slice::from_mut(&mut wr))?;
        let status = post_status(receipt, receive)?;
        Ok(SingleSubmission {
            status,
            operation: d.accept(pending, r.buffer, status),
        })
    }
    pub fn progress(&mut self) -> Result<usize> {
        self.qp.progress()
    }
    pub fn wait_for_progress(&mut self, deadline: Instant) -> Result<usize> {
        self.qp.wait_for_progress(deadline)
    }
    pub fn close(&mut self) -> Result<()> {
        self.qp.close()
    }
}
