use bytesstr::BytesStr;
use ezk_session::{Codec, Codecs};
use sdp_types::{MediaType, SessionDescription};
use sip_core::transport::udp::Udp;
use sip_core::{Endpoint, IncomingRequest, Layer, LayerKey, MayTake, Result};
use sip_types::header::typed::{Contact, ContentType};
use sip_types::uri::sip::SipUri;
use sip_types::uri::NameAddr;
use sip_types::{Code, Method};
use sip_ua::dialog::{Dialog, DialogLayer};
use sip_ua::invite::acceptor::Acceptor;
use sip_ua::invite::session::Event;
use sip_ua::invite::InviteLayer;
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

        let mut sdp_session = ezk_session::SdpSession::new(ip);
        sdp_session.add_local_media(
            Codecs::new(MediaType::Audio).with_codec(Codec::PCMA, |builder| {
                builder.add_receiver(|mut s| {
                    println!("got receiver pcma");

                    tokio::spawn(async move {
                        while s.recv().await.is_some() {}

                        println!("break pcma");
                    });
                });
            }),
            1,
        );

        let ip = local_ip_address::local_ip().unwrap();
        let contact: SipUri = format!("sip:{ip}:5060").parse().unwrap();
        let contact = Contact::new(NameAddr::uri(contact));

        let dialog =
            Dialog::new_server(endpoint.clone(), self.dialog_layer, &invite, contact).unwrap();

        let sdp_offer =
            SessionDescription::parse(&BytesStr::from_utf8_bytes(invite.body.clone()).unwrap())
                .unwrap();

        sdp_session.receiver_offer(sdp_offer).await.unwrap();

        let sdp_response = sdp_session.create_sdp_answer();
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

        sdp_session.add_local_media(
            Codecs::new(MediaType::Video).with_codec(Codec::AV1, |builder| {
                builder.add_receiver(|mut s| {
                    println!("got receiver av1");

                    tokio::spawn(async move {
                        while s.recv().await.is_some() {}

                        println!("break av1");
                    });
                });
            }),
            1,
        );

        loop {
            match session.drive().await.unwrap() {
                Event::RefreshNeeded(event) => {
                    event.process_default().await.unwrap();
                }
                Event::ReInviteReceived(event) => {
                    let mut response = endpoint.create_response(&event.invite, Code::OK, None);

                    let offer = SessionDescription::parse(
                        &BytesStr::from_utf8_bytes(event.invite.body.clone()).unwrap(),
                    )
                    .unwrap();

                    sdp_session.receiver_offer(offer).await.unwrap();

                    response
                        .msg
                        .headers
                        .insert_named(&ContentType(BytesStr::from_static("application/sdp")));

                    response.msg.body = sdp_session.create_sdp_answer().to_string().into();
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
