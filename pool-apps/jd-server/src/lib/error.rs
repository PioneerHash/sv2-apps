//! ## Error Module
//!
//! Defines [`JdsError`], the central error enum used throughout the Job Declarator Server (JDS).
//!
//! It unifies errors from:
//! - I/O operations
//! - Channels (send/recv)
//! - SV2 stack: Binary, Codec, Noise, Framing
//! - Mempool layer
//! - Locking logic (PoisonError)
//! - Domain-specific issues (e.g., missing job, invalid URL, reconstruction failures)
//!
//! This module ensures that all errors can be passed around consistently, including across async
//! boundaries.

use std::{
    convert::From,
    fmt::Debug,
    sync::{MutexGuard, PoisonError},
};

use stratum_apps::stratum_core::{
    binary_sv2, codec_sv2, framing_sv2,
    handlers_sv2::HandlerErrorType,
    noise_sv2,
    parsers_sv2::{Mining, ParserError},
};

use crate::mempool::error::JdsMempoolError;

#[derive(std::fmt::Debug)]
pub enum JdsError {
    Io(std::io::Error),
    ChannelSend(Box<dyn std::marker::Send + Debug>),
    ChannelRecv(async_channel::RecvError),
    BinarySv2(binary_sv2::Error),
    Codec(codec_sv2::Error),
    Noise(noise_sv2::Error),
    Framing(framing_sv2::Error),
    PoisonLock(String),
    Custom(String),
    Sv2ProtocolError((u32, Mining<'static>)),
    MempoolError(JdsMempoolError),
    ImpossibleToReconstructBlock(String),
    NoLastDeclaredJob,
    InvalidRPCUrl,
    BadCliArgs,
    /// Unexpected message received
    UnexpectedMessage(u16, u8),
    /// Parser error
    Parser(ParserError),
    /// Transaction decoding error
    TxDecodingError(String),
    /// Missing transactions in JDS
    JDSMissingTransactions,
    /// No valid job available
    NoValidJob,
}

impl std::fmt::Display for JdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use JdsError::*;
        match self {
            Io(ref e) => write!(f, "I/O error: `{e:?}"),
            ChannelSend(ref e) => write!(f, "Channel send failed: `{e:?}`"),
            ChannelRecv(ref e) => write!(f, "Channel recv failed: `{e:?}`"),
            BinarySv2(ref e) => write!(f, "Binary SV2 error: `{e:?}`"),
            Codec(ref e) => write!(f, "Codec SV2 error: `{e:?}"),
            Framing(ref e) => write!(f, "Framing SV2 error: `{e:?}`"),
            Noise(ref e) => write!(f, "Noise SV2 error: `{e:?}"),
            PoisonLock(ref e) => write!(f, "Poison lock: {e:?}"),
            Custom(ref e) => write!(f, "Custom SV2 error: `{e:?}`"),
            Sv2ProtocolError(ref e) => {
                write!(f, "Received Sv2 Protocol Error from upstream: `{e:?}`")
            }
            MempoolError(ref e) => write!(f, "Mempool error: `{e:?}`"),
            ImpossibleToReconstructBlock(e) => {
                write!(f, "Error in reconstructing the block: {e:?}")
            }
            NoLastDeclaredJob => write!(f, "Last declared job not found"),
            InvalidRPCUrl => write!(f, "Invalid Template Provider RPC URL"),
            BadCliArgs => write!(f, "Bad CLI arg input"),
            UnexpectedMessage(ext_type, msg_type) => {
                write!(
                    f,
                    "Unexpected message: ext_type={ext_type}, msg_type={msg_type}"
                )
            }
            Parser(ref e) => write!(f, "Parser error: `{e:?}`"),
            TxDecodingError(ref e) => write!(f, "Transaction decoding error: `{e}`"),
            JDSMissingTransactions => write!(f, "JD server missing transactions"),
            NoValidJob => write!(f, "No valid job available"),
        }
    }
}

impl From<std::io::Error> for JdsError {
    fn from(e: std::io::Error) -> JdsError {
        JdsError::Io(e)
    }
}

impl From<async_channel::RecvError> for JdsError {
    fn from(e: async_channel::RecvError) -> JdsError {
        JdsError::ChannelRecv(e)
    }
}

impl From<binary_sv2::Error> for JdsError {
    fn from(e: binary_sv2::Error) -> JdsError {
        JdsError::BinarySv2(e)
    }
}

impl From<codec_sv2::Error> for JdsError {
    fn from(e: codec_sv2::Error) -> JdsError {
        JdsError::Codec(e)
    }
}

impl From<noise_sv2::Error> for JdsError {
    fn from(e: noise_sv2::Error) -> JdsError {
        JdsError::Noise(e)
    }
}

impl<T: 'static + std::marker::Send + Debug> From<async_channel::SendError<T>> for JdsError {
    fn from(e: async_channel::SendError<T>) -> JdsError {
        JdsError::ChannelSend(Box::new(e))
    }
}

impl From<String> for JdsError {
    fn from(e: String) -> JdsError {
        JdsError::Custom(e)
    }
}

impl From<framing_sv2::Error> for JdsError {
    fn from(e: framing_sv2::Error) -> JdsError {
        JdsError::Framing(e)
    }
}

impl<T> From<PoisonError<MutexGuard<'_, T>>> for JdsError {
    fn from(e: PoisonError<MutexGuard<T>>) -> JdsError {
        JdsError::PoisonLock(e.to_string())
    }
}

impl From<(u32, Mining<'static>)> for JdsError {
    fn from(e: (u32, Mining<'static>)) -> Self {
        JdsError::Sv2ProtocolError(e)
    }
}

impl From<JdsMempoolError> for JdsError {
    fn from(error: JdsMempoolError) -> Self {
        JdsError::MempoolError(error)
    }
}

impl From<ParserError> for JdsError {
    fn from(e: ParserError) -> Self {
        JdsError::Parser(e)
    }
}

/// Implementation of HandlerErrorType for JdsError
/// Required by the new handlers_sv2 API
impl HandlerErrorType for JdsError {
    fn unexpected_message(extension_type: u16, message_type: u8) -> Self {
        JdsError::UnexpectedMessage(extension_type, message_type)
    }

    fn parse_error(error: ParserError) -> Self {
        JdsError::Parser(error)
    }
}
