use super::*;

#[test]
fn records_match_the_original_two_peer_example() {
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
    let frame = [
        0, 0x78, 0x56, 0x34, 0, 0x10, 0x20, 0x30, 0, 79, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
        9, 9, 3, 4, 2, 3,
    ];
    assert_eq!(params.encode(), frame);
    assert_eq!(ConnectionParams::decode(&frame).unwrap(), params);
    for (link, tag) in [(Link::InfiniBand, 1), (Link::RoceV1, 2), (Link::RoceV2, 3)] {
        let p = ConnectionParams { link, ..params };
        let bytes = p.encode();
        assert_eq!(bytes[26], tag);
        assert_eq!(ConnectionParams::decode(&bytes).unwrap(), p);
    }
    let mut invalid = frame;
    invalid[26] = 0xff;
    assert!(ConnectionParams::decode(&invalid).is_err());
    let d = RemoteDescriptor {
        qpn: params.qpn,
        address: 0x1_2345_6789,
        length: 17,
        rkey: 0x789abc,
        access: RemoteAccess::Write,
    };
    let record = [
        0, 0x78, 0x56, 0x34, 0, 0, 0, 1, 0x23, 0x45, 0x67, 0x89, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0x78,
        0x9a, 0xbc, 2,
    ];
    assert_eq!(d.to_bytes(), record);
    assert_eq!(RemoteDescriptor::from_bytes(record).unwrap(), d);
    for (range, byte) in [
        (0..4, 0),
        (0..4, 0xff),
        (4..12, 0xff),
        (12..20, 0),
        (20..24, 0),
        (24..25, 3),
    ] {
        let mut bad = record;
        bad[range].fill(byte);
        assert!(RemoteDescriptor::from_bytes(bad).is_err());
    }
}
