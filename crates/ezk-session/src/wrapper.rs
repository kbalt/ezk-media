use sdp_types::SessionDescription;
use std::net::IpAddr;

pub struct SdpSession {
    inner: super::SdpSession,
}

impl SdpSession {
    pub fn new(address: IpAddr) -> Self {
        Self {
            inner: super::SdpSession::new(address),
        }
    }

    pub async fn receive_offer(&mut self, offer: SessionDescription) -> Result<(), super::Error> {
        self.inner.receive_offer(offer)?;

        self.handle_instructions().await
    }

    async fn handle_instructions(&mut self) -> Result<(), super::Error> {
        while let Some(ins) = self.inner.pop_instruction() {
            match ins {
                crate::Instruction::CreateUdpSocketPair { transport_id } => {}
                crate::Instruction::CreateUdpSocket { transport_id } => {}
                crate::Instruction::SendData { socket, data } => {}
                crate::Instruction::ReceiveRTP { packet } => {}
                crate::Instruction::TrackAdded {} => {}
            }
        }

        Ok(())
    }
}
