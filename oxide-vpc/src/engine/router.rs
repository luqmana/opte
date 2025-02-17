// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

//! The Oxide Network VPC Router.
//!
//! This implements both the Oxide Network VPC "System Router" and
//! "Custom Router" abstractions, as described in RFD 21 §2.3.
use core::fmt;

cfg_if! {
    if #[cfg(all(not(feature = "std"), not(test)))] {
        use alloc::string::{String, ToString};
        use alloc::sync::Arc;
        use alloc::vec::Vec;
    } else {
        use std::string::{String, ToString};
        use std::sync::Arc;
        use std::vec::Vec;
    }
}

use super::firewall as fw;
use super::VpcNetwork;
use crate::api::DelRouterEntryResp;
use crate::api::RouterTarget;
use crate::api::VpcCfg;
use opte::api::Direction;
use opte::api::Ipv4Addr;
use opte::api::Ipv4Cidr;
use opte::api::Ipv6Addr;
use opte::api::Ipv6Cidr;
use opte::api::NoResp;
use opte::api::OpteError;
use opte::engine::headers::IpAddr;
use opte::engine::headers::IpCidr;
use opte::engine::layer::DefaultAction;
use opte::engine::layer::Layer;
use opte::engine::layer::LayerActions;
use opte::engine::packet::InnerFlowId;
use opte::engine::port::meta::ActionMeta;
use opte::engine::port::meta::ActionMetaValue;
use opte::engine::port::Port;
use opte::engine::port::PortBuilder;
use opte::engine::port::Pos;
use opte::engine::predicate::DataPredicate;
use opte::engine::predicate::Ipv4AddrMatch;
use opte::engine::predicate::Ipv6AddrMatch;
use opte::engine::predicate::Predicate;
use opte::engine::rule::Action;
use opte::engine::rule::AllowOrDeny;
use opte::engine::rule::Finalized;
use opte::engine::rule::MetaAction;
use opte::engine::rule::ModMetaResult;
use opte::engine::rule::Rule;

pub const ROUTER_LAYER_NAME: &'static str = "router";

// The control plane wants to define "no destination" as a router
// target. This routing layer implementation converts said target to a
// `Rule` paired with `Action::Deny`. The MetaAction wants an internal
// version of the router target without the "drop" target to match the
// remaining possible targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouterTargetInternal {
    InternetGateway,
    Ip(IpAddr),
    VpcSubnet(IpCidr),
}

impl ActionMetaValue for RouterTargetInternal {
    const KEY: &'static str = "router-target";

    fn from_meta(s: &str) -> Result<Self, String> {
        match s {
            "ig" => Ok(Self::InternetGateway),

            _ => match s.split_once("=") {
                Some(("ip4", ip4_s)) => {
                    let ip4 = ip4_s.parse::<Ipv4Addr>()?;
                    Ok(Self::Ip(IpAddr::Ip4(ip4)))
                }

                Some(("ip6", ip6_s)) => {
                    let ip6 = ip6_s.parse::<Ipv6Addr>()?;
                    Ok(Self::Ip(IpAddr::Ip6(ip6)))
                }

                Some(("sub4", cidr4_s)) => {
                    let cidr4 = cidr4_s.parse::<Ipv4Cidr>()?;
                    Ok(Self::VpcSubnet(IpCidr::Ip4(cidr4)))
                }

                Some(("sub6", cidr6_s)) => {
                    let cidr6 = cidr6_s.parse::<Ipv6Cidr>()?;
                    Ok(Self::VpcSubnet(IpCidr::Ip6(cidr6)))
                }

                _ => Err(format!("bad router target: {}", s)),
            },
        }
    }

    fn as_meta(&self) -> String {
        match self {
            Self::InternetGateway => "ig".to_string(),
            Self::Ip(IpAddr::Ip4(ip4)) => format!("ip4={}", ip4),
            Self::Ip(IpAddr::Ip6(ip6)) => format!("ip6={}", ip6),
            Self::VpcSubnet(IpCidr::Ip4(cidr4)) => format!("sub4={}", cidr4),
            Self::VpcSubnet(IpCidr::Ip6(cidr6)) => format!("sub6={}", cidr6),
        }
    }
}

impl fmt::Display for RouterTargetInternal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            Self::InternetGateway => "IG".to_string(),
            Self::Ip(addr) => format!("IP: {}", addr),
            Self::VpcSubnet(sub) => format!("Subnet: {}", sub),
        };
        write!(f, "{}", s)
    }
}

// Return the priority for a given IP subnet. The priority is based on
// the subnet's prefix length. Specifically, it is given the following
// value.
//
// ```
// priroity = max_prefix_len - prefix len + 10
// ```
//
// `max_prefix_len` is the maximum prefix length for a given IP
// CIDR type: `32` for IPv4, `128` for IPv6.
//
// `prefix_len` comes from the passed in `cidr` argument.
//
// The constant `10` displaces these rules so they start at a priority
// of `10`. This allows placing higher priority rules (lower number)
// to override them, if needed.
//
// # IPv4
//
// ```
// |Prefix Len |Priority            |
// |-----------|--------------------|
// |32         |10 = 32 - 32  10    |
// |31         |11 = 32 - 31  10    |
// |30         |12 = 32 - 30  10    |
// |...        |...                 |
// |0          |42 = 32 - 0  10     |
// ```
//
// # IPv6
//
// ```
// |Prefix Len |Priority            |
// |-----------|--------------------|
// |128        |10 = 128 - 128  10  |
// |127        |11 = 128 - 127  10  |
// |126        |12 = 128 - 126  10  |
// |...        |...                 |
// |0          |138 = 128 - 0  10   |
// ```
fn prefix_len_to_priority(cidr: &IpCidr) -> u16 {
    use opte::api::ip::IpCidr::*;
    use opte::api::ip::Ipv4PrefixLen;
    use opte::api::ip::Ipv6PrefixLen;
    let (max_prefix_len, prefix_len) = match cidr {
        Ip4(ipv4) => (Ipv4PrefixLen::NETMASK_ALL.val(), ipv4.prefix_len()),
        Ip6(ipv6) => (Ipv6PrefixLen::NETMASK_ALL.val(), ipv6.prefix_len()),
    };
    (max_prefix_len - prefix_len) as u16 + 10
}

pub fn setup(
    pb: &PortBuilder,
    _cfg: &VpcCfg,
    ft_limit: core::num::NonZeroU32,
) -> Result<(), OpteError> {
    // Inbound: The router assumes that if the packet made it here,
    // then it had a route to get here.
    //
    // Outbound: If there is no matching route, then the packet should
    // make it no further.
    let actions = LayerActions {
        actions: vec![],
        default_in: DefaultAction::Allow,
        default_out: DefaultAction::Deny,
    };

    let layer = Layer::new(ROUTER_LAYER_NAME, pb.name(), actions, ft_limit);
    pb.add_layer(layer, Pos::After(fw::FW_LAYER_NAME))
}

fn valid_router_dest_target_pair(dest: &IpCidr, target: &RouterTarget) -> bool {
    matches!(
        (&dest, &target),
        // Anything can be dropped
        (_, RouterTarget::Drop) |
        // IPv4 destination, IPv4 address
        (IpCidr::Ip4(_), RouterTarget::Ip(IpAddr::Ip4(_))) |
        // IPv4 destination, IPv4 subnet
        (IpCidr::Ip4(_), RouterTarget::VpcSubnet(IpCidr::Ip4(_))) |
        // IPv6 destination, IPv6 address
        (IpCidr::Ip6(_), RouterTarget::Ip(IpAddr::Ip6(_))) |
        // IPv6 destination, IPv6 subnet
        (IpCidr::Ip6(_), RouterTarget::VpcSubnet(IpCidr::Ip6(_)))
    ) ||
    // Only the default IP addresses are currently allowed to be directed to
    // the gateway
    (matches!(target, RouterTarget::InternetGateway) && dest.is_default())
}

fn make_rule(
    dest: IpCidr,
    target: RouterTarget,
) -> Result<Rule<Finalized>, OpteError> {
    if !valid_router_dest_target_pair(&dest, &target) {
        return Err(OpteError::InvalidRouterEntry {
            dest,
            target: target.to_string(),
        });
    }

    let (predicate, action) = match target {
        RouterTarget::Drop => {
            let predicate = match dest {
                IpCidr::Ip4(ip4) => {
                    Predicate::InnerDstIp4(vec![Ipv4AddrMatch::Prefix(ip4)])
                }

                IpCidr::Ip6(ip6) => {
                    Predicate::InnerDstIp6(vec![Ipv6AddrMatch::Prefix(ip6)])
                }
            };
            (predicate, Action::Deny)
        }

        RouterTarget::InternetGateway => {
            let predicate = match dest {
                IpCidr::Ip4(ip4) => {
                    Predicate::InnerDstIp4(vec![Ipv4AddrMatch::Prefix(ip4)])
                }

                IpCidr::Ip6(ip6) => {
                    Predicate::InnerDstIp6(vec![Ipv6AddrMatch::Prefix(ip6)])
                }
            };
            let action = Action::Meta(Arc::new(RouterAction::new(
                RouterTargetInternal::InternetGateway,
            )));
            (predicate, action)
        }

        RouterTarget::Ip(ip) => {
            let predicate = match dest {
                IpCidr::Ip4(ip4) => {
                    Predicate::InnerDstIp4(vec![Ipv4AddrMatch::Prefix(ip4)])
                }

                IpCidr::Ip6(ip6) => {
                    Predicate::InnerDstIp6(vec![Ipv6AddrMatch::Prefix(ip6)])
                }
            };
            let action = Action::Meta(Arc::new(RouterAction::new(
                RouterTargetInternal::Ip(ip),
            )));
            (predicate, action)
        }

        RouterTarget::VpcSubnet(vpc) => {
            let predicate = match dest {
                IpCidr::Ip4(ip4) => {
                    Predicate::InnerDstIp4(vec![Ipv4AddrMatch::Prefix(ip4)])
                }

                IpCidr::Ip6(ip6) => {
                    Predicate::InnerDstIp6(vec![Ipv6AddrMatch::Prefix(ip6)])
                }
            };
            let action = Action::Meta(Arc::new(RouterAction::new(
                RouterTargetInternal::VpcSubnet(vpc),
            )));
            (predicate, action)
        }
    };

    let priority = prefix_len_to_priority(&dest);
    let mut rule = Rule::new(priority, action);
    rule.add_predicate(predicate);
    Ok(rule.finalize())
}

/// Delete a router entry.
///
/// For the entry to be deleted it must match exactly for the
/// destination [`IpCidr`] as well as its paired [`RouterTarget`].
pub fn del_entry(
    port: &Port<VpcNetwork>,
    dest: IpCidr,
    target: RouterTarget,
) -> Result<DelRouterEntryResp, OpteError> {
    let rule = make_rule(dest, target)?;
    let maybe_id = port.find_rule(ROUTER_LAYER_NAME, Direction::Out, &rule)?;
    match maybe_id {
        Some(id) => {
            port.remove_rule(ROUTER_LAYER_NAME, Direction::Out, id)?;
            Ok(DelRouterEntryResp::Ok)
        }

        None => Ok(DelRouterEntryResp::NotFound),
    }
}

/// Add a router entry.
///
/// Route the [`IpCidr`] to the specified [`RouterTarget`].
pub fn add_entry(
    port: &Port<VpcNetwork>,
    dest: IpCidr,
    target: RouterTarget,
) -> Result<NoResp, OpteError> {
    let rule = make_rule(dest, target)?;
    port.add_rule(ROUTER_LAYER_NAME, Direction::Out, rule)?;
    Ok(NoResp::default())
}

/// Replace the current set of router entries with the set passed in.
pub fn replace(
    port: &Port<VpcNetwork>,
    entries: Vec<(IpCidr, RouterTarget)>,
) -> Result<NoResp, OpteError> {
    let mut out_rules = Vec::with_capacity(entries.len());
    for (cidr, target) in entries {
        out_rules.push(make_rule(cidr, target)?);
    }

    port.set_rules(ROUTER_LAYER_NAME, vec![], out_rules)?;
    Ok(NoResp::default())
}

// TODO For each router table entry we should mark whether it came
// from system or custom.
//
// TODO I may want to have different types of rule/flow tables a layer
// can have. Up to this point the tables consist of `Rule` entires;
// matching arbitrary header predicates to a `RuleAction`. I may want
// to also have more switch-like MATs which match one specific header
// field to an action. For example a table which matches
// longest-prefix-match of the packet's IP destination.
//
// VFP §5.4 ("Groups") talks about using longest prefix match for
// Layer Groups (I still haven't implemented groups).
//
// VFP §6.5 ("Packet Classification"), talks about the ability for
// each condition type to use 1 of 4 different types of classifiers.
pub struct RouterAction {
    // system_table: RouterTable,
    // subnet_table: Option<RouterTable>,
    target: RouterTargetInternal,
}

impl RouterAction {
    fn new(target: RouterTargetInternal) -> Self {
        Self { target }
    }
}

impl fmt::Display for RouterAction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Target = {}", self.target)
    }
}

impl MetaAction for RouterAction {
    fn implicit_preds(&self) -> (Vec<Predicate>, Vec<DataPredicate>) {
        (vec![], vec![])
    }

    fn mod_meta(
        &self,
        _flow_id: &InnerFlowId,
        meta: &mut ActionMeta,
    ) -> ModMetaResult {
        // No target entry should currently exist in the metadata; it
        // would be a bug. However, because of the dynamic nature of
        // metadata we don't have an easy way to enforce this
        // constraint in the type system.
        meta.insert(self.target.key(), self.target.as_meta());
        Ok(AllowOrDeny::Allow(()))
    }
}
