use bytesstr::BytesStr;
use ezk_session::{AsyncSdpSession, Codec, Codecs, Options};
use sdp_types::{Direction, MediaType, SessionDescription};
use sip_core::transport::udp::Udp;
use sip_core::{Endpoint, IncomingRequest, Layer, LayerKey, MayTake, Result};
use sip_types::header::typed::{Contact, ContentType};
use sip_types::msg::Line;
use sip_types::uri::sip::SipUri;
use sip_types::uri::NameAddr;
use sip_types::{Code, Method};
use sip_ua::dialog::{Dialog, DialogLayer};
use sip_ua::invite::acceptor::Acceptor;
use sip_ua::invite::session::{Event, Session};
use sip_ua::invite::{create_ack, InviteLayer};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::time::sleep;

/// Custom layer which we use to accept incoming invites
struct InviteAcceptLayer {
    dialog_layer: LayerKey<DialogLayer>,
    invite_layer: LayerKey<InviteLayer>,
}

#[async_trait::async_trait]
impl Layer for InviteAcceptLayer {
    fn name(&self) -> &'static str {
        "invite-accept-layer"
    }

    async fn receive(&self, endpoint: &Endpoint, request: MayTake<'_, IncomingRequest>) {
        let invite = if request.line.method == Method::INVITE {
            request.take()
        } else {
            return;
        };

        let start = Instant::now();
        let ip = local_ip_address::local_ip().unwrap();

        let mut sdp_session = AsyncSdpSession::new(ip, Options::default());
        sdp_session.add_stun_server("15.197.250.192:3478".parse().unwrap());
        sdp_session.add_local_media(
            Codecs::new(MediaType::Audio).with_codec(Codec::PCMA),
            1,
            Direction::RecvOnly,
        );

        let ip = local_ip_address::local_ip().unwrap();
        let contact: SipUri = format!("sip:{ip}:5060").parse().unwrap();
        let contact = Contact::new(NameAddr::uri(contact));

        let dialog = Dialog::new_server(
            endpoint.clone(),
            self.dialog_layer,
            &invite,
            contact.clone(),
        )
        .unwrap();

        let sdp_offer =
            SessionDescription::parse(&BytesStr::from_utf8_bytes(invite.body.clone()).unwrap())
                .unwrap();

        let sdp_response = sdp_session.receive_sdp_offer(sdp_offer).await.unwrap();

        let acceptor = Acceptor::new(dialog, self.invite_layer, invite).unwrap();

        let mut response = acceptor.create_response(Code::OK, None).await.unwrap();
        response.msg.body = sdp_response.to_string().into();
        response
            .msg
            .headers
            .insert("Content-Type", "application/sdp");

        println!("Ready to respond in {:?}", start.elapsed());

        let (mut session, _ack) = acceptor.respond_success(response).await.unwrap();

        loop {
            let e = tokio::select! {
                _ = sdp_session.run() => {continue},
                e = session.drive() => {e}
            };

            match e.unwrap() {
                Event::RefreshNeeded(event) => {
                    event.process_default().await.unwrap();
                }
                Event::ReInviteReceived(event) => {
                    let mut response = endpoint.create_response(&event.invite, Code::OK, None);

                    let offer = SessionDescription::parse(
                        &BytesStr::from_utf8_bytes(event.invite.body.clone()).unwrap(),
                    )
                    .unwrap();

                    let sdp_answer = sdp_session.receive_sdp_offer(offer).await.unwrap();

                    response
                        .msg
                        .headers
                        .insert_named(&ContentType(BytesStr::from_static("application/sdp")));

                    response.msg.body = sdp_answer.to_string().into();
                    event.respond_success(response).await.unwrap();
                }
                Event::Bye(event) => {
                    event.process_default().await.unwrap();
                }
                Event::Terminated => {
                    break;
                }
            }
        }
    }
}

async fn add_video_stream(
    endpoint: &Endpoint,
    contact: &Contact,
    session: &mut Session,
    sdp_session: &mut AsyncSdpSession,
) {
    let v = sdp_session
        .add_local_media(
            Codecs::new(MediaType::Video).with_codec(Codec::AV1),
            1,
            Direction::RecvOnly,
        )
        .unwrap();

    sdp_session.add_media(v, Direction::SendRecv);

    let offer = sdp_session.create_sdp_offer().await.unwrap();

    let mut request = session.dialog.create_request(Method::INVITE);

    request.headers.insert_named(contact);
    request.headers.insert("Content-Type", "application/sdp");
    request.body = offer.to_string().into();

    let mut target_tp_info = session.dialog.target_tp_info.lock().await;

    let mut tsx = endpoint
        .send_invite(request, &mut target_tp_info)
        .await
        .unwrap();

    drop(target_tp_info);

    while let Ok(Some(msg)) = tsx.receive().await {
        if let Ok(ct) = msg.headers.get_named::<ContentType>() {
            if ct.0 == "application/sdp" {
                let sdp_response = BytesStr::from_utf8_bytes(msg.body).unwrap();
                let sdp_response = SessionDescription::parse(&sdp_response).unwrap();
                let mut ack = create_ack(&session.dialog, msg.base_headers.cseq.cseq)
                    .await
                    .unwrap();
                endpoint.send_outgoing_request(&mut ack).await.unwrap();
                sdp_session.receive_sdp_answer(sdp_response).await.unwrap();
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let mut builder = Endpoint::builder();

    let dialog_layer = builder.add_layer(DialogLayer::default());
    let invite_layer = builder.add_layer(InviteLayer::default());

    builder.add_layer(InviteAcceptLayer {
        dialog_layer,
        invite_layer,
    });

    Udp::spawn(&mut builder, "0.0.0.0:5060").await?;
    // builder.add_transport_factory(Arc::new(TcpConnector::new()));

    // Build endpoint to start the SIP Stack
    let endpoint = builder.build();

    let mut out = String::new();
    loop {
        let mut line = [0u8; 1024 * 10];

        let len = tokio::io::stdin().read(&mut line).await.unwrap();

        let read = std::str::from_utf8(&line[..len]).unwrap().trim();

        if read.is_empty() {
            break;
        }

        out += read;
        out += "\n";
    }

    // let mut initiator = Initiator::new(
    //     endpoint.clone(),
    //     dialog_layer,
    //     invite_layer,
    //     NameAddr::uri(SipUri::new(
    //         "10.6.0.3:5066".parse::<SocketAddr>().unwrap().into(),
    //     )),
    //     Contact::new(NameAddr::uri(SipUri::new(
    //         "10.6.0.3:5066".parse::<SocketAddr>().unwrap().into(),
    //     ))),
    //     Box::new(SipUri::new(
    //         "10.6.0.3:5067".parse::<SocketAddr>().unwrap().into(),
    //     )),
    // );

    let mut sess = AsyncSdpSession::new("10.6.0.3".parse().unwrap(), Options::default());
    // sess.add_stun_server("15.197.250.192:3478".parse().unwrap());
    let audio_id = sess
        .add_local_media(
            Codecs::new(MediaType::Audio).with_codec(Codec::PCMA),
            1,
            Direction::RecvOnly,
        )
        .unwrap();
    let _m = sess.add_media(audio_id, Direction::SendRecv);
    // let offer = sess.create_offer().await;
    let answer = sess
        .receive_sdp_offer(SessionDescription::parse(&BytesStr::from(out)).unwrap())
        .await
        .unwrap();

    println!("\n\n\n\n############################\n\n{answer}");

    sess.run().await.unwrap();
    // let mut invite = initiator.create_invite();
    // invite.headers.insert("Content-Type", "application/sdp");
    // invite.body = offer.to_string().into();

    // initiator.send_invite(invite).await.unwrap();
    // while let Ok(response) = initiator.receive().await {
    //     match response {
    //         Response::Provisional(..) => {}
    //         Response::Failure(..) => break,
    //         Response::Early(..) => todo!(),
    //         Response::Session(session, tsx_response) => {
    //             let mut x = create_ack(&session.dialog, tsx_response.base_headers.cseq.cseq)
    //                 .await
    //                 .unwrap();

    //             endpoint.send_outgoing_request(&mut x).await.unwrap();

    //             let answer = SessionDescription::parse(
    //                 &BytesStr::from_utf8_bytes(tsx_response.body).unwrap(),
    //             )
    //             .unwrap();

    //             sess.receive_sdp_answer(answer).await.unwrap();

    //             sess.run().await.unwrap();
    //         }
    //         Response::Finished => break,
    //     };
    // }

    // Busy sleep loop
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}
