//! What a CLI connection's single writer task can be asked to send.
//!
//! A CLI connection is strictly request/response for [`ControlRequest`]s
//! (blueprint §7.3), but a subscription opened by one of those requests
//! (`LogSubscribe`, `TopicSubscribe`) pushes frames asynchronously,
//! interleaved with any later request's reply on the *same* connection. One
//! [`CliOutbound`] queue, drained by one writer task, is what lets both
//! paths share the connection without two tasks racing to write to it.

use astrs_wire::{ControlReply, DataFrame, LogFrame};

/// One thing queued for a CLI connection's writer task.
#[derive(Debug)]
pub enum CliOutbound {
    /// The answer to a request, sent in request order.
    Reply(ControlReply),
    /// A pushed log record for an active [`astrs_wire::ControlRequest::LogSubscribe`].
    Log(Box<LogFrame>),
    /// A pushed message for an active [`astrs_wire::ControlRequest::TopicSubscribe`].
    Data(Box<DataFrame>),
}

impl From<ControlReply> for CliOutbound {
    fn from(reply: ControlReply) -> Self {
        Self::Reply(reply)
    }
}

impl From<LogFrame> for CliOutbound {
    fn from(frame: LogFrame) -> Self {
        Self::Log(Box::new(frame))
    }
}

impl From<DataFrame> for CliOutbound {
    fn from(frame: DataFrame) -> Self {
        Self::Data(Box::new(frame))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn conversions_tag_the_right_variant() {
        assert!(matches!(
            CliOutbound::from(ControlReply::Ok),
            CliOutbound::Reply(_)
        ));
        let log = LogFrame::new(
            astrs_wire::SubscriptionId::FIRST,
            astrs_wire::LogRecord::new(
                astrs_time::HlcTimestamp::EPOCH,
                astrs_wire::LogLevel::Info,
                "x",
            ),
        );
        assert!(matches!(CliOutbound::from(log), CliOutbound::Log(_)));
    }
}
