use bytesstr::BytesStr;
use ezk::Source;
use ezk_session::{Codec, Codecs};
use sdp_types::{MediaType, SessionDescription};
use sip_core::transport::udp::Udp;
use sip_core::{Endpoint, IncomingRequest, Layer, LayerKey, MayTake, Result};
use sip_types::header::typed::Contact;
use sip_types::uri::sip::SipUri;
use sip_types::uri::NameAddr;
use sip_types::{Code, Method};
use sip_ua::dialog::{Dialog, DialogLayer};
use sip_ua::invite::acceptor::Acceptor;
use sip_ua::invite::session::Event;
use sip_ua::invite::InviteLayer;
use std::time::Duration;
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

        let contact: SipUri = "sip:192.168.178.39:5065".parse().unwrap();
        let contact = Contact::new(NameAddr::uri(contact));

        let dialog =
            Dialog::new_server(endpoint.clone(), self.dialog_layer, &invite, contact).unwrap();

        let sdp_offer =
            SessionDescription::parse(&BytesStr::from_utf8_bytes(invite.body.clone()).unwrap())
                .unwrap();

        let mut sdp_session = ezk_session::Session::new("192.168.178.39".parse().unwrap());
        sdp_session.add_local_media(
            Codecs::new(MediaType::Audio).with_codec(
                Codec {
                    static_pt: Some(8),
                    name: "PCMA".into(),
                    clock_rate: 8000,
                    params: vec![],
                },
                |builder| {
                    builder.add_receiver(|mut s| {
                        println!("got receiver");

                        // tokio::spawn(async move {
                        //     loop {
                        //         dbg!(s.next_event().await);
                        //     }
                        // });
                    });
                },
            ),
            1,
        );
        sdp_session.receiver_offer(sdp_offer).await.unwrap();

        let sdp_response = sdp_session.create_sdp_answer();

        println!("{sdp_response}");

        let acceptor = Acceptor::new(dialog, self.invite_layer, invite).unwrap();

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let mut response = acceptor.create_response(Code::OK, None).await.unwrap();

        response.msg.body = sdp_response.to_string().into();
        response
            .msg
            .headers
            .insert("Content-Type", "application/sdp");

        // Here goes SDP handling

        let (mut session, _ack) = acceptor.respond_success(response).await.unwrap();

        loop {
            match session.drive().await.unwrap() {
                Event::RefreshNeeded(event) => {
                    event.process_default().await.unwrap();
                }
                Event::ReInviteReceived(event) => {
                    let response = endpoint.create_response(&event.invite, Code::OK, None);

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

    Udp::spawn(&mut builder, "0.0.0.0:5065").await?;

    // Build endpoint to start the SIP Stack
    let _endpoint = builder.build();

    // Busy sleep loop
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}
