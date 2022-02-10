/// <https://datatracker.ietf.org/doc/html/rfc6455#section-5.2>
use bytes::{Buf, BufMut, BytesMut};
use futures_util::{SinkExt, StreamExt};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Decoder, Encoder, Framed};

use std::{mem::take, string::FromUtf8Error};

use crate::{mask, utf8, Error};

const FRAME_SIZE: usize = 4096;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum OpCode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl OpCode {
    fn is_control(self) -> bool {
        return matches!(self, Self::Close | Self::Ping | Self::Pong);
    }
}

impl TryFrom<u8> for OpCode {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Continuation),
            1 => Ok(Self::Text),
            2 => Ok(Self::Binary),
            8 => Ok(Self::Close),
            9 => Ok(Self::Ping),
            10 => Ok(Self::Pong),
            _ => Err(ProtocolError::InvalidOpcode),
        }
    }
}

impl From<OpCode> for u8 {
    fn from(value: OpCode) -> Self {
        match value {
            OpCode::Continuation => 0,
            OpCode::Text => 1,
            OpCode::Binary => 2,
            OpCode::Close => 8,
            OpCode::Ping => 9,
            OpCode::Pong => 10,
        }
    }
}

#[derive(Debug)]
pub struct Frame {
    opcode: OpCode,
    is_final: bool,
    payload: Vec<u8>,
}

#[derive(Debug)]
pub enum ProtocolError {
    InvalidCloseCode,
    InvalidCloseSequence,
    InvalidOpcode,
    InvalidRsv,
    InvalidPayloadLength,
    InvalidUtf8,
    DisallowedOpcode,
    DisallowedCloseCode,
    MessageCannotBeText,
    ServerMaskedData,
    InvalidControlFrameLength,
    FragmentedControlFrame,
    UnexpectedContinuation,
    UnfinishedMessage,
}

impl ProtocolError {
    fn to_close(&self) -> Message {
        match self {
            Self::InvalidUtf8 => Message::Close(
                Some(CloseCode::InvalidFramePayloadData),
                Some(String::from("invalid utf8")),
            ),
            _ => Message::Close(
                Some(CloseCode::ProtocolError),
                Some(String::from("protocol violation")),
            ),
        }
    }
}

impl From<FromUtf8Error> for ProtocolError {
    fn from(_: FromUtf8Error) -> Self {
        Self::InvalidUtf8
    }
}

impl From<std::str::Utf8Error> for ProtocolError {
    fn from(_: std::str::Utf8Error) -> Self {
        Self::InvalidUtf8
    }
}

#[derive(PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

pub struct WebsocketProtocol {
    role: Role,
    payload: Vec<u8>,
    payload_in: usize,
    utf8_valid_up_to: usize,
}

macro_rules! ensure_buffer_has_space {
    ($buf:expr, $space:expr) => {
        if $buf.len() < $space {
            $buf.reserve($space);

            return Ok(None);
        }
    };
}

impl WebsocketProtocol {
    #[must_use]
    pub fn new(role: Role) -> Self {
        Self {
            role,
            payload: Vec::new(),
            payload_in: 0,
            utf8_valid_up_to: 0,
        }
    }
}

impl Decoder for WebsocketProtocol {
    type Item = Frame;
    type Error = Error;

    #[allow(clippy::cast_possible_truncation)]
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Opcode and payload length must be present
        ensure_buffer_has_space!(src, 2);

        let fin_and_rsv = unsafe { src.get_unchecked(0) };
        let payload_len_1 = unsafe { src.get_unchecked(1) };

        // Bit 0
        let fin = fin_and_rsv & 1 << 7 != 0;

        // Bits 1-3
        let rsv = fin_and_rsv & 0x70;

        if rsv != 0 {
            return Err(Error::Protocol(ProtocolError::InvalidRsv));
        }

        // Bits 4-7
        let opcode_value = fin_and_rsv & 31;
        let opcode = OpCode::try_from(opcode_value)?;

        if !fin && opcode.is_control() {
            return Err(Error::Protocol(ProtocolError::FragmentedControlFrame));
        }

        let mask = payload_len_1 >> 7 != 0;

        if mask && self.role == Role::Client {
            return Err(Error::Protocol(ProtocolError::ServerMaskedData));
        }

        // Bits 1-7
        let mut payload_length = (payload_len_1 & 127) as usize;

        let mut offset = 2;

        if payload_length > 125 {
            if opcode.is_control() {
                return Err(Error::Protocol(ProtocolError::InvalidControlFrameLength));
            }

            if payload_length == 126 {
                ensure_buffer_has_space!(src, 4);
                let mut payload_length_bytes = [0; 2];
                payload_length_bytes.copy_from_slice(unsafe { src.get_unchecked(2..4) });
                payload_length = u16::from_be_bytes(payload_length_bytes) as usize;
                offset = 4;
            } else if payload_length == 127 {
                ensure_buffer_has_space!(src, 10);
                let mut payload_length_bytes = [0; 8];
                payload_length_bytes.copy_from_slice(unsafe { src.get_unchecked(2..10) });
                payload_length = u64::from_be_bytes(payload_length_bytes) as usize;
                offset = 10;
            } else {
                return Err(Error::Protocol(ProtocolError::InvalidPayloadLength));
            }
        }

        let mut masking_key = [0; 4];
        if mask {
            ensure_buffer_has_space!(src, offset + 4);
            masking_key.copy_from_slice(unsafe { src.get_unchecked(offset..offset + 4) });
            offset += 4;
        }

        // Reserve space for the incoming payload
        self.payload.resize(payload_length, 0);

        offset += self.payload_in;

        // Get the actual payload, if any
        let data_available = src.len() - offset;
        let data_missing = payload_length - self.payload_in;
        let to_read = data_missing.min(data_available);
        let possible_end_in_payload = self.payload_in + to_read;

        if payload_length > 0 {
            // Copy what we have to the payload body
            unsafe {
                self.payload
                    .get_unchecked_mut(self.payload_in..possible_end_in_payload)
                    .copy_from_slice(src.get_unchecked(offset..offset + to_read));
            };

            // Unmask it if needed
            if mask {
                masking_key.rotate_left(self.payload_in % 4);

                mask::frame(masking_key, unsafe {
                    self.payload
                        .get_unchecked_mut(self.payload_in..possible_end_in_payload)
                });
            }

            self.payload_in = possible_end_in_payload;

            let bytes_missing = payload_length - self.payload_in;

            // If the current payload is incomplete
            if bytes_missing > 0 {
                // Even here, we can fast fail on invalid UTF8
                if opcode == OpCode::Text {
                    let (should_fail, valid_up_to) = utf8::should_fail_fast(
                        unsafe {
                            self.payload
                                .get_unchecked(self.utf8_valid_up_to..self.payload_in)
                        },
                        false,
                    );

                    if should_fail {
                        return Err(Error::Protocol(ProtocolError::InvalidUtf8));
                    }

                    self.utf8_valid_up_to += valid_up_to;
                }

                src.reserve(bytes_missing);

                return Ok(None);
            }

            offset += to_read;

            // Close frames must be at least 2 bytes in length
            if opcode == OpCode::Close && payload_length == 1 {
                return Err(Error::Protocol(ProtocolError::InvalidCloseSequence));
            }
        }

        src.advance(offset);

        let payload = take(&mut self.payload);
        self.payload_in = 0;
        self.utf8_valid_up_to = 0;

        let frame = Frame {
            opcode,
            payload,
            is_final: fin,
        };

        Ok(Some(frame))
    }
}

impl Encoder<Frame> for WebsocketProtocol {
    type Error = Error;

    #[allow(clippy::cast_possible_truncation)]
    fn encode(&mut self, mut item: Frame, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let chunk_size = item.payload.len();
        let masked = self.role == Role::Client;
        let mask_bit = 128 * u8::from(masked);
        let opcode_value: u8 = item.opcode.into();

        let frame = (u8::from(item.is_final) << 7) + opcode_value;

        dst.put_u8(frame);

        if chunk_size > u16::MAX as usize {
            dst.put_u8(127 + mask_bit);
            dst.put_u64(chunk_size as u64);
        } else if chunk_size > 125 {
            dst.put_u8(126 + mask_bit);
            dst.put_u16(chunk_size as u16);
        } else {
            dst.put_u8(chunk_size as u8 + mask_bit);
        }

        if masked {
            let mask = [
                fastrand::u8(0..=255),
                fastrand::u8(0..=255),
                fastrand::u8(0..=255),
                fastrand::u8(0..=255),
            ];

            dst.extend_from_slice(&mask);

            mask::frame(mask, &mut item.payload);
        }

        dst.extend_from_slice(&item.payload);

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum CloseCode {
    NormalClosure,
    GoingAway,
    ProtocolError,
    UnsupportedData,
    Reserved,
    NoStatusReceived,
    AbnormalClosure,
    InvalidFramePayloadData,
    PolicyViolation,
    MessageTooBig,
    MandatoryExtension,
    InternalServerError,
    TlsHandshake,
    ReservedForStandards(u16),
    Libraries(u16),
    Private(u16),
}

impl TryFrom<u16> for CloseCode {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1000 => Ok(Self::NormalClosure),
            1001 => Ok(Self::GoingAway),
            1002 => Ok(Self::ProtocolError),
            1003 => Ok(Self::UnsupportedData),
            1004 => Ok(Self::Reserved),
            1005 => Ok(Self::NoStatusReceived),
            1006 => Ok(Self::AbnormalClosure),
            1007 => Ok(Self::InvalidFramePayloadData),
            1008 => Ok(Self::PolicyViolation),
            1009 => Ok(Self::MessageTooBig),
            1010 => Ok(Self::MandatoryExtension),
            1011 => Ok(Self::InternalServerError),
            1015 => Ok(Self::TlsHandshake),
            1012..=1014 | 1016..=2999 => Ok(Self::ReservedForStandards(value)),
            3000..=3999 => Ok(Self::Libraries(value)),
            4000..=4999 => Ok(Self::Private(value)),
            _ => Err(ProtocolError::InvalidCloseCode),
        }
    }
}

impl From<CloseCode> for u16 {
    fn from(value: CloseCode) -> Self {
        match value {
            CloseCode::NormalClosure => 1000,
            CloseCode::GoingAway => 1001,
            CloseCode::ProtocolError => 1002,
            CloseCode::UnsupportedData => 1003,
            CloseCode::Reserved => 1004,
            CloseCode::NoStatusReceived => 1005,
            CloseCode::AbnormalClosure => 1006,
            CloseCode::InvalidFramePayloadData => 1007,
            CloseCode::PolicyViolation => 1008,
            CloseCode::MessageTooBig => 1009,
            CloseCode::MandatoryExtension => 1010,
            CloseCode::InternalServerError => 1011,
            CloseCode::TlsHandshake => 1015,
            CloseCode::ReservedForStandards(value)
            | CloseCode::Libraries(value)
            | CloseCode::Private(value) => value,
        }
    }
}

impl CloseCode {
    fn is_allowed(&self) -> bool {
        !matches!(
            self,
            Self::Reserved
                | Self::NoStatusReceived
                | Self::AbnormalClosure
                | Self::TlsHandshake
                | Self::ReservedForStandards(_)
        )
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Close(Option<CloseCode>, Option<String>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
}

impl Message {
    fn from_raw(opcode: OpCode, data: Vec<u8>) -> Result<Self, ProtocolError> {
        match opcode {
            OpCode::Continuation => Err(ProtocolError::DisallowedOpcode),
            OpCode::Text => {
                let data = unsafe { String::from_utf8_unchecked(data) };

                Ok(Self::Text(data))
            }
            OpCode::Binary => Ok(Self::Binary(data)),
            OpCode::Close => {
                if data.is_empty() {
                    Ok(Self::Close(None, None))
                } else {
                    let close_code_value = u16::from_be_bytes(data[..2].try_into().unwrap());
                    let close_code = CloseCode::try_from(close_code_value)?;

                    if !close_code.is_allowed() {
                        return Err(ProtocolError::DisallowedCloseCode);
                    }

                    let reason = if data.is_empty() {
                        None
                    } else {
                        Some(utf8::parse(data[2..].to_vec())?)
                    };

                    Ok(Self::Close(Some(close_code), reason))
                }
            }
            OpCode::Ping => Ok(Self::Ping(data)),
            OpCode::Pong => Ok(Self::Pong(data)),
        }
    }

    fn into_raw(self) -> (OpCode, Vec<u8>) {
        match self {
            Self::Text(text) => (OpCode::Text, text.into_bytes()),
            Self::Binary(data) => (OpCode::Binary, data),
            Self::Close(close_code, reason) => {
                if let Some(close_code) = close_code {
                    let reason = reason.unwrap_or_default();
                    let close_code_value: u16 = close_code.into();
                    let mut body = vec![0; 2 + reason.len()];

                    unsafe {
                        body.get_unchecked_mut(0..2)
                            .copy_from_slice(&close_code_value.to_be_bytes());
                        body.get_unchecked_mut(2..)
                            .copy_from_slice(reason.as_bytes());
                    }

                    (OpCode::Close, body)
                } else {
                    (OpCode::Close, Vec::new())
                }
            }
            Self::Ping(data) => (OpCode::Ping, data),
            Self::Pong(data) => (OpCode::Pong, data),
        }
    }

    #[must_use]
    pub fn is_text(&self) -> bool {
        return matches!(self, Self::Text(_));
    }

    #[must_use]
    pub fn is_binary(&self) -> bool {
        return matches!(self, Self::Binary(_));
    }

    #[must_use]
    pub fn is_close(&self) -> bool {
        return matches!(self, Self::Close(_, _));
    }

    #[must_use]
    pub fn is_ping(&self) -> bool {
        return matches!(self, Self::Ping(_));
    }

    #[must_use]
    pub fn is_pong(&self) -> bool {
        return matches!(self, Self::Pong(_));
    }

    pub fn into_text(self) -> Result<String, ProtocolError> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Binary(data) => Ok(utf8::parse(data)?),
            _ => Err(ProtocolError::MessageCannotBeText),
        }
    }
}

#[derive(Debug)]
enum StreamState {
    Active,
    ClosedByPeer,
    ClosedByUs,
    CloseAcknowledged,
    Terminated,
}

impl StreamState {
    fn can_read(&self) -> bool {
        return matches!(self, Self::Active | Self::ClosedByUs);
    }

    fn check_active(&self) -> Result<(), Error> {
        match self {
            Self::Terminated => Err(Error::AlreadyClosed),
            _ => Ok(()),
        }
    }
}

pub struct WebsocketStream<T> {
    protocol: Framed<T, WebsocketProtocol>,
    state: StreamState,

    framing_payload: Vec<u8>,
    framing_opcode: OpCode,
    framing_final: bool,

    utf8_valid_up_to: usize,
}

impl<T> WebsocketStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub fn from_raw_stream(stream: T, role: Role) -> Self {
        let mut framed = WebsocketProtocol::new(role).framed(stream);
        framed.read_buffer_mut().reserve(4 * 1024);

        Self {
            protocol: framed,
            state: StreamState::Active,
            framing_payload: Vec::new(),
            framing_opcode: OpCode::Continuation,
            framing_final: false,
            utf8_valid_up_to: 0,
        }
    }

    pub(crate) fn from_framed<C>(framed: Framed<T, C>, role: Role) -> Self {
        let old_parts = framed.into_parts();
        let mut new_parts = WebsocketProtocol::new(role)
            .framed(old_parts.io)
            .into_parts();
        new_parts.write_buf = old_parts.write_buf;
        new_parts.read_buf = old_parts.read_buf;

        let framed = Framed::from_parts(new_parts);

        Self {
            protocol: framed,
            state: StreamState::Active,
            framing_payload: Vec::new(),
            framing_opcode: OpCode::Continuation,
            framing_final: false,
            utf8_valid_up_to: 0,
        }
    }

    async fn read_full_message(&mut self) -> Option<Result<(OpCode, Vec<u8>), Error>> {
        if let Err(e) = self.state.check_active() {
            return Some(Err(e));
        };

        while !self.framing_final {
            match self.protocol.next().await? {
                Ok(mut frame) => {
                    // Control frames are allowed in between other frames
                    if frame.opcode.is_control() {
                        return Some(Ok((frame.opcode, frame.payload)));
                    }

                    if self.framing_opcode == OpCode::Continuation {
                        if frame.opcode == OpCode::Continuation {
                            return Some(Err(Error::Protocol(
                                ProtocolError::UnexpectedContinuation,
                            )));
                        }

                        self.framing_opcode = frame.opcode;
                    } else if frame.opcode != OpCode::Continuation {
                        return Some(Err(Error::Protocol(ProtocolError::UnfinishedMessage)));
                    }

                    self.framing_final = frame.is_final;
                    self.framing_payload.append(&mut frame.payload);

                    if self.framing_opcode == OpCode::Text {
                        let (should_fail, valid_up_to) = utf8::should_fail_fast(
                            unsafe { self.framing_payload.get_unchecked(self.utf8_valid_up_to..) },
                            self.framing_final,
                        );

                        if should_fail {
                            return Some(Err(Error::Protocol(ProtocolError::InvalidUtf8)));
                        }

                        self.utf8_valid_up_to += valid_up_to;
                    }
                }
                Err(e) => {
                    return Some(Err(e));
                }
            }
        }

        let opcode = self.framing_opcode;
        let payload = take(&mut self.framing_payload);

        self.framing_opcode = OpCode::Continuation;
        self.framing_final = false;
        self.utf8_valid_up_to = 0;

        Some(Ok((opcode, payload)))
    }

    pub async fn read_message(&mut self) -> Option<Result<Message, Error>> {
        let (opcode, payload) = match self.read_full_message().await? {
            Ok((opcode, payload)) => (opcode, payload),
            Err(e) => {
                if let Error::Protocol(protocol) = &e {
                    let close_msg = protocol.to_close();

                    if let Err(e) = self.write_message(close_msg).await {
                        return Some(Err(e));
                    };
                }

                return Some(Err(e));
            }
        };

        let message = match Message::from_raw(opcode, payload) {
            Ok(msg) => msg,
            Err(e) => {
                let close_msg = e.to_close();

                if let Err(e) = self.write_message(close_msg).await {
                    return Some(Err(e));
                };

                return Some(Err(Error::Protocol(e)));
            }
        };

        match &message {
            Message::Close(_, _) => match self.state {
                StreamState::Active => {
                    self.state = StreamState::ClosedByPeer;
                    if let Err(e) = self.write_message(message.clone()).await {
                        return Some(Err(e));
                    };
                }
                StreamState::ClosedByPeer | StreamState::CloseAcknowledged => return None,
                StreamState::ClosedByUs => {
                    self.state = StreamState::CloseAcknowledged;
                }
                StreamState::Terminated => unreachable!(),
            },
            Message::Ping(data) => {
                if let Err(e) = self.write_message(Message::Pong(data.clone())).await {
                    return Some(Err(e));
                };
            }
            _ => {}
        }

        Some(Ok(message))
    }

    pub async fn write_message(&mut self, message: Message) -> Result<(), Error> {
        self.state.check_active()?;

        if message.is_close() {
            self.state = StreamState::ClosedByUs;
        }

        let (opcode, data) = message.into_raw();
        let mut chunks = data.chunks(FRAME_SIZE).peekable();
        let mut next_chunk = Some(chunks.next().unwrap_or_default());
        let mut chunk_number = 0;

        while let Some(chunk) = next_chunk {
            let frame_opcode = if chunk_number == 0 {
                opcode
            } else {
                OpCode::Continuation
            };

            let frame = Frame {
                opcode: frame_opcode,
                is_final: chunks.peek().is_none(),
                payload: chunk.to_vec(),
            };

            self.protocol.send(frame).await?;

            next_chunk = chunks.next();
            chunk_number += 1;
        }

        if self.protocol.codec().role == Role::Server && !self.state.can_read() {
            self.state = StreamState::Terminated;
            Err(Error::ConnectionClosed)
        } else {
            Ok(())
        }
    }

    pub async fn close(
        &mut self,
        close_code: Option<CloseCode>,
        reason: Option<String>,
    ) -> Result<(), Error> {
        self.write_message(Message::Close(close_code, reason)).await
    }
}
