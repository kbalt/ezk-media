use ezk_ice::{Component, IceAgent, IceConnectionState, IceCredentials, IceEvent, ReceivedPkt};
use std::{cmp::min, mem::take, net::SocketAddr, time::Instant};

fn create_pair() -> (IceAgent, IceAgent) {
    let a = IceCredentials::random();
    let b = IceCredentials::random();

    let a_agent = IceAgent::new_from_answer(a.clone(), b.clone(), true, true);
    let b_agent = IceAgent::new_from_answer(b, a, false, true);

    (a_agent, b_agent)
}

struct Packet {
    data: Vec<u8>,
    source: SocketAddr,
    destination: SocketAddr,
}

#[test]
fn same_network() {
    env_logger::init();
    let (mut a, mut b) = create_pair();

    let a_addr: SocketAddr = "192.168.178.2:5555".parse().unwrap();
    let b_addr: SocketAddr = "192.168.178.3:5555".parse().unwrap();

    a.add_host_addr(Component::Rtp, a_addr);
    b.add_host_addr(Component::Rtp, b_addr);

    for c in a.ice_candidates() {
        b.add_remote_candidate(&c);
    }

    for c in b.ice_candidates() {
        a.add_remote_candidate(&c);
    }

    let mut now = Instant::now();

    while a.connection_state() != IceConnectionState::Connected
        && b.connection_state() != IceConnectionState::Connected
    {
        poll_agent(&mut a, &mut b, a_addr, b_addr, now);

        now += opt_min(a.timeout(now), b.timeout(now)).unwrap();
    }
}

fn poll_agent(
    a: &mut IceAgent,
    b: &mut IceAgent,
    a_addr: SocketAddr,
    b_addr: SocketAddr,
    now: Instant,
) {
    loop {
        let mut a_events = Vec::new();
        let mut b_events = Vec::new();

        a.poll(now, handle_events(a_addr, &mut a_events));
        b.poll(now, handle_events(b_addr, &mut b_events));

        if a_events.is_empty() && b_events.is_empty() {
            return;
        }

        while !a_events.is_empty() || !b_events.is_empty() {
            feed_agent_events(a, a_addr, &mut a_events, &mut b_events);
            feed_agent_events(b, b_addr, &mut b_events, &mut a_events);
        }
    }
}

fn feed_agent_events(
    agent: &mut IceAgent,
    agent_addr: SocketAddr,
    to_peer: &mut Vec<Packet>,
    from_peer: &mut Vec<Packet>,
) {
    for packet in take(from_peer) {
        agent.receive(
            handle_events(agent_addr, to_peer),
            &ReceivedPkt {
                data: packet.data,
                source: packet.source,
                destination: packet.destination,
                component: Component::Rtp,
            },
        );
    }
}

fn handle_events(
    agent_addr: SocketAddr,
    to_peer: &mut Vec<Packet>,
) -> impl FnMut(IceEvent) + use<'_> {
    move |event| {
        if let IceEvent::SendData {
            component,
            data,
            source,
            target,
        } = event
        {
            to_peer.push(Packet {
                data,
                source: agent_addr,
                destination: target,
            })
        };
    }
}

fn opt_min<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (None, None) => None,
        (None, Some(b)) => Some(b),
        (Some(a), None) => Some(a),
        (Some(a), Some(b)) => Some(min(a, b)),
    }
}
