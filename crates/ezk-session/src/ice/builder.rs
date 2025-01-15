use std::{
    collections::VecDeque,
    net::SocketAddr,
    time::{Duration, Instant},
};

use slotmap::SlotMap;
use stun_types::{attributes::Fingerprint, is_stun_message, Class, IsStunMessageInfo, Message};

use crate::{transport::SocketUse, ReceivedPkt};

use super::{
    CandidateKind, Checklist, ChecklistState, IceAgent, IceCredentials, IceEvent, StunConfig,
    StunServerBinding,
};

/// Ice Agent before remote credentials are received, performs stun bindings with configured servers
pub(crate) struct IceAgentBuilder {
    stun_config: StunConfig,
    local_credentials: IceCredentials,
    stun_server: Vec<StunServerBinding>,
    is_controlling: bool,
}

impl IceAgentBuilder {
    pub(crate) fn new(is_controlling: bool) -> Self {
        Self {
            stun_config: StunConfig::new(),
            local_credentials: IceCredentials::random(),
            stun_server: vec![],
            is_controlling,
        }
    }

    pub(crate) fn add_stun_server(&mut self, server: SocketAddr) {
        self.stun_server
            .push(StunServerBinding::new(server, SocketUse::Rtp));

        // TODO: only make this if we're using an rtcp socket
        self.stun_server
            .push(StunServerBinding::new(server, SocketUse::Rtcp));
    }

    pub(crate) fn timeout(&self, now: Instant) -> Option<Duration> {
        self.stun_server.iter().filter_map(|b| b.timeout(now)).min()
    }

    pub(crate) fn poll(&mut self, now: Instant, mut on_event: impl FnMut(IceEvent)) {
        for stun_server in &mut self.stun_server {
            stun_server.poll(now, &self.stun_config, &mut on_event);
        }
    }

    /// Receive a packet, returns if the packet has been handled
    pub(crate) fn receive(&mut self, pkt: &ReceivedPkt) -> bool {
        // Discard non-STUN messages
        if !matches!(is_stun_message(&pkt.data), IsStunMessageInfo::Yes { .. }) {
            return false;
        }

        // TODO: avoid clone here, this should be free
        let mut stun_msg = Message::parse(pkt.data.clone()).unwrap();

        let passed_fingerprint_check = stun_msg
            .attribute::<Fingerprint>()
            .is_some_and(|r| r.is_ok());

        if !passed_fingerprint_check {
            log::debug!(
                "Incoming STUN {:?} failed fingerprint check, discarding",
                stun_msg.class()
            );
            return true;
        }

        if stun_msg.class() != Class::Success {
            // todo; log?
            return false;
        }

        for stun_server in &mut self.stun_server {
            if !stun_server.wants_stun_response(stun_msg.transaction_id()) {
                continue;
            }

            // Discard result here, the discovered addresses will be used when building the ICE agent
            let _ = stun_server.receive_stun_response(&self.stun_config, pkt, stun_msg);
            return true;
        }

        false
    }

    pub(crate) fn build(self, remote_credentials: IceCredentials) -> IceAgent {
        // TODO; remove stun servers if the negotiation for rtcp-mux was a success
        let mut ice_agent = IceAgent {
            stun_config: self.stun_config,
            local_credentials: self.local_credentials,
            remote_credentials,
            local_candidates: SlotMap::with_key(),
            remote_candidates: SlotMap::with_key(),
            checklist: Checklist {
                state: ChecklistState::Running,
                max_pairs: 100,
                pairs: Vec::new(),
                triggered_check_queue: VecDeque::new(),
            },
            stun_server: vec![],
            is_controlling: self.is_controlling,
            control_tie_breaker: rand::random(),
            last_ta_trigger: None,
        };

        for stun_server in &self.stun_server {
            let Some((destination, addr)) = stun_server.discovered_addr() else {
                continue;
            };

            ice_agent.add_local_candidate(
                stun_server.socket(),
                CandidateKind::ServerReflexive,
                destination,
                addr,
            );
        }

        ice_agent.stun_server = self.stun_server;
        ice_agent
    }
}
