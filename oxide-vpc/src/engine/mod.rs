// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

pub mod firewall;
pub mod gateway;
pub mod nat;
pub mod overlay;
#[cfg(any(feature = "std", test))]
pub mod print;
pub mod router;

use crate::api::VpcCfg;
use opte::engine::ether::EtherType;
use opte::engine::flow_table::FlowTable;
use opte::engine::headers::EncapMeta;
use opte::engine::ip4::Protocol;
use opte::engine::packet::HeaderOffsets;
use opte::engine::packet::InnerFlowId;
use opte::engine::packet::Packet;
use opte::engine::packet::PacketInfo;
use opte::engine::packet::PacketMeta;
use opte::engine::packet::PacketRead;
use opte::engine::packet::PacketReaderMut;
use opte::engine::packet::ParseError;
use opte::engine::packet::Parsed;
use opte::engine::port::UftEntry;
use opte::engine::Direction;
use opte::engine::HdlPktAction;
use opte::engine::HdlPktError;
use opte::engine::NetworkImpl;
use opte::engine::NetworkParser;

use opte::engine::arp;
use opte::engine::arp::ArpEthIpv4;
use opte::engine::arp::ArpOp;
use opte::engine::ether::ETHER_TYPE_IPV4;
use opte::engine::ip4::Ipv4Addr;

#[derive(Clone, Copy, Debug)]
pub struct VpcParser {
    // XXX-EXT-IP hack
    pub proxy_arp_enable: bool,
}

impl VpcParser {
    pub fn new() -> Self {
        Self { proxy_arp_enable: false }
    }
}

#[derive(Clone, Debug)]
pub struct VpcNetwork {
    pub cfg: VpcCfg,
}

// The ARP HTYPE for Ethernet.
const HTYPE_ETHER: u16 = 1;

fn is_arp_req(arp: &ArpEthIpv4) -> bool {
    arp.htype == HTYPE_ETHER
        && arp.ptype == ETHER_TYPE_IPV4
        && arp.op == ArpOp::Request
}

fn is_arp_req_for_tpa(tpa: Ipv4Addr, arp: &ArpEthIpv4) -> bool {
    if is_arp_req(&arp) {
        if arp.tpa == tpa {
            return true;
        }
    }

    false
}

impl VpcNetwork {
    fn handle_arp_out(
        &self,
        pkt: &mut Packet<Parsed>,
    ) -> Result<HdlPktAction, HdlPktError> {
        let arp_start = pkt.hdr_offsets().inner.ether.hdr_len;
        let mut rdr = pkt.get_rdr_mut();
        rdr.seek(arp_start).unwrap();
        let arp = ArpEthIpv4::parse(&mut rdr)
            .map_err(|_| HdlPktError("outbound ARP"))?;
        let gw_ip = self.cfg.ipv4_cfg().unwrap().gateway_ip;

        if is_arp_req_for_tpa(gw_ip, &arp) {
            let gw_mac = self.cfg.gateway_mac;

            let hp = arp::gen_arp_reply(gw_mac, gw_ip, arp.sha, arp.spa);
            return Ok(HdlPktAction::Hairpin(hp));
        }

        Ok(HdlPktAction::Deny)
    }

    fn handle_arp_in(
        &self,
        pkt: &mut Packet<Parsed>,
    ) -> Result<HdlPktAction, HdlPktError> {
        let arp_start = pkt.hdr_offsets().inner.ether.hdr_len;
        let mut rdr = pkt.get_rdr_mut();
        rdr.seek(arp_start).unwrap();
        let arp = ArpEthIpv4::parse(&mut rdr)
            .map_err(|_| HdlPktError("inbound ARP"))?;
        let proxy_arp = self.cfg.proxy_arp_enable;
        let guest_mac = self.cfg.guest_mac;
        let ip_cfg = self.cfg.ipv4_cfg().unwrap();

        // ================================================================
        // Proxy ARP for any incoming requests for guest's external IP.
        //
        // XXX-EXT-IP This is a hack to get guest access working until
        // we have boundary services integrated.
        // ================================================================
        if let Some(external_ip) = ip_cfg.external_ips {
            if proxy_arp && is_arp_req_for_tpa(external_ip, &arp) {
                let hp = arp::gen_arp_reply(
                    guest_mac,
                    external_ip,
                    arp.sha,
                    arp.spa,
                );
                return Ok(HdlPktAction::Hairpin(hp));
            }
        }

        // ================================================================
        // Proxy ARP for any incoming requests for guest's SNAT IP.
        //
        // This is not great because once you have more than one guest
        // it means there is an ARP battle for the same SNAT IP. One
        // more rason why this hack needs to go away.
        //
        // XXX-EXT-IP This is a hack to get guest access working until
        // we have boundary services integrated.
        // ================================================================
        if let Some(snat) = ip_cfg.snat.as_ref() {
            if proxy_arp && is_arp_req_for_tpa(snat.external_ip, &arp) {
                let hp = arp::gen_arp_reply(
                    guest_mac,
                    snat.external_ip,
                    arp.sha,
                    arp.spa,
                );
                return Ok(HdlPktAction::Hairpin(hp));
            }
        }

        Ok(HdlPktAction::Deny)
    }
}

impl NetworkImpl for VpcNetwork {
    type Parser = VpcParser;

    fn handle_pkt(
        &self,
        dir: Direction,
        pkt: &mut Packet<Parsed>,
        _uft_in: &FlowTable<UftEntry<InnerFlowId>>,
        _uft_out: &FlowTable<UftEntry<InnerFlowId>>,
    ) -> Result<HdlPktAction, HdlPktError> {
        match (dir, pkt.meta().inner.ether.ether_type) {
            (Direction::Out, EtherType::Arp) => self.handle_arp_out(pkt),

            // XXX-EXT-IP This is only need for the hack.
            (Direction::In, EtherType::Arp) => self.handle_arp_in(pkt),

            _ => Ok(HdlPktAction::Deny),
        }
    }

    fn parser(&self) -> Self::Parser {
        VpcParser { proxy_arp_enable: self.cfg.proxy_arp_enable }
    }
}

impl NetworkParser for VpcParser {
    fn parse_outbound(
        &self,
        rdr: &mut PacketReaderMut,
    ) -> Result<PacketInfo, ParseError> {
        let mut meta = PacketMeta::default();
        let mut offsets = HeaderOffsets::default();
        let (ether_hi, _hdr) = Packet::parse_ether(rdr)?;
        meta.inner.ether = ether_hi.meta;
        offsets.inner.ether = ether_hi.offset;
        let ether_type = ether_hi.meta.ether_type;

        let (ip_hi, pseudo_csum) = match ether_type {
            EtherType::Arp => {
                return Ok(PacketInfo { meta, offsets, body_csum: None });
            }

            EtherType::Ipv4 => {
                let (ip_hi, hdr) = Packet::parse_ip4(rdr)?;
                (ip_hi, hdr.pseudo_csum())
            }

            EtherType::Ipv6 => {
                let (ip_hi, hdr) = Packet::parse_ip6(rdr)?;
                (ip_hi, hdr.pseudo_csum())
            }

            _ => return Err(ParseError::UnexpectedEtherType(ether_type)),
        };

        meta.inner.ip = Some(ip_hi.meta);
        offsets.inner.ip = Some(ip_hi.offset);

        let (ulp_hi, ulp_hdr) = match ip_hi.meta.proto() {
            Protocol::ICMP => {
                return Ok(PacketInfo { meta, offsets, body_csum: None });
                // todo!("need to reintrodouce ICMP as pseudo-ULP header");
                // pkt.parse_icmp()?,
            }

            Protocol::ICMPv6 => {
                return Ok(PacketInfo { meta, offsets, body_csum: None });
            }

            Protocol::TCP => Packet::parse_tcp(rdr)?,
            Protocol::UDP => Packet::parse_udp(rdr)?,
            proto => return Err(ParseError::UnexpectedProtocol(proto)),
        };

        meta.inner.ulp = Some(ulp_hi.meta);
        offsets.inner.ulp = Some(ulp_hi.offset);

        let body_csum = if let Some(mut csum) = ulp_hdr.csum_minus_hdr() {
            csum -= pseudo_csum;
            Some(csum)
        } else {
            None
        };

        Ok(PacketInfo { meta, offsets, body_csum })
    }

    fn parse_inbound(
        &self,
        rdr: &mut PacketReaderMut,
    ) -> Result<PacketInfo, ParseError> {
        let mut meta = PacketMeta::default();
        let mut offsets = HeaderOffsets::default();

        // XXX-EXT-IP If proxy ARP is enabled, then we are not on the
        // Oxide Rack Network and have no encap.
        if !self.proxy_arp_enable {
            let (outer_ether_hi, _hdr) = Packet::parse_ether(rdr)?;
            meta.outer.ether = Some(outer_ether_hi.meta);
            offsets.outer.ether = Some(outer_ether_hi.offset);
            let outer_et = outer_ether_hi.meta.ether_type;

            // VPC traffic is delivered exclusively on an IPv6 +
            // Geneve underlay.
            let outer_ip_hi = match outer_et {
                EtherType::Ipv6 => Packet::parse_ip6(rdr)?.0,

                _ => return Err(ParseError::UnexpectedEtherType(outer_et)),
            };

            meta.outer.ip = Some(outer_ip_hi.meta);
            offsets.outer.ip = Some(outer_ip_hi.offset);

            let (geneve_hi, _geneve_hdr) = match outer_ip_hi.meta.proto() {
                Protocol::UDP => Packet::parse_geneve(rdr)?,
                proto => return Err(ParseError::UnexpectedProtocol(proto)),
            };

            meta.outer.encap = Some(EncapMeta::from(geneve_hi.meta));
            offsets.outer.encap = Some(geneve_hi.offset);
        }

        let (inner_ether_hi, _) = Packet::parse_ether(rdr)?;
        meta.inner.ether = inner_ether_hi.meta;
        offsets.inner.ether = inner_ether_hi.offset;
        let inner_et = inner_ether_hi.meta.ether_type;

        let (inner_ip_hi, pseudo_csum) = match inner_et {
            EtherType::Ipv4 => {
                let (ip_hi, hdr) = Packet::parse_ip4(rdr)?;
                (ip_hi, hdr.pseudo_csum())
            }

            EtherType::Ipv6 => {
                let (ip_hi, hdr) = Packet::parse_ip6(rdr)?;
                (ip_hi, hdr.pseudo_csum())
            }

            EtherType::Arp => {
                // XXX-EXT-IP Need to allow inbound ARP for proxy ARP
                // to work.
                if self.proxy_arp_enable {
                    return Ok(PacketInfo { meta, offsets, body_csum: None });
                } else {
                    return Err(ParseError::UnexpectedEtherType(inner_et));
                }
            }

            _ => return Err(ParseError::UnexpectedEtherType(inner_et)),
        };

        meta.inner.ip = Some(inner_ip_hi.meta);
        offsets.inner.ip = Some(inner_ip_hi.offset);

        let (inner_ulp_hi, inner_ulp_hdr) = match inner_ip_hi.meta.proto() {
            Protocol::ICMP => {
                return Ok(PacketInfo { meta, offsets, body_csum: None });
                // todo!("need to reintrodouce ICMP as pseudo-ULP header");
                // pkt.parse_icmp()?,
            }

            Protocol::ICMPv6 => {
                return Ok(PacketInfo { meta, offsets, body_csum: None });
            }

            Protocol::TCP => Packet::parse_tcp(rdr)?,
            Protocol::UDP => Packet::parse_udp(rdr)?,
            proto => return Err(ParseError::UnexpectedProtocol(proto)),
        };

        meta.inner.ulp = Some(inner_ulp_hi.meta);
        offsets.inner.ulp = Some(inner_ulp_hi.offset);
        let body_csum = if let Some(mut csum) = inner_ulp_hdr.csum_minus_hdr() {
            csum -= pseudo_csum;
            Some(csum)
        } else {
            None
        };

        Ok(PacketInfo { meta, offsets, body_csum })
    }
}
