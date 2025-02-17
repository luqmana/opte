// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

//! OPTE driver administration library
// Copyright 2021 Oxide Computer Company

use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;

use opte::api::NoResp;
use opte::api::OpteCmd;
use opte::api::SetXdeUnderlayReq;
use opte::engine::ioctl::{self as api};
use opte_ioctl::run_cmd_ioctl;
use opte_ioctl::Error;
use oxide_vpc::api::AddFwRuleReq;
use oxide_vpc::api::AddRouterEntryReq;
use oxide_vpc::api::CreateXdeReq;
use oxide_vpc::api::DeleteXdeReq;
use oxide_vpc::api::FirewallRule;
use oxide_vpc::api::ListPortsResp;
use oxide_vpc::api::RemFwRuleReq;
use oxide_vpc::api::SetFwRulesReq;
use oxide_vpc::api::SetVirt2PhysReq;
use oxide_vpc::api::VpcCfg;
use oxide_vpc::engine::overlay;

/// The handle used to send administration commands to the OPTE
/// control node.
#[derive(Debug)]
pub struct OpteAdm {
    device: File,
}

impl OpteAdm {
    pub const DLD_CTL: &'static str = "/dev/dld";

    /// Add xde device
    pub fn create_xde(
        &self,
        name: &str,
        cfg: VpcCfg,
        passthrough: bool,
    ) -> Result<NoResp, Error> {
        use libnet::link;

        let linkid = link::create_link_id(
            name,
            libnet::LinkClass::Xde,
            libnet::LinkFlags::Active,
        )?;

        let xde_devname = name.into();
        let cmd = OpteCmd::CreateXde;
        let req = CreateXdeReq { xde_devname, linkid, cfg, passthrough };
        let res = run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req));

        if res.is_err() {
            let _ = link::delete_link_id(linkid, libnet::LinkFlags::Active);
        }

        res
    }

    /// Delete xde device
    pub fn delete_xde(&self, name: &str) -> Result<NoResp, Error> {
        let link_id = libnet::LinkHandle::Name(name.into()).id()?;
        let req = DeleteXdeReq { xde_devname: name.into() };
        let cmd = OpteCmd::DeleteXde;
        let resp = run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))?;
        libnet::link::delete_link_id(link_id, libnet::LinkFlags::Active)?;
        Ok(resp)
    }

    /// Set xde underlay devices
    pub fn set_xde_underlay(
        &self,
        u1: &str,
        u2: &str,
    ) -> Result<NoResp, Error> {
        let req = SetXdeUnderlayReq { u1: u1.into(), u2: u2.into() };
        let cmd = OpteCmd::SetXdeUnderlay;
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))
    }

    /// Add a firewall rule
    pub fn add_firewall_rule(
        &self,
        port_name: &str,
        rule: &FirewallRule,
    ) -> Result<NoResp, Error> {
        let cmd = OpteCmd::AddFwRule;
        let req = AddFwRuleReq {
            port_name: port_name.to_string(),
            rule: rule.clone(),
        };
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))
    }

    pub fn set_firewall_rules(
        &self,
        port_name: &str,
        rules: Vec<FirewallRule>,
    ) -> Result<NoResp, Error> {
        let cmd = OpteCmd::SetFwRules;
        let req =
            SetFwRulesReq { port_name: port_name.to_string(), rules: rules };
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))
    }

    /// Return the contents of an OPTE layer.
    pub fn get_layer_by_name(
        &self,
        port_name: &str,
        name: &str,
    ) -> Result<api::DumpLayerResp, Error> {
        let cmd = OpteCmd::DumpLayer;
        let req = api::DumpLayerReq {
            port_name: port_name.to_string(),
            name: name.to_string(),
        };
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))
    }

    /// List all the ports.
    pub fn list_ports(&self) -> Result<ListPortsResp, Error> {
        run_cmd_ioctl(self.device.as_raw_fd(), OpteCmd::ListPorts, None::<&()>)
    }

    pub fn list_layers(
        &self,
        port: &str,
    ) -> Result<api::ListLayersResp, Error> {
        let cmd = OpteCmd::ListLayers;
        run_cmd_ioctl::<api::ListLayersResp, _>(
            self.device.as_raw_fd(),
            cmd,
            Some(&api::ListLayersReq { port_name: port.to_string() }),
        )
    }

    /// Create a new handle to the OPTE control node.
    pub fn open(what: &str) -> Result<Self, Error> {
        Ok(OpteAdm {
            device: OpenOptions::new().read(true).write(true).open(what)?,
        })
    }

    /// Remove a firewall rule.
    pub fn remove_firewall_rule(
        &self,
        req: &RemFwRuleReq,
    ) -> Result<NoResp, Error> {
        let cmd = OpteCmd::RemFwRule;
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(req))
    }

    /// Return the TCP flows.
    pub fn dump_tcp_flows(
        &self,
        port_name: &str,
    ) -> Result<api::DumpTcpFlowsResp, Error> {
        let cmd = OpteCmd::DumpTcpFlows;
        run_cmd_ioctl::<api::DumpTcpFlowsResp, _>(
            self.device.as_raw_fd(),
            cmd,
            Some(&api::DumpTcpFlowsReq { port_name: port_name.to_string() }),
        )
    }

    /// Clear all entries from the Unified Flow Table (UFT).
    pub fn clear_uft(&self, port_name: &str) -> Result<NoResp, Error> {
        let cmd = OpteCmd::ClearUft;
        run_cmd_ioctl(
            self.device.as_raw_fd(),
            cmd,
            Some(&api::ClearUftReq { port_name: port_name.to_string() }),
        )
    }

    /// Return the Unified Flow Table (UFT).
    pub fn dump_uft(&self, port_name: &str) -> Result<api::DumpUftResp, Error> {
        let cmd = OpteCmd::DumpUft;
        run_cmd_ioctl::<api::DumpUftResp, _>(
            self.device.as_raw_fd(),
            cmd,
            Some(&api::DumpUftReq { port_name: port_name.to_string() }),
        )
    }

    pub fn set_v2p(&self, req: &SetVirt2PhysReq) -> Result<NoResp, Error> {
        let cmd = OpteCmd::SetVirt2Phys;
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))
    }

    /// Dump the Virtual-to-Physical mappings.
    pub fn dump_v2p(&self) -> Result<overlay::DumpVirt2PhysResp, Error> {
        let cmd = OpteCmd::DumpVirt2Phys;
        run_cmd_ioctl(
            self.device.as_raw_fd(),
            cmd,
            Some(&overlay::DumpVirt2PhysReq { unused: 99 }),
        )
    }

    pub fn add_router_entry(
        &self,
        req: &AddRouterEntryReq,
    ) -> Result<NoResp, Error> {
        let cmd = OpteCmd::AddRouterEntry;
        run_cmd_ioctl(self.device.as_raw_fd(), cmd, Some(&req))
    }
}
