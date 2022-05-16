// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

//! Integration tests.
//!
//! The idea behind these tests is to use actual packet captures to
//! regression test known good captures. This is done by taking a
//! packet capture in the guest as well as on the host -- one for each
//! side of OPTE. These captures are then used to regression test an
//! OPTE pipeline by single-stepping the packets in each capture and
//! verifying that OPTE processing produces the expected bytes.
//!
//! TODO: We should also write tests which programmatically build
//! packets in order to better test more interesting scenarios. For
//! example, attempt an inbound connect to the guest's HTTP server,
//! verify it's blocked by firewall, add a new rule to allow incoming
//! on 80/443, verify the next request passes, remove the rules,
//! verify it once again is denied, etc.
//!
//! TODO This module belongs in oxide_vpc as it's testing VPC-specific
//! configuration.
use std::boxed::Box;
use std::num::NonZeroU32;
use std::ops::Range;
use std::prelude::v1::*;
use std::sync::Arc;
use std::time::Duration;

use pcap_parser::pcap::{self, LegacyPcapBlock, PcapHeader};

use smoltcp::phy::ChecksumCapabilities as CsumCapab;

use zerocopy::AsBytes;

use super::arp::{ArpEth4Payload, ArpEth4PayloadRaw, ArpHdrRaw, ARP_HDR_SZ};
use super::checksum::HeaderChecksum;
use super::ether::{
    EtherHdr, EtherHdrRaw, EtherMeta, EtherType, ETHER_HDR_SZ, ETHER_TYPE_ARP,
    ETHER_TYPE_IPV4,
};
use super::flow_table::FLOW_DEF_EXPIRE_SECS;
use super::geneve::{self, Vni};
use super::headers::{IpAddr, IpCidr, IpMeta, UlpMeta};
use super::ip4::{Ipv4Addr, Ipv4Hdr, Ipv4Meta, Protocol, UlpCsumOpt};
use super::ip6::Ipv6Addr;
use super::packet::{
    Initialized, Packet, PacketRead, PacketReader, PacketWriter, ParseError,
};
use super::port::meta::Meta;
use super::port::{Port, PortBuilder, ProcessError, ProcessResult};
use super::rule::{self, Rule};
use super::tcp::{TcpFlags, TcpHdr};
use super::time::Moment;
use super::udp::{UdpHdr, UdpMeta};
use crate::api::{Direction::*, MacAddr};
use crate::oxide_vpc::api::{
    AddFwRuleReq, GuestPhysAddr, PhysNet, RouterTarget, SetFwRulesReq,
};
use crate::oxide_vpc::engine::overlay::{self, Virt2Phys};
use crate::oxide_vpc::engine::{arp, dyn_nat4, firewall, icmp, router};
use crate::oxide_vpc::{DynNat4Cfg, PortCfg};
use crate::ExecCtx;

use ProcessResult::*;

// I'm not sure if we've defined the MAC address OPTE uses to
// masqurade as the guests gateway.
pub const GW_MAC_ADDR: [u8; 6] = [0xA8, 0x40, 0x25, 0xFF, 0x77, 0x77];

// If we are running `cargo test --feature=usdt`, then make sure to
// register the USDT probes before running any tests.
#[cfg(all(test, feature = "usdt"))]
#[ctor::ctor]
fn register_usdt() {
    usdt::register_probes().unwrap();
}

#[allow(dead_code)]
fn get_header(offset: &[u8]) -> (&[u8], PcapHeader) {
    match pcap::parse_pcap_header(offset) {
        Ok((new_offset, header)) => (new_offset, header),
        Err(e) => panic!("failed to get header: {:?}", e),
    }
}

#[allow(dead_code)]
fn next_block(offset: &[u8]) -> (&[u8], LegacyPcapBlock) {
    match pcap::parse_pcap_frame(offset) {
        Ok((new_offset, block)) => {
            // We always want access to the entire packet.
            assert_eq!(block.origlen, block.caplen);
            (new_offset, block)
        }

        Err(e) => panic!("failed to get next block: {:?}", e),
    }
}

fn lab_cfg() -> PortCfg {
    PortCfg {
        private_ip: "172.20.14.16".parse().unwrap(),
        private_mac: MacAddr::from([0xAA, 0x00, 0x04, 0x00, 0xFF, 0x10]),
        vpc_subnet: "172.20.14.0/24".parse().unwrap(),
        dyn_nat: DynNat4Cfg {
            public_ip: "76.76.21.21".parse().unwrap(),
            ports: Range { start: 1025, end: 4096 },
        },
        gw_mac: MacAddr::from([0xAA, 0x00, 0x04, 0x00, 0xFF, 0x01]),
        gw_ip: "172.20.14.1".parse().unwrap(),

        // XXX These values don't really mean anything in this
        // context. This "lab cfg" was created during the early days
        // of OPTE dev when the VPC implementation was just part of an
        // existing IPv4 network. Any tests relying on this cfg need
        // to be rewritten or deleted.
        vni: Vni::new(99u32).unwrap(),
        // Site 0xF7, Rack 1, Sled 1, Interface 1
        phys_ip: Ipv6Addr::from([
            0xFD00, 0x0000, 0x00F7, 0x0101, 0x0000, 0x0000, 0x0000, 0x0001,
        ]),
        bsvc_addr: PhysNet {
            ether: MacAddr::from([0xA8, 0x40, 0x25, 0x77, 0x77, 0x77]),
            ip: Ipv6Addr::from([
                0xFD, 0x00, 0x11, 0x22, 0x33, 0x44, 0x01, 0xFF, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x77, 0x77,
            ]),
            vni: Vni::new(7777u32).unwrap(),
        },
    }
}

fn oxide_net_builder(name: &str, cfg: &PortCfg) -> PortBuilder {
    let ectx = Arc::new(ExecCtx { log: Box::new(crate::PrintlnLog {}) });
    let name_cstr = crate::CString::new(name).unwrap();
    let mut pb =
        PortBuilder::new(name, name_cstr, cfg.private_mac.into(), ectx.clone());

    let fw_limit = NonZeroU32::new(8096).unwrap();
    let snat_limit = NonZeroU32::new(8096).unwrap();
    let one_limit = NonZeroU32::new(1).unwrap();

    firewall::setup(&mut pb, fw_limit).expect("failed to add firewall layer");
    icmp::setup(&mut pb, cfg, one_limit).expect("failed to add icmp layer");
    dyn_nat4::setup(&mut pb, cfg, snat_limit)
        .expect("failed to add dyn-nat4 layer");
    arp::setup(&mut pb, cfg, one_limit).expect("failed to add ARP layer");
    router::setup(&mut pb, cfg, one_limit).expect("failed to add router layer");
    overlay::setup(&mut pb, cfg, one_limit)
        .expect("failed to add overlay layer");

    // Deny all inbound packets by default.
    pb.add_rule("firewall", In, Rule::match_any(65535, rule::Action::Deny))
        .unwrap();
    // Allow all outbound by default.
    let act = pb.layer_action("firewall", 0).unwrap();
    pb.add_rule("firewall", Out, Rule::match_any(65535, act)).unwrap();
    pb
}

fn oxide_net_setup(name: &str, cfg: &PortCfg) -> Port {
    oxide_net_builder(name, cfg).create(UFT_LIMIT.unwrap(), TCP_LIMIT.unwrap())
}

const UFT_LIMIT: Option<NonZeroU32> = NonZeroU32::new(16);
const TCP_LIMIT: Option<NonZeroU32> = NonZeroU32::new(16);

fn g1_cfg() -> PortCfg {
    PortCfg {
        private_ip: "192.168.77.101".parse().unwrap(),
        private_mac: MacAddr::from([0xA8, 0x40, 0x25, 0xF7, 0x00, 0x65]),
        vpc_subnet: "192.168.77.0/24".parse().unwrap(),
        dyn_nat: DynNat4Cfg {
            // NOTE: This is not a routable IP, but remember that a
            // "public IP" for an Oxide guest could either be a
            // public, routable IP or simply an IP on their wider LAN
            // which the oxide Rack is simply a part of.
            public_ip: "10.77.77.13".parse().unwrap(),
            ports: Range { start: 1025, end: 4096 },
        },
        gw_mac: MacAddr::from([0xA8, 0x40, 0x25, 0xF7, 0x00, 0x1]),
        gw_ip: "192.168.77.1".parse().unwrap(),
        vni: Vni::new(99u32).unwrap(),
        // Site 0xF7, Rack 1, Sled 1, Interface 1
        phys_ip: Ipv6Addr::from([
            0xFD00, 0x0000, 0x00F7, 0x0101, 0x0000, 0x0000, 0x0000, 0x0001,
        ]),
        bsvc_addr: PhysNet {
            ether: MacAddr::from([0xA8, 0x40, 0x25, 0x77, 0x77, 0x77]),
            ip: Ipv6Addr::from([
                0xFD, 0x00, 0x11, 0x22, 0x33, 0x44, 0x01, 0xFF, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x77, 0x77,
            ]),
            vni: Vni::new(7777u32).unwrap(),
        },
    }
}

fn g2_cfg() -> PortCfg {
    PortCfg {
        private_ip: "192.168.77.102".parse().unwrap(),
        private_mac: MacAddr::from([0xA8, 0x40, 0x25, 0xF7, 0x00, 0x66]),
        vpc_subnet: "192.168.77.0/24".parse().unwrap(),
        dyn_nat: DynNat4Cfg {
            // NOTE: This is not a routable IP, but remember that a
            // "public IP" for an Oxide guest could either be a
            // public, routable IP or simply an IP on their wider LAN
            // which the oxide Rack is simply a part of.
            public_ip: "10.77.77.23".parse().unwrap(),
            ports: Range { start: 4097, end: 8192 },
        },
        gw_mac: MacAddr::from([0xA8, 0x40, 0x25, 0xF7, 0x00, 0x1]),
        gw_ip: "192.168.77.1".parse().unwrap(),
        vni: Vni::new(99u32).unwrap(),
        // Site 0xF7, Rack 1, Sled 22, Interface 1
        phys_ip: Ipv6Addr::from([
            0xFD00, 0x0000, 0x00F7, 0x0116, 0x0000, 0x0000, 0x0000, 0x0001,
        ]),
        bsvc_addr: PhysNet {
            ether: MacAddr::from([0xA8, 0x40, 0x25, 0x77, 0x77, 0x77]),
            ip: Ipv6Addr::from([
                0xFD, 0x00, 0x11, 0x22, 0x33, 0x44, 0x01, 0xFF, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x77, 0x77,
            ]),
            vni: Vni::new(7777u32).unwrap(),
        },
    }
}

// Verify that two guests on the same VPC can communicate via overlay.
// I.e., test routing + encap/decap.
#[test]
fn port_transitions() {
    // ================================================================
    // Configure ports for g1 and g2.
    // ================================================================
    let g1_cfg = g1_cfg();
    let g2_cfg = g2_cfg();
    let g2_phys =
        GuestPhysAddr { ether: g2_cfg.private_mac.into(), ip: g2_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g2_cfg.private_ip), g2_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    assert_eq!(g1_port.num_rules("firewall", Out), 1);

    // Add router entry that allows Guest 1 to send to Guest 2.
    router::add_entry(
        &g1_port,
        IpCidr::Ip4(g2_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g2_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    // ================================================================
    // Generate a telnet SYN packet from g1 to g2.
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(7865, 23);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(4224936861);
    let mut ip4 =
        Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, g2_cfg.private_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(EtherType::Ipv4, g1_cfg.private_mac, g1_cfg.gw_mac);

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();

    // ================================================================
    // Try processing the packet while taking the port through a Ready
    // -> Running -> Ready transition. Verify that flows are cleared
    // but rules remain.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Err(ProcessError::BadState(_))));
    g1_port.start();
    assert_eq!(g1_port.num_rules("firewall", Out), 1);
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Ok(Modified)));
    assert_eq!(g1_port.num_flows("firewall", Out), 1);
    assert_eq!(g1_port.num_flows("uft", Out), 1);

    g1_port.reset();
    assert_eq!(g1_port.num_rules("firewall", Out), 1);
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Err(ProcessError::BadState(_))));
    assert_eq!(g1_port.num_flows("firewall", Out), 0);
    assert_eq!(g1_port.num_flows("uft", Out), 0);
}

// Verify that the guest can ping the virtual gateway.
#[test]
fn gateway_icmp4_ping() {
    use smoltcp::wire::{Icmpv4Packet, Icmpv4Repr};
    let g1_cfg = g1_cfg();
    let g2_cfg = g2_cfg();
    let g2_phys =
        GuestPhysAddr { ether: g2_cfg.private_mac.into(), ip: g2_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g2_cfg.private_ip), g2_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();

    let mut pcap = crate::test::PcapBuilder::new("gateway_icmpv4_ping.pcap");

    // ================================================================
    // Generate an ICMP Echo Request from G1 to Virtual GW
    // ================================================================
    let ident = 7;
    let seq_no = 777;
    let data = b"reunion\0";

    let req = Icmpv4Repr::EchoRequest { ident, seq_no, data: &data[..] };

    let mut body_bytes = vec![0u8; req.buffer_len()];
    let mut req_pkt = Icmpv4Packet::new_unchecked(&mut body_bytes);
    let _ = req.emit(&mut req_pkt, &Default::default());

    let mut ip4 = Ipv4Hdr::from(&Ipv4Meta {
        src: g1_cfg.private_ip,
        dst: g1_cfg.gw_ip,
        proto: Protocol::ICMP,
    });
    ip4.set_total_len(ip4.hdr_len() as u16 + req.buffer_len() as u16);
    ip4.compute_hdr_csum();

    let eth = EtherHdr::from(&EtherMeta {
        dst: g1_cfg.gw_mac,
        src: g1_cfg.private_mac,
        ether_type: ETHER_TYPE_IPV4,
    });

    let mut pkt_bytes =
        Vec::with_capacity(ETHER_HDR_SZ + ip4.hdr_len() + req.buffer_len());
    pkt_bytes.extend_from_slice(&eth.as_bytes());
    pkt_bytes.extend_from_slice(&ip4.as_bytes());
    pkt_bytes.extend_from_slice(&body_bytes);
    let mut g1_pkt = Packet::copy(&pkt_bytes).parse().unwrap();
    pcap.add_pkt(&g1_pkt);

    // ================================================================
    // Run the Echo Request through g1's port in the outbound
    // direction and verify it results in an Echo Reply Hairpin packet
    // back to guest.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    let hp = match res {
        Ok(Hairpin(hp)) => hp,
        _ => panic!("expected Hairpin, got {:?}", res),
    };

    let reply = hp.parse().unwrap();
    pcap.add_pkt(&reply);

    // Ether + IPv4
    assert_eq!(reply.body_offset(), 14 + 20);
    assert_eq!(reply.body_seg(), 0);

    let meta = reply.meta();
    assert!(meta.outer.ether.is_none());
    assert!(meta.outer.ip.is_none());
    assert!(meta.outer.ulp.is_none());

    match meta.inner.ether.as_ref() {
        Some(eth) => {
            assert_eq!(eth.src, g1_cfg.gw_mac);
            assert_eq!(eth.dst, g1_cfg.private_mac);
        }

        None => panic!("no inner ether header"),
    }

    match meta.inner.ip.as_ref().unwrap() {
        IpMeta::Ip4(ip4) => {
            assert_eq!(ip4.src, g1_cfg.gw_ip);
            assert_eq!(ip4.dst, g1_cfg.private_ip);
            assert_eq!(ip4.proto, Protocol::ICMP);
        }

        ip6 => panic!("execpted inner IPv4 metadata, got IPv6: {:?}", ip6),
    }

    let mut rdr = PacketReader::new(&reply, ());
    // Need to seek to body.
    rdr.seek(14 + 20).unwrap();
    let reply_body = rdr.copy_remaining();
    let reply_pkt = Icmpv4Packet::new_checked(&reply_body).unwrap();
    // TODO The 2nd arguemnt is the checksum capab, while the default
    // value should verify the checksums better to make this explicit
    // so it's clear what is happening.
    let mut csum = CsumCapab::ignored();
    csum.ipv4 = smoltcp::phy::Checksum::Rx;
    csum.icmpv4 = smoltcp::phy::Checksum::Rx;
    let reply_icmp = Icmpv4Repr::parse(&reply_pkt, &csum).unwrap();
    match reply_icmp {
        Icmpv4Repr::EchoReply {
            ident: r_ident,
            seq_no: r_seq_no,
            data: r_data,
        } => {
            assert_eq!(r_ident, ident);
            assert_eq!(r_seq_no, seq_no);
            assert_eq!(r_data, data);
        }

        _ => panic!("expected Echo Reply, got {:?}", reply_icmp),
    }
}

// Try to send a TCP packet from one guest to another; but in this
// case the guest has not route to the other guest, resulting in the
// packet being dropped.
#[test]
fn overlay_guest_to_guest_no_route() {
    // ================================================================
    // Configure ports for g1 and g2.
    // ================================================================
    let g1_cfg = g1_cfg();
    let g2_cfg = g2_cfg();
    let g2_phys =
        GuestPhysAddr { ether: g2_cfg.private_mac.into(), ip: g2_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g2_cfg.private_ip), g2_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();

    // ================================================================
    // Generate a telnet SYN packet from g1 to g2.
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(7865, 23);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(4224936861);
    let mut ip4 =
        Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, g2_cfg.private_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(EtherType::Ipv4, g1_cfg.private_mac, g1_cfg.gw_mac);

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();

    // ================================================================
    // Run the telnet SYN packet through g1's port in the outbound
    // direction and verify the resulting packet meets expectations.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Ok(ProcessResult::Drop { .. })));
}

// Verify that two guests on the same VPC can communicate via overlay.
// I.e., test routing + encap/decap.
#[test]
fn overlay_guest_to_guest() {
    // ================================================================
    // Configure ports for g1 and g2.
    // ================================================================
    let g1_cfg = g1_cfg();
    let g2_cfg = g2_cfg();
    let g2_phys =
        GuestPhysAddr { ether: g2_cfg.private_mac.into(), ip: g2_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g2_cfg.private_ip), g2_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();

    // Add router entry that allows Guest 1 to send to Guest 2.
    router::add_entry(
        &g1_port,
        IpCidr::Ip4(g2_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g2_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    let g2_port = oxide_net_setup("g2_port", &g2_cfg);
    g2_port.start();

    // Add router entry that allows Guest 2 to send to Guest 1.
    //
    // XXX I just realized that it might make sense to move the router
    // tables up to a global level like the Virt2Phys mappings. This
    // way a new router entry that applies to many guests can placed
    // once instead of on each port individually.
    router::add_entry(
        &g2_port,
        IpCidr::Ip4(g1_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g1_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    // Allow incoming TCP connection from anyone.
    let rule = "dir=in action=allow priority=10 protocol=TCP";
    firewall::add_fw_rule(
        &g2_port,
        &AddFwRuleReq {
            port_name: g2_port.name().to_string(),
            rule: rule.parse().unwrap(),
        },
    )
    .unwrap();

    let mut pcap_guest1 =
        crate::test::PcapBuilder::new("overlay_guest_to_guest-guest-1.pcap");
    let mut pcap_phys1 =
        crate::test::PcapBuilder::new("overlay_guest_to_guest-phys-1.pcap");

    let mut pcap_guest2 =
        crate::test::PcapBuilder::new("overlay_guest_to_guest-guest-2.pcap");
    let mut pcap_phys2 =
        crate::test::PcapBuilder::new("overlay_guest_to_guest-phys-2.pcap");

    // ================================================================
    // Generate a telnet SYN packet from g1 to g2.
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(7865, 23);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(4224936861);
    let mut ip4 =
        Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, g2_cfg.private_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(EtherType::Ipv4, g1_cfg.private_mac, g1_cfg.gw_mac);

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();
    pcap_guest1.add_pkt(&g1_pkt);

    // ================================================================
    // Run the telnet SYN packet through g1's port in the outbound
    // direction and verify the resulting packet meets expectations.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    pcap_phys1.add_pkt(&g1_pkt);
    assert!(matches!(res, Ok(Modified)));

    // Ether + IPv6 + UDP + Geneve + Ether + IPv4 + TCP
    assert_eq!(g1_pkt.body_offset(), 14 + 40 + 8 + 8 + 14 + 20 + 20);
    assert_eq!(g1_pkt.body_seg(), 1);

    let meta = g1_pkt.meta();
    match meta.outer.ether.as_ref() {
        Some(eth) => {
            assert_eq!(eth.src, MacAddr::ZERO);
            assert_eq!(eth.dst, MacAddr::ZERO);
        }

        None => panic!("no outer ether header"),
    }

    match meta.outer.ip.as_ref().unwrap() {
        IpMeta::Ip6(ip6) => {
            assert_eq!(ip6.src, g1_cfg.phys_ip);
            assert_eq!(ip6.dst, g2_cfg.phys_ip);
        }

        val => panic!("expected outer IPv6, got: {:?}", val),
    }

    match meta.outer.ulp.as_ref().unwrap() {
        UlpMeta::Udp(udp) => {
            assert_eq!(udp.src, 7777);
            assert_eq!(udp.dst, geneve::GENEVE_PORT);
        }

        ulp => panic!("expected outer UDP metadata, got: {:?}", ulp),
    }

    match meta.outer.encap.as_ref() {
        Some(geneve) => {
            assert_eq!(geneve.vni, Vni::new(99u32).unwrap());
        }

        None => panic!("expected outer Geneve metadata"),
    }

    match meta.inner.ether.as_ref() {
        Some(eth) => {
            assert_eq!(eth.src, g1_cfg.private_mac);
            assert_eq!(eth.dst, g2_cfg.private_mac);
            assert_eq!(eth.ether_type, ETHER_TYPE_IPV4);
        }

        None => panic!("expected inner Ether header"),
    }

    match meta.inner.ip.as_ref().unwrap() {
        IpMeta::Ip4(ip4) => {
            assert_eq!(ip4.src, g1_cfg.private_ip);
            assert_eq!(ip4.dst, g2_cfg.private_ip);
            assert_eq!(ip4.proto, Protocol::TCP);
        }

        ip6 => panic!("execpted inner IPv4 metadata, got IPv6: {:?}", ip6),
    }

    match meta.inner.ulp.as_ref().unwrap() {
        UlpMeta::Tcp(tcp) => {
            assert_eq!(tcp.src, 7865);
            assert_eq!(tcp.dst, 23);
        }

        ulp => panic!("expected inner TCP metadata, got: {:?}", ulp),
    }

    // ================================================================
    // Now that the packet has been encap'd let's play the role of
    // router and send this inbound to g2's port. For maximum fidelity
    // of the real process we first dump the raw bytes of g1's
    // outgoing packet and then reparse it.
    // ================================================================
    let mblk = g1_pkt.unwrap();
    let mut g2_pkt =
        unsafe { Packet::<Initialized>::wrap(mblk).parse().unwrap() };
    pcap_phys2.add_pkt(&g2_pkt);

    let res = g2_port.process(In, &mut g2_pkt, &mut port_meta);
    pcap_guest2.add_pkt(&g2_pkt);
    assert!(matches!(res, Ok(Modified)));

    // Ether + IPv4 + TCP
    assert_eq!(g2_pkt.body_offset(), 14 + 20 + 20);
    assert_eq!(g2_pkt.body_seg(), 1);

    let g2_meta = g2_pkt.meta();
    assert!(g2_meta.outer.ether.is_none());
    assert!(g2_meta.outer.ip.is_none());
    assert!(g2_meta.outer.ulp.is_none());
    assert!(g2_meta.outer.encap.is_none());

    match g2_meta.inner.ether.as_ref() {
        Some(eth) => {
            assert_eq!(eth.src, g1_cfg.private_mac);
            assert_eq!(eth.dst, g2_cfg.private_mac);
            assert_eq!(eth.ether_type, ETHER_TYPE_IPV4);
        }

        None => panic!("expected inner Ether header"),
    }

    match g2_meta.inner.ip.as_ref().unwrap() {
        IpMeta::Ip4(ip4) => {
            assert_eq!(ip4.src, g1_cfg.private_ip);
            assert_eq!(ip4.dst, g2_cfg.private_ip);
            assert_eq!(ip4.proto, Protocol::TCP);
        }

        ip6 => panic!("execpted inner IPv4 metadata, got IPv6: {:?}", ip6),
    }

    match g2_meta.inner.ulp.as_ref().unwrap() {
        UlpMeta::Tcp(tcp) => {
            assert_eq!(tcp.src, 7865);
            assert_eq!(tcp.dst, 23);
        }

        ulp => panic!("expected inner TCP metadata, got: {:?}", ulp),
    }
}

// Two guests on different, non-peered VPCs should not be able to
// communicate.
#[test]
fn guest_to_guest_diff_vpc_no_peer() {
    // ================================================================
    // Configure ports for g1 and g2. Place g1 on VNI 99 and g2 on VNI
    // 100.
    // ================================================================
    let g1_cfg = g1_cfg();
    let mut g2_cfg = g2_cfg();
    g2_cfg.vni = Vni::new(100u32).unwrap();

    let g1_phys =
        GuestPhysAddr { ether: g1_cfg.private_mac.into(), ip: g1_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses. In this case the only guest in VNI 99 is
    // g1.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g1_cfg.private_ip), g1_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p.clone()).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();

    // Add router entry that allows g1 to talk to any other guest on
    // its VPC subnet.
    //
    // In this case both g1 and g2 have the same subnet. However, g1
    // is part of VNI 99, and g2 is part of VNI 100. Without a VPC
    // Peering Gateway they have no way to reach each other.
    router::add_entry(
        &g1_port,
        IpCidr::Ip4(g1_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g1_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    let g2_port = oxide_net_setup("g2_port", &g2_cfg);
    g2_port.start();

    // Add router entry that allows Guest 2 to send to Guest 1.
    //
    // XXX I just realized that it might make sense to move the router
    // tables up to a global level like the Virt2Phys mappings. This
    // way a new router entry that applies to many guests can placed
    // once instead of on each port individually.
    router::add_entry(
        &g2_port,
        IpCidr::Ip4(g1_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g1_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    // Allow incoming TCP connection from anyone.
    let rule = "dir=in action=allow priority=10 protocol=TCP";
    firewall::add_fw_rule(
        &g2_port,
        &AddFwRuleReq {
            port_name: g2_port.name().to_string(),
            rule: rule.parse().unwrap(),
        },
    )
    .unwrap();

    // ================================================================
    // Generate a telnet SYN packet from g1 to g2.
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(7865, 23);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(4224936861);
    let mut ip4 =
        Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, g2_cfg.private_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(EtherType::Ipv4, g1_cfg.private_mac, g1_cfg.gw_mac);

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();

    // ================================================================
    // Run the telnet SYN packet through g1's port in the outbound
    // direction and verify the packet is dropped.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    println!("=== res: {:?}", res);
    assert!(matches!(res, Ok(ProcessResult::Drop { .. })));
}

// Verify that a guest can communicate with the internet.
#[test]
fn overlay_guest_to_internet() {
    // ================================================================
    // Configure g1 port.
    // ================================================================
    let g1_cfg = g1_cfg();
    let v2p = Arc::new(Virt2Phys::new());
    let mut port_meta = Meta::new();
    port_meta.add(v2p).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();

    // Add router entry that allows Guest 1 to send to Guest 2.
    router::add_entry(
        &g1_port,
        IpCidr::Ip4("0.0.0.0/0".parse().unwrap()),
        RouterTarget::InternetGateway,
    )
    .unwrap();

    let dst_ip = "52.10.128.69".parse().unwrap();

    // ================================================================
    // Generate a TCP SYN packet from g1 to zinascii.com
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(54854, 443);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(1741469041);
    let mut ip4 = Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, dst_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(
        EtherType::Ipv4,
        g1_cfg.private_mac,
        MacAddr::from(GW_MAC_ADDR),
    );

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();

    // ================================================================
    // Run the telnet SYN packet through g1's port in the outbound
    // direction and verify the resulting packet meets expectations.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Ok(Modified)), "bad result: {:?}", res);

    // Ether + IPv6 + UDP + Geneve + Ether + IPv4 + TCP
    assert_eq!(g1_pkt.body_offset(), 14 + 40 + 8 + 8 + 14 + 20 + 20);
    assert_eq!(g1_pkt.body_seg(), 1);

    let meta = g1_pkt.meta();
    match meta.outer.ether.as_ref() {
        Some(eth) => {
            assert_eq!(eth.src, MacAddr::ZERO);
            assert_eq!(eth.dst, MacAddr::ZERO);
        }

        None => panic!("no outer ether header"),
    }

    match meta.outer.ip.as_ref().unwrap() {
        IpMeta::Ip6(ip6) => {
            assert_eq!(ip6.src, g1_cfg.phys_ip);
            assert_eq!(ip6.dst, g1_cfg.bsvc_addr.ip);
        }

        val => panic!("expected outer IPv6, got: {:?}", val),
    }

    match meta.outer.ulp.as_ref().unwrap() {
        UlpMeta::Udp(udp) => {
            assert_eq!(udp.src, 7777);
            assert_eq!(udp.dst, geneve::GENEVE_PORT);
        }

        ulp => panic!("expected outer UDP metadata, got: {:?}", ulp),
    }

    match meta.outer.encap.as_ref() {
        Some(geneve) => {
            assert_eq!(geneve.vni, g1_cfg.bsvc_addr.vni);
        }

        None => panic!("expected outer Geneve metadata"),
    }

    match meta.inner.ether.as_ref() {
        Some(eth) => {
            assert_eq!(eth.src, g1_cfg.private_mac);
            assert_eq!(eth.dst, g1_cfg.bsvc_addr.ether.into());
            assert_eq!(eth.ether_type, ETHER_TYPE_IPV4);
        }

        None => panic!("expected inner Ether header"),
    }

    match meta.inner.ip.as_ref().unwrap() {
        IpMeta::Ip4(ip4) => {
            assert_eq!(ip4.src, g1_cfg.dyn_nat.public_ip);
            assert_eq!(ip4.dst, dst_ip);
            assert_eq!(ip4.proto, Protocol::TCP);
        }

        ip6 => panic!("execpted inner IPv4 metadata, got IPv6: {:?}", ip6),
    }

    match meta.inner.ulp.as_ref().unwrap() {
        UlpMeta::Tcp(tcp) => {
            assert_eq!(tcp.src, g1_cfg.dyn_nat.ports.rev().next().unwrap());
            assert_eq!(tcp.dst, 443);
        }

        ulp => panic!("expected inner TCP metadata, got: {:?}", ulp),
    }
}

#[test]
fn bad_ip_len() {
    let cfg = lab_cfg();
    let pkt = Packet::alloc(42);

    let ether = EtherHdr::from(&EtherMeta {
        src: cfg.private_mac,
        dst: MacAddr::BROADCAST,
        ether_type: ETHER_TYPE_IPV4,
    });

    let mut ip = Ipv4Hdr::from(&Ipv4Meta {
        src: "0.0.0.0".parse().unwrap(),
        dst: Ipv4Addr::LOCAL_BCAST,
        proto: Protocol::UDP,
    });

    // We write a total legnth of 4 bytes, which is completely bogus
    // for an IP header and should return an error during processing.
    ip.set_total_len(4);

    let udp = UdpHdr::from(&UdpMeta { src: 68, dst: 67 });

    let mut wtr = PacketWriter::new(pkt, None);
    let _ = wtr.write(&ether.as_bytes()).unwrap();
    let _ = wtr.write(&ip.as_bytes()).unwrap();
    let _ = wtr.write(&udp.as_bytes()).unwrap();
    let res = wtr.finish().parse();
    assert_eq!(
        res.err().unwrap(),
        ParseError::BadHeader("IPv4: BadTotalLen { total_len: 4 }".to_string())
    );

    let pkt = Packet::alloc(42);

    let ether = EtherHdr::from(&EtherMeta {
        src: cfg.private_mac,
        dst: MacAddr::BROADCAST,
        ether_type: ETHER_TYPE_IPV4,
    });

    let mut ip = Ipv4Hdr::from(&Ipv4Meta {
        src: "0.0.0.0".parse().unwrap(),
        dst: Ipv4Addr::LOCAL_BCAST,
        proto: Protocol::UDP,
    });

    // We write an incorrect total legnth of 40 bytes, but the real
    // total length should only be 28 bytes.
    ip.set_total_len(40);

    let udp = UdpHdr::from(&UdpMeta { src: 68, dst: 67 });

    let mut wtr = PacketWriter::new(pkt, None);
    let _ = wtr.write(&ether.as_bytes()).unwrap();
    let _ = wtr.write(&ip.as_bytes()).unwrap();
    let _ = wtr.write(&udp.as_bytes()).unwrap();
    let res = wtr.finish().parse();
    assert_eq!(
        res.err().unwrap(),
        ParseError::BadInnerIpLen { expected: 8, actual: 20 }
    );
}

// Verify that OPTE generates a hairpin ARP reply when the guest
// queries for the gateway.
#[test]
fn arp_gateway() {
    use super::arp::ArpOp;
    use super::ether::ETHER_TYPE_IPV4;

    let cfg = g1_cfg();
    let mut port_meta = Meta::new();
    let port = oxide_net_setup("arp_hairpin", &cfg);
    port.start();
    let reply_hdr_sz = ETHER_HDR_SZ + ARP_HDR_SZ;

    let pkt = Packet::alloc(42);
    let eth_hdr = EtherHdrRaw {
        dst: [0xff; 6],
        src: cfg.private_mac.bytes(),
        ether_type: [0x08, 0x06],
    };

    let arp_hdr = ArpHdrRaw {
        htype: [0x00, 0x01],
        ptype: [0x08, 0x00],
        hlen: 0x06,
        plen: 0x04,
        op: [0x00, 0x01],
    };

    let arp = ArpEth4Payload {
        sha: cfg.private_mac,
        spa: cfg.private_ip,
        tha: MacAddr::from([0x00; 6]),
        tpa: cfg.gw_ip,
    };

    let mut wtr = PacketWriter::new(pkt, None);
    let _ = wtr.write(eth_hdr.as_bytes()).unwrap();
    let _ = wtr.write(arp_hdr.as_bytes()).unwrap();
    let _ = wtr.write(ArpEth4PayloadRaw::from(arp).as_bytes()).unwrap();
    let mut pkt = wtr.finish().parse().unwrap();

    let res = port.process(Out, &mut pkt, &mut port_meta);
    match res {
        Ok(Hairpin(hppkt)) => {
            let hppkt = hppkt.parse().unwrap();
            let meta = hppkt.meta();
            let ethm = meta.inner.ether.as_ref().unwrap();
            let arpm = meta.inner.arp.as_ref().unwrap();
            assert_eq!(ethm.dst, cfg.private_mac);
            assert_eq!(ethm.src, cfg.gw_mac);
            assert_eq!(ethm.ether_type, ETHER_TYPE_ARP);
            assert_eq!(arpm.op, ArpOp::Reply);
            assert_eq!(arpm.ptype, ETHER_TYPE_IPV4);

            let mut rdr = PacketReader::new(&hppkt, ());
            assert!(rdr.seek(reply_hdr_sz).is_ok());
            let arp = ArpEth4Payload::from(
                &ArpEth4PayloadRaw::parse(&mut rdr).unwrap(),
            );

            assert_eq!(arp.sha, cfg.gw_mac);
            assert_eq!(arp.spa, cfg.gw_ip);
            assert_eq!(arp.tha, cfg.private_mac);
            assert_eq!(arp.tpa, cfg.private_ip);
        }

        res => panic!("expected a Hairpin, got {:?}", res),
    }
}

#[test]
fn flow_expiration() {
    // ================================================================
    // Configure ports for g1 and g2.
    // ================================================================
    let g1_cfg = g1_cfg();
    let g2_cfg = g2_cfg();
    let g2_phys =
        GuestPhysAddr { ether: g2_cfg.private_mac.into(), ip: g2_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g2_cfg.private_ip), g2_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();
    let now = Moment::now();

    // Add router entry that allows Guest 1 to send to Guest 2.
    router::add_entry(
        &g1_port,
        IpCidr::Ip4(g2_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g2_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    // ================================================================
    // Generate a telnet SYN packet from g1 to g2.
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(7865, 23);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(4224936861);
    let mut ip4 =
        Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, g2_cfg.private_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(EtherType::Ipv4, g1_cfg.private_mac, g1_cfg.gw_mac);

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();

    // ================================================================
    // Run the telnet SYN packet through g1's port in the outbound
    // direction and verify the resulting packet meets expectations.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Ok(Modified)));

    // ================================================================
    // Verify expiration
    // ================================================================
    g1_port.expire_flows(now + Duration::new(FLOW_DEF_EXPIRE_SECS as u64, 0));
    assert_eq!(g1_port.num_flows("firewall", In), 1);
    assert_eq!(g1_port.num_flows("firewall", Out), 1);
    assert_eq!(g1_port.num_flows("uft", In), 0);
    assert_eq!(g1_port.num_flows("uft", Out), 1);

    g1_port
        .expire_flows(now + Duration::new(FLOW_DEF_EXPIRE_SECS as u64 + 1, 0));
    assert_eq!(g1_port.num_flows("firewall", In), 0);
    assert_eq!(g1_port.num_flows("firewall", Out), 0);
    assert_eq!(g1_port.num_flows("uft", In), 0);
    assert_eq!(g1_port.num_flows("uft", Out), 0);
}

#[test]
fn firewall_replace_rules() {
    // ================================================================
    // Configure ports for g1 and g2.
    // ================================================================
    let g1_cfg = g1_cfg();
    let g2_cfg = g2_cfg();
    let g2_phys =
        GuestPhysAddr { ether: g2_cfg.private_mac.into(), ip: g2_cfg.phys_ip };

    // Add V2P mappings that allow guests to resolve each others
    // physical addresses.
    let v2p = Arc::new(Virt2Phys::new());
    v2p.set(IpAddr::Ip4(g2_cfg.private_ip), g2_phys);
    let mut port_meta = Meta::new();
    port_meta.add(v2p.clone()).unwrap();

    let g1_port = oxide_net_setup("g1_port", &g1_cfg);
    g1_port.start();

    // Add router entry that allows Guest 1 to send to Guest 2.
    router::add_entry(
        &g1_port,
        IpCidr::Ip4(g2_cfg.vpc_subnet.cidr()),
        RouterTarget::VpcSubnet(IpCidr::Ip4(g2_cfg.vpc_subnet.cidr())),
    )
    .unwrap();

    let g2_port = oxide_net_setup("g2_port", &g2_cfg);
    g2_port.start();

    // Allow incoming TCP connection on g2 from anyone.
    let rule = "dir=in action=allow priority=10 protocol=TCP";
    firewall::add_fw_rule(
        &g2_port,
        &AddFwRuleReq {
            port_name: g2_port.name().to_string(),
            rule: rule.parse().unwrap(),
        },
    )
    .unwrap();

    // ================================================================
    // Generate a telnet SYN packet from g1 to g2.
    // ================================================================
    let body = vec![];
    let mut tcp = TcpHdr::new(7865, 23);
    tcp.set_flags(TcpFlags::SYN);
    tcp.set_seq(4224936861);
    let mut ip4 =
        Ipv4Hdr::new_tcp(&mut tcp, &body, g1_cfg.private_ip, g2_cfg.private_ip);
    ip4.compute_hdr_csum();
    let tcp_csum =
        ip4.compute_ulp_csum(UlpCsumOpt::Full, &tcp.as_bytes(), &body);
    tcp.set_csum(HeaderChecksum::from(tcp_csum).bytes());
    let eth = EtherHdr::new(EtherType::Ipv4, g1_cfg.private_mac, g1_cfg.gw_mac);

    let mut bytes = vec![];
    bytes.extend_from_slice(&eth.as_bytes());
    bytes.extend_from_slice(&ip4.as_bytes());
    bytes.extend_from_slice(&tcp.as_bytes());
    bytes.extend_from_slice(&body);
    let mut g1_pkt = Packet::copy(&bytes).parse().unwrap();

    // ================================================================
    // Run the telnet SYN packet through g1's port in the outbound
    // direction and verify if passes the firewall.
    // ================================================================
    let res = g1_port.process(Out, &mut g1_pkt, &mut port_meta);
    assert!(matches!(res, Ok(Modified)));

    // ================================================================
    // Modify the outgoing ruleset, but still allow the traffic to
    // pass. This test makes sure that flow table entries are updated
    // without issue and everything still works.
    //
    // XXX It would be nice if tests could verify that a probe fires
    // (in this case uft-invalidated) without using dtrace.
    // ================================================================
    let any_out = "dir=out action=deny priority=65535 protocol=any";
    let tcp_out = "dir=out action=allow priority=1000 protocol=TCP";
    firewall::set_fw_rules(
        &g1_port,
        &SetFwRulesReq {
            port_name: g1_port.name().to_string(),
            rules: vec![any_out.parse().unwrap(), tcp_out.parse().unwrap()],
        },
    )
    .unwrap();
    port_meta.clear();
    port_meta.add(v2p.clone()).unwrap();
    let mut g1_pkt2 = Packet::copy(&bytes).parse().unwrap();
    let res = g1_port.process(Out, &mut g1_pkt2, &mut port_meta);
    assert!(matches!(res, Ok(Modified)));

    // ================================================================
    // Now that the packet has been encap'd let's play the role of
    // router and send this inbound to g2's port. For maximum fidelity
    // of the real process we first dump the raw bytes of g1's
    // outgoing packet and then reparse it.
    // ================================================================
    let mblk = g1_pkt.unwrap();
    let mut g2_pkt =
        unsafe { Packet::<Initialized>::wrap(mblk).parse().unwrap() };
    port_meta.clear();
    port_meta.add(v2p).unwrap();
    let res = g2_port.process(In, &mut g2_pkt, &mut port_meta);
    assert!(matches!(res, Ok(Modified)));

    // ================================================================
    // Replace g2's firewall rule set to deny all inbound TCP traffic.
    // Verify the rules have been replaced and retry processing of the
    // g2_pkt, but this time it should be dropped.
    // ================================================================
    assert_eq!(g2_port.num_rules("firewall", In), 2);
    assert_eq!(g2_port.num_flows("firewall", In), 1);
    let new_rule = "dir=in action=deny priority=1000 protocol=TCP";
    firewall::set_fw_rules(
        &g2_port,
        &SetFwRulesReq {
            port_name: g2_port.name().to_string(),
            rules: vec![new_rule.parse().unwrap()],
        },
    )
    .unwrap();
    assert_eq!(g2_port.num_rules("firewall", In), 1);
    assert_eq!(g2_port.num_flows("firewall", In), 0);

    // Need to create a new g2_pkt by re-running the process.
    let mut g1_pkt3 = Packet::copy(&bytes).parse().unwrap();
    let res = g1_port.process(Out, &mut g1_pkt3, &mut port_meta);
    assert!(matches!(res, Ok(Modified)));
    let mblk2 = g1_pkt3.unwrap();
    let mut g2_pkt2 =
        unsafe { Packet::<Initialized>::wrap(mblk2).parse().unwrap() };

    // Verify the packet is dropped and that the firewall flow table
    // entry (along with its dual) was invalidated.
    let res = g2_port.process(In, &mut g2_pkt2, &mut port_meta);
    use super::port::DropReason;
    match res {
        Ok(ProcessResult::Drop { reason: DropReason::Layer { name } }) => {
            assert_eq!("firewall", name);
        }

        _ => panic!("expected drop but got: {:?}", res),
    }
}
