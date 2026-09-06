//! The behavior half: the state machines, the clock and the socket.
//!
//! The message model ([`structure`](crate::structure),
//! [`messages`](crate::messages)) turns octets into values. This half turns
//! values into a working participant — one that finds peers, matches
//! endpoints, retransmits what was lost and gives up on what expired.
//!
//! # The layering, and the one place `await` appears
//!
//! | Module | What it owns | Async? |
//! |---|---|---|
//! | [`error`] | [`BehaviorError`], the taxonomy for everything below | no |
//! | [`transport`] | the [`DatagramSocket`] seam and its tokio implementation | the trait only |
//! | [`cache`] | [`HistoryCache`]: samples, `HISTORY`, `LIFESPAN` | no |
//! | [`fragment`] | cutting a sample up and putting it back together | no |
//! | [`proxy`] | [`ReaderProxy`] and [`WriterProxy`]: what each side remembers about the other | no |
//! | [`endpoint`] | [`Outbound`], [`Sample`], entity-id allocation | no |
//! | [`writer`] | [`RtpsWriter`]: history, heartbeats, GAPs, retransmission | no |
//! | [`reader`] | [`RtpsReader`]: acceptance, ACKNACKs, reassembly, deadlines | no |
//! | [`liveliness`] | WLP: `ParticipantMessageData` and the lease timers | no |
//! | [`handle`] | the queue an application awaits samples on | yes |
//! | [`participant`] | sockets, the receive loop, the cadence | yes |
//!
//! Only the last two mention a runtime. Everything else is a pure function of
//! (state, input, `now`), which is why the reliability protocol — the part
//! that is genuinely hard — can be tested exhaustively without a port, a
//! sleep or a scheduler.
//!
//! # Where this half meets the other
//!
//! It consumes [`structure`](crate::structure) and
//! [`messages`](crate::messages) and edits neither. [`BehaviorError`] is its
//! own taxonomy and converts from [`RtpsError`](crate::RtpsError) through
//! `#[from]` rather than extending it, so a parse failure surfacing out of
//! the receive loop keeps its precise octet-level diagnosis while gaining the
//! context of which participant dropped it.
//!
//! # Example
//!
//! ```no_run
//! use astrs_rtps::behavior::{Participant, ParticipantConfig};
//! use astrs_rtps::behavior::endpoint::TopicKey;
//! use astrs_rtps::discovery::{SpdpConfig, WriterQos};
//! use astrs_rtps::structure::{GuidPrefix, VendorId};
//!
//! # async fn example() -> Result<(), astrs_rtps::behavior::BehaviorError> {
//! let prefix = GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]);
//! let participant = Participant::new(ParticipantConfig::from_spdp(
//!     SpdpConfig::new(0, 0, prefix)?,
//! ))
//! .await?;
//!
//! let topic = TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_")?;
//! let writer = participant
//!     .create_writer(topic, WriterQos::services_default())
//!     .await?;
//! writer.write(b"hello\0\0\0".to_vec()).await?;
//! # Ok(())
//! # }
//! ```

pub mod cache;
pub mod endpoint;
pub mod error;
pub mod fragment;
pub mod handle;
pub mod liveliness;
pub mod participant;
pub mod proxy;
pub mod reader;
pub(crate) mod state;
pub mod transport;
pub mod writer;

pub use cache::{CacheChange, ChangeKind, HistoryCache, InstanceHandle, Removal};
pub use endpoint::{EntityIdAllocator, Outbound, Sample, TopicKey};
pub use error::{BehaviorError, BehaviorResult, IoFailure, QosPolicyId, ReassemblyDefect};
pub use fragment::{Assembly, FragmentPlan, Reassembler, fragment_sample};
pub use handle::{ReaderHandle, SampleSink};
pub use liveliness::{
    LivelinessTracker, ParticipantMessageData, ParticipantMessageKind, WLP_TOPIC_NAME,
    WLP_TYPE_NAME,
};
pub use participant::{BindPolicy, Participant, ParticipantConfig, WriterHandle};
pub use proxy::{ReaderProxy, WriterProxy};
pub use reader::{DeadlineMiss, ReaderConfig, RtpsReader};
pub use transport::{
    DatagramSocket, MulticastCapability, RtpsSocket, SocketFuture, UdpTransport, probe_multicast,
};
pub use writer::{RtpsWriter, WriterConfig};
