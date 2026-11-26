use orbit_rdma::{ConnectionParams, Link, RemoteAccess, RemoteDescriptor};
use std::io::{self, Read, Write};

pub(super) fn field<const N: usize>(stream: &mut impl Read) -> io::Result<[u8; N]> {
    let mut value = [0; N];
    stream.read_exact(&mut value)?;
    Ok(value)
}

pub(super) fn send_params(w: &mut impl Write, p: ConnectionParams) -> io::Result<()> {
    w.write_all(&p.qpn.to_be_bytes())?;
    w.write_all(&p.psn.to_be_bytes())?;
    w.write_all(&p.lid.to_be_bytes())?;
    w.write_all(&p.gid)?;
    let link = match p.link {
        Link::InfiniBand => 1,
        Link::RoceV1 => 2,
        Link::RoceV2 => 3,
    };
    w.write_all(&[link, p.mtu, p.initiator_depth, p.responder_depth])
}

pub(super) fn read_params(r: &mut impl Read) -> io::Result<ConnectionParams> {
    let qpn = u32::from_be_bytes(field(r)?);
    let psn = u32::from_be_bytes(field(r)?);
    let lid = u16::from_be_bytes(field(r)?);
    let gid = field(r)?;
    let [link, mtu, initiator_depth, responder_depth] = field(r)?;
    let link = match link {
        1 => Link::InfiniBand,
        2 => Link::RoceV1,
        3 => Link::RoceV2,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown link type",
            ));
        }
    };
    Ok(ConnectionParams {
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

pub(super) fn send_window(w: &mut impl Write, d: RemoteDescriptor) -> io::Result<()> {
    w.write_all(&d.qpn.to_be_bytes())?;
    w.write_all(&d.address.to_be_bytes())?;
    w.write_all(&d.length.to_be_bytes())?;
    w.write_all(&d.rkey.to_be_bytes())?;
    w.write_all(&[match d.access {
        RemoteAccess::Read => 1,
        RemoteAccess::Write => 2,
    }])
}

pub(super) fn read_window(r: &mut impl Read) -> io::Result<RemoteDescriptor> {
    let qpn = u32::from_be_bytes(field(r)?);
    let address = u64::from_be_bytes(field(r)?);
    let length = u64::from_be_bytes(field(r)?);
    let rkey = u32::from_be_bytes(field(r)?);
    let access = match field(r)? {
        [1] => RemoteAccess::Read,
        [2] => RemoteAccess::Write,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown window access",
            ));
        }
    };
    Ok(RemoteDescriptor {
        qpn,
        address,
        length,
        rkey,
        access,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_and_window_frames_round_trip_and_reject_malformed_input() {
        let params = ConnectionParams {
            qpn: 0x785634,
            psn: 0x102030,
            lid: 79,
            gid: [9; 16],
            link: Link::RoceV2,
            mtu: 4,
            initiator_depth: 2,
            responder_depth: 3,
        };
        let window = RemoteDescriptor {
            qpn: params.qpn,
            address: 0x1_2345_6789,
            length: 17,
            rkey: 0x789abc,
            access: RemoteAccess::Write,
        };
        let mut bytes = Vec::new();
        send_params(&mut bytes, params).unwrap();
        let link_offset = bytes.len() - 4;
        send_window(&mut bytes, window).unwrap();
        let mut stream = bytes.as_slice();
        assert_eq!(read_params(&mut stream).unwrap(), params);
        let window_bytes = stream;
        assert_eq!(read_window(&mut stream).unwrap(), window);
        assert!(stream.is_empty());
        assert!(read_window(&mut &window_bytes[..window_bytes.len() - 1]).is_err());
        bytes[link_offset] = 0xff;
        assert!(read_params(&mut bytes.as_slice()).is_err());
    }
}
