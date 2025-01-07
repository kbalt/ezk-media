use bytesstr::BytesStr;
use ezk_session::{AsyncSdpSession, Codec, Codecs};
use sdp_types::{Direction, MediaType, SessionDescription};
use sip_core::transport::udp::Udp;
use sip_core::{Endpoint, IncomingRequest, Layer, LayerKey, MayTake, Result};
use sip_types::header::typed::{Contact, ContentType};
use sip_types::uri::sip::SipUri;
use sip_types::uri::NameAddr;
use sip_types::{Code, Method, Name};
use sip_ua::dialog::{Dialog, DialogLayer};
use sip_ua::invite::acceptor::Acceptor;
use sip_ua::invite::prack::send_prack;
use sip_ua::invite::session::Event;
use sip_ua::invite::{create_ack, InviteLayer};
use std::time::{Duration, Instant};
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

        let mut sdp_session = AsyncSdpSession::new(ip);
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

        // Here goes SDP handling

        println!("Ready to respond in {:?}", start.elapsed());

        let (mut session, _ack) = acceptor.respond_success(response).await.unwrap();

        {
            sleep(Duration::from_secs(5)).await;
            let v = sdp_session.add_local_media(
                Codecs::new(MediaType::Video).with_codec(Codec::AV1),
                1,
                Direction::RecvOnly,
            );

            sdp_session.add_media(v, Direction::SendRecv);

            let offer = sdp_session.create_offer().await;

            let mut request = session.dialog.create_request(Method::INVITE);

            request.headers.insert_named(&contact);
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
                        println!("GOT RESPONSE");

                        let sdp_response = BytesStr::from_utf8_bytes(msg.body).unwrap();
                        let sdp_response = SessionDescription::parse(&sdp_response).unwrap();

                        let mut ack = create_ack(&session.dialog, msg.base_headers.cseq.cseq)
                            .await
                            .unwrap();

                        println!("CREATED ACK");

                        endpoint.send_outgoing_request(&mut ack).await.unwrap();

                        println!("SENT RESPONSE");

                        sdp_session.receive_sdp_answer(sdp_response).await.unwrap();

                        println!("RECEIVED ANSWER");
                    }
                }
            }
        }

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

    // Build endpoint to start the SIP Stack
    let _endpoint = builder.build();

    // Busy sleep loop
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}
