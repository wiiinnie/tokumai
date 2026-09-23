// Copyright 2021 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

// JS: I personally don't like this name very much, but could not think of anything better.
// I will gladly take any suggestions on how to rename this.

use crate::error::GatewayClientError;
use crate::GatewayPacketRouter;
use futures::channel::mpsc;
use nym_task::ShutdownToken;

pub type MixnetMessageSender = mpsc::UnboundedSender<Vec<Vec<u8>>>;
pub type MixnetMessageReceiver = mpsc::UnboundedReceiver<Vec<Vec<u8>>>;

pub type AcknowledgementSender = mpsc::UnboundedSender<Vec<Vec<u8>>>;
pub type AcknowledgementReceiver = mpsc::UnboundedReceiver<Vec<Vec<u8>>>;

#[derive(Clone, Debug)]
pub struct PacketRouter {
    ack_sender: AcknowledgementSender,
    mixnet_message_sender: MixnetMessageSender,
    shutdown: ShutdownToken,
}

impl PacketRouter {
    pub fn new(
        ack_sender: AcknowledgementSender,
        mixnet_message_sender: MixnetMessageSender,
        shutdown: ShutdownToken,
    ) -> Self {
        PacketRouter {
            ack_sender,
            mixnet_message_sender,
            shutdown,
        }
    }

    #[allow(clippy::panic)]
    pub fn route_mixnet_messages(
        &self,
        received_messages: Vec<Vec<u8>>,
    ) -> Result<(), GatewayClientError> {
        if let Err(err) = self.mixnet_message_sender.unbounded_send(received_messages) {
            // check if the failure is due to the shutdown being in progress and thus the receiver channel
            // having already been dropped
            if self.shutdown.is_cancelled() {
                // This should ideally not happen, but it's ok
                tracing::warn!("Failed to send mixnet messages due to receiver task shutdown");
                return Err(GatewayClientError::ShutdownInProgress);
            }
            // tokumai: upstream panics here, reasoning that a gone receiver cannot happen
            // "during ordinary operation the way it's currently used". In an enclave with
            // several clients it happens whenever one of them is dropped — a gateway that
            // would not take us, say — and the panic took the whole enclave down with it,
            // book, doors and all. A receiver that is gone means that client is finished,
            // which is what ShutdownInProgress already says.
            tracing::warn!("Failed to send mixnet messages, the receiver is gone: {err}");
            return Err(GatewayClientError::ShutdownInProgress);
        }
        Ok(())
    }

    #[allow(clippy::panic)]
    pub fn route_acks(&self, received_acks: Vec<Vec<u8>>) -> Result<(), GatewayClientError> {
        if let Err(err) = self.ack_sender.unbounded_send(received_acks) {
            // check if the failure is due to the shutdown being in progress and thus the receiver channel
            // having already been dropped
            if self.shutdown.is_cancelled() {
                // This should ideally not happen, but it's ok
                tracing::warn!("Failed to send acks due to receiver task shutdown");
                return Err(GatewayClientError::ShutdownInProgress);
            }
            // tokumai: the same as above — a client on its way out is not a reason to end
            // the process.
            tracing::warn!("Failed to send acks, the receiver is gone: {err}");
            return Err(GatewayClientError::ShutdownInProgress);
        }
        Ok(())
    }
}

impl GatewayPacketRouter for PacketRouter {
    type Error = GatewayClientError;

    // note: this trait tries to decide whether a given message is an ack or a data message

    fn route_mixnet_messages(&self, received_messages: Vec<Vec<u8>>) -> Result<(), Self::Error> {
        self.route_mixnet_messages(received_messages)
    }

    fn route_acks(&self, received_acks: Vec<Vec<u8>>) -> Result<(), Self::Error> {
        self.route_acks(received_acks)
    }
}
