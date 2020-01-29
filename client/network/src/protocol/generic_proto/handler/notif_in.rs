// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Implementations of the `IntoProtocolsHandler` and `ProtocolsHandler` traits for ingoing
//! substreams a single gossiping protocol.
//!
//! > **Note**: Each instance corresponds to a single protocol. In order to support multiple
//! >			protocols, you need to create multiple instances.
//!

use crate::protocol::generic_proto::upgrade::{NotificationsIn, NotificationsInSubstream};
use bytes::BytesMut;
use futures::prelude::*;
use libp2p::core::{ConnectedPoint, Negotiated, PeerId};
use libp2p::core::upgrade::{DeniedUpgrade, InboundUpgrade, OutboundUpgrade};
use libp2p::swarm::{
	ProtocolsHandler, ProtocolsHandlerEvent,
	IntoProtocolsHandler,
	KeepAlive,
	ProtocolsHandlerUpgrErr,
	SubstreamProtocol,
};
use log::{error, warn};
use smallvec::SmallVec;
use std::{borrow::Cow, fmt, marker::PhantomData, pin::Pin, task::{Context, Poll}};

/// Implements the `IntoProtocolsHandler` trait of libp2p.
///
/// Every time a connection with a remote starts, an instance of this struct is created and
/// sent to a background task dedicated to this connection. Once the connection is established,
/// it is turned into a [`NotifsInHandler`].
pub struct NotifsInHandlerProto<TSubstream> {
	/// Configuration for the protocol upgrade to negotiate.
	in_protocol: NotificationsIn,

	/// Marker to pin the generic type.
	marker: PhantomData<TSubstream>,
}

impl<TSubstream> NotifsInHandlerProto<TSubstream> {
	/// Builds a new `NotifsInHandlerProto`.
	pub fn new(
		proto_name: impl Into<Cow<'static, [u8]>>
	) -> Self {
		NotifsInHandlerProto {
			in_protocol: NotificationsIn::new(proto_name),
			marker: PhantomData,
		}
	}
}

impl<TSubstream> IntoProtocolsHandler for NotifsInHandlerProto<TSubstream>
where
	TSubstream: AsyncRead + AsyncWrite + Unpin + 'static,
{
	type Handler = NotifsInHandler<TSubstream>;

	fn inbound_protocol(&self) -> NotificationsIn {
		self.in_protocol.clone()
	}

	fn into_handler(self, _: &PeerId, _: &ConnectedPoint) -> Self::Handler {
		NotifsInHandler {
			in_protocol: self.in_protocol,
			substream: None,
			pending_accept_refuses: 0,
			events_queue: SmallVec::new(),
		}
	}
}

/// The actual handler once the connection has been established.
pub struct NotifsInHandler<TSubstream> {
	/// Configuration for the protocol upgrade to negotiate for inbound substreams.
	in_protocol: NotificationsIn,

	/// Substream that is open with the remote.
	substream: Option<NotificationsInSubstream<Negotiated<TSubstream>>>,

	/// If the substream is opened and closed rapidly, we can emit several `OpenRequest` messages
	/// without the handler having time to respond with `Accept` or `Refuse`. Every time an
	/// `OpenRequest` is emitted, we increment this variable in order to keep the state consistent.
	pending_accept_refuses: usize,

	/// Queue of events to send to the outside.
	///
	/// This queue must only ever be modified to insert elements at the back, or remove the first
	/// element.
	events_queue: SmallVec<[ProtocolsHandlerEvent<DeniedUpgrade, (), NotifsInHandlerOut, void::Void>; 16]>,
}

/// Event that can be received by a `NotifsInHandler`.
#[derive(Debug)]
pub enum NotifsInHandlerIn {
	/// Can be sent back as a response to an `OpenRequest`. Contains the status message to send
	/// to the remote.
	///
	/// The substream is now considered open, and `Notif` events can be received.
	Accept(Vec<u8>),

	/// Can be sent back as a response to an `OpenRequest`.
	Refuse,
}

/// Event that can be emitted by a `NotifsInHandler`.
#[derive(Debug)]
pub enum NotifsInHandlerOut {
	/// The remote wants to open a substream.
	///
	/// Every time this event is emitted, a corresponding `Accepted` or `Refused` **must** be sent
	/// back.
	OpenRequest,

	/// The notifications substream has been closed by the remote. In order to avoid race
	/// conditions, this does **not** cancel any previously-sent `OpenRequest`.
	Closed,

	/// Received a message on the notifications substream.
	///
	/// Can only happen after an `Accept` and before a `Closed`.
	Notif(BytesMut),
}

impl<TSubstream> NotifsInHandler<TSubstream> {
	/// Returns the name of the protocol that we accept.
	pub fn protocol_name(&self) -> &[u8] {
		self.in_protocol.protocol_name()
	}
}

impl<TSubstream> ProtocolsHandler for NotifsInHandler<TSubstream>
where TSubstream: AsyncRead + AsyncWrite + Unpin + 'static {
	type InEvent = NotifsInHandlerIn;
	type OutEvent = NotifsInHandlerOut;
	type Substream = TSubstream;
	type Error = void::Void;
	type InboundProtocol = NotificationsIn;
	type OutboundProtocol = DeniedUpgrade;
	type OutboundOpenInfo = ();

	fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol> {
		SubstreamProtocol::new(self.in_protocol.clone())
	}

	fn inject_fully_negotiated_inbound(
		&mut self,
		proto: <Self::InboundProtocol as InboundUpgrade<Negotiated<TSubstream>>>::Output
	) {
		if self.substream.is_some() {
			warn!(target: "sub-libp2p", "Received duplicate inbound substream");
			return;
		}

		self.substream = Some(proto);
		self.events_queue.push(ProtocolsHandlerEvent::Custom(NotifsInHandlerOut::OpenRequest));
		self.pending_accept_refuses += 1;
	}

	fn inject_fully_negotiated_outbound(
		&mut self,
		out: <Self::OutboundProtocol as OutboundUpgrade<Negotiated<TSubstream>>>::Output,
		_: Self::OutboundOpenInfo
	) {
		// We never emit any outgoing substream.
		void::unreachable(out)
	}

	fn inject_event(&mut self, message: NotifsInHandlerIn) {
		self.pending_accept_refuses = match self.pending_accept_refuses.checked_sub(1) {
			Some(v) => v,
			None => {
				error!(target: "sub-libp2p", "Inconsistent state: received Accept/Refuse when no \
					pending request exists");
				return;
			}
		};

		// If we send multiple `OpenRequest`s in a row, we will receive back multiple
		// `Accept`/`Refuse` messages. All of them are obsolete except the last one.
		if self.pending_accept_refuses != 0 {
			return;
		}

		match (message, self.substream.as_mut()) {
			(NotifsInHandlerIn::Accept(message), Some(sub)) => sub.send_handshake(message),
			(NotifsInHandlerIn::Accept(_), None) => {},
			(NotifsInHandlerIn::Refuse, _) => self.substream = None,
		}
	}

	fn inject_dial_upgrade_error(&mut self, _: (), err: ProtocolsHandlerUpgrErr<void::Void>) {
		error!(target: "sub-libp2p", "Received dial upgrade error in inbound-only handler");
	}

	fn connection_keep_alive(&self) -> KeepAlive {
		if self.substream.is_some() {
			KeepAlive::Yes
		} else {
			KeepAlive::No
		}
	}

	fn poll(
		&mut self,
		cx: &mut Context,
	) -> Poll<
		ProtocolsHandlerEvent<Self::OutboundProtocol, Self::OutboundOpenInfo, Self::OutEvent, Self::Error>
	> {
		// Flush the events queue if necessary.
		if !self.events_queue.is_empty() {
			let event = self.events_queue.remove(0);
			return Poll::Ready(event)
		}

		match self.substream.as_mut().map(|s| s.poll(cx)) {
			None | Some(Poll::Pending) => {},
			Some(Poll::Ready(Some(msg))) =>
				return Poll::Ready(ProtocolsHandlerEvent::Custom(NotifsInHandlerOut::Notif(msg))),
			Some(Poll::Ready(None)) => {
				self.substream = None;
				return Poll::Ready(ProtocolsHandlerEvent::Custom(NotifsInHandlerOut::Closed));
			},
		}

		Poll::Pending
	}
}

impl<TSubstream> fmt::Debug for NotifsInHandler<TSubstream> {
	fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
		f.debug_struct("NotifsInHandler")
			.finish()
	}
}