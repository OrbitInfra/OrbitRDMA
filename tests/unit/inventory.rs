use super::*;

#[test]
fn linux_inventory_filters_invalid_routes_and_keeps_the_netdev_identity() {
    let root = std::env::temp_dir().join(format!("orbit-rdma-gids-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let port = root.join("class/infiniband/rxe0/ports/1");
    for name in ["gids", "gid_attrs/types", "gid_attrs/ndevs"] {
        fs::create_dir_all(port.join(name)).unwrap();
    }
    let netdev = root.join("class/net/eth0");
    fs::create_dir_all(&netdev).unwrap();
    fs::write(netdev.join("ifindex"), "7\n").unwrap();
    for (path, contents) in [
        ("state", "4: ACTIVE"),
        ("link_layer", "Ethernet"),
        ("gids/1", "::ffff:192.0.2.1"),
        ("gids/2", "ff00::1"),
        ("gid_attrs/types/1", "RoCE v2"),
        ("gid_attrs/ndevs/1", "eth0"),
    ] {
        fs::write(port.join(path), contents).unwrap();
    }
    assert_eq!(ports(&root, "rxe0"), [1]);
    let routes = port_addresses(&root, "rxe0", 1, None);
    assert_eq!(routes.len(), 1);
    assert!(routes[0].active);
    assert_eq!(routes[0].link, Link::RoceV2);
    assert_eq!(routes[0].net_device.as_deref(), Some("eth0"));
    assert_eq!(routes[0].net_device_index, 7);
    assert!(port_addresses(&root, "rxe0", 1, Some(2)).is_empty());
    assert!(ports(&root, "../rxe0").is_empty());
    fs::write(port.join("gid_attrs/ndevs/1"), "../eth0").unwrap();
    assert!(matches!(
        gid_metadata(&root, "rxe0", 1, 1, 2),
        Err(Error::Unsupported(_))
    ));
    assert!(port_addresses(&root, "rxe0", 1, None).is_empty());
    fs::remove_dir_all(root).unwrap();
}
