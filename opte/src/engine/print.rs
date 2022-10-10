// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2022 Oxide Computer Company

//! Print comannd responses in human-friendly manner.
//!
//! This is mostly just a place to hang printing routines so that they
//! can be used by both opteadm and integration tests.

use super::flow_table::FlowEntryDump;
use super::ioctl::DumpLayerResp;
use super::ioctl::DumpUftResp;
use super::ioctl::ListLayersResp;
use super::packet::InnerFlowId;
use opte::engine::rule::RuleDump;
use std::string::String;
use std::string::ToString;
use std::vec::Vec;

/// Print a [`DumpLayerResp`].
pub fn print_layer(resp: &DumpLayerResp) {
    println!("Layer {}", resp.name);
    print_hrb();
    println!("Inbound Flows");
    print_hr();
    print_flow_header();
    for (flow_id, flow_state) in &resp.ft_in {
        print_flow(flow_id, flow_state);
    }

    println!("\nOutbound Flows");
    print_hr();
    print_flow_header();
    for (flow_id, flow_state) in &resp.ft_out {
        print_flow(flow_id, flow_state);
    }

    println!("\nInbound Rules");
    print_hr();
    print_rule_header();
    for (id, rule) in &resp.rules_in {
        print_rule(*id, rule);
    }

    println!("\nOutbound Rules");
    print_hr();
    print_rule_header();
    for (id, rule) in &resp.rules_out {
        print_rule(*id, rule);
    }

    println!("");
}

/// Print a [`ListLayersResp`].
pub fn print_list_layers(resp: &ListLayersResp) {
    println!(
        "{:<12} {:<10} {:<10} {:<10}",
        "NAME", "RULES IN", "RULES OUT", "FLOWS",
    );

    for desc in &resp.layers {
        println!(
            "{:<12} {:<10} {:<10} {:<10}",
            desc.name, desc.rules_in, desc.rules_out, desc.flows,
        );
    }
}

/// Print a [`DumpUftResp`].
pub fn print_uft(uft: &DumpUftResp) {
    println!("UFT Inbound: {}/{}", uft.in_num_flows, uft.in_limit);
    print_hr();
    print_flow_header();
    for (flow_id, flow_state) in &uft.in_flows {
        print_flow(flow_id, flow_state);
    }

    println!("");
    println!("UFT Outbound: {}/{}", uft.out_num_flows, uft.out_limit);
    print_hr();
    print_flow_header();
    for (flow_id, flow_state) in &uft.out_flows {
        print_flow(flow_id, flow_state);
    }
}

/// Print the header for the [`print_rule()`] output.
pub fn print_rule_header() {
    println!("{:<8} {:<6} {:<48} {:<18}", "ID", "PRI", "PREDICATES", "ACTION");
}

/// Print a [`RuleDump`].
pub fn print_rule(id: u64, rule: &RuleDump) {
    let hdr_preds = rule
        .predicates
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<String>>()
        .join(" ");

    let data_preds = rule
        .data_predicates
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<String>>()
        .join(" ");

    let mut preds = format!("{} {}", hdr_preds, data_preds);

    if preds == "" {
        preds = "*".to_string();
    }

    println!("{:<8} {:<6} {:<48} {:<?}", id, rule.priority, preds, rule.action);
}

/// Print the header for the [`print_flow()`] output.
pub fn print_flow_header() {
    println!(
        "{:<6} {:<16} {:<6} {:<16} {:<6} {:<8} {:<22}",
        "PROTO", "SRC IP", "SPORT", "DST IP", "DPORT", "HITS", "ACTION"
    );
}

/// Print information about a flow.
pub fn print_flow(flow_id: &InnerFlowId, flow_entry: &FlowEntryDump) {
    // For those types with custom Display implementations
    // we need to first format in into a String before
    // passing it to println in order for the format
    // specification to be honored.
    println!(
        "{:<6} {:<16} {:<6} {:<16} {:<6} {:<8} {:<22}",
        flow_id.proto.to_string(),
        flow_id.src_ip.to_string(),
        flow_id.src_port,
        flow_id.dst_ip.to_string(),
        flow_id.dst_port,
        flow_entry.hits,
        flow_entry.state_summary,
    );
}

/// Print horizontal rule in bold.
pub fn print_hrb() {
    println!("{:=<70}", "=");
}

/// Print horizontal rule.
pub fn print_hr() {
    println!("{:-<70}", "-");
}
