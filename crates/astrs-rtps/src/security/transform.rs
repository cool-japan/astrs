//! [`EndpointSecurity`], [`SecurityContext`], and the two passes that turn a
//! datagram into a protected one and back.
//!
//! # What one pass does
//!
//! ```text
//!  protect                                  unprotect
//!  ───────                                  ─────────
//!  walk the submessages                     walk the submessages
//!   INFO_*, PAD  → copy, remember            SEC_PREFIX  → read the header
//!   entity, no policy → copy                              find the key by id
//!   entity, policy   → SEC_PREFIX                         collect to SEC_POSTFIX
//!                      [SEC_BODY]                         verify / decrypt
//!                      SEC_POSTFIX                        check the counter
//!  split when the budget is reached          everything else → keep
//!  reseed with the INFO_* state              refuse plaintext on a
//!                                            protected endpoint
//! ```
//!
//! # Which endpoint owns a submessage
//!
//! The two directions ask different questions of the same fields, and getting
//! them the wrong way round would key every ACKNACK off the remote writer:
//!
//! | Submessage | Local endpoint when *sending* | when *receiving* |
//! |---|---|---|
//! | `DATA`, `DATA_FRAG`, `HEARTBEAT`, `HEARTBEAT_FRAG`, `GAP` | `writerId` | `readerId` |
//! | `ACKNACK`, `NACK_FRAG` | `readerId` | `writerId` |
//! | `INFO_*`, `PAD` | none — never protected | none |
//!
//! `INFO_DST` and `INFO_TS` stay in the clear on purpose. They are how a
//! receiver decides the message is for it at all, and encrypting them would
//! mean every participant had to attempt every key on every datagram.
//!
//! # Downgrade, in both directions
//!
//! A protected endpoint refuses a plaintext submessage
//! ([`SecurityError::ProtectionMismatch`]), because accepting one would make
//! the protection advisory: an attacker would simply omit it. An unprotected
//! endpoint refuses a protected one for the mirror reason — it has no policy
//! that could have produced it. Both checks run after the tag is verified, on
//! the recovered submessage, so neither can be provoked into doing work.

use std::collections::BTreeMap;

use crate::messages::{HEADER_LEN, Message, Submessage, SubmessageId};
use crate::security::crypto;
use crate::security::error::{SecurityError, SecurityResult, protection_word};
use crate::security::keys::{KeyMaterial, Psk, SessionKey, SessionSender};
use crate::security::kind::{AadBinding, ProtectionKind, TransformationKind};
use crate::security::replay::ReplayWindow;
use crate::security::wire::{
    COMMON_MAC_LEN, CryptoFooter, CryptoHeader, PROTECTION_OVERHEAD, decode_submessage,
    encode_submessage, read_secure_body, secure_body_submessage,
};
use crate::structure::{EntityId, GuidPrefix};

/// One endpoint's security settings.
///
/// The configuration surface the task names: a pre-shared key and a
/// protection level. [`AadBinding`] rides along because it changes what the
/// tag covers and an integrator deserves to see it rather than discover it.
///
/// `Default` is [`EndpointSecurity::none`] — no key, no protection — so an
/// endpoint that says nothing about security behaves exactly as it did before
/// this module existed, down to the octets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EndpointSecurity {
    /// The pre-shared key, or `None` for an unprotected endpoint.
    pub psk: Option<Psk>,
    /// How much protection to apply.
    pub protection: ProtectionKind,
    /// What the tag covers besides the submessage.
    pub aad: AadBinding,
}

impl EndpointSecurity {
    /// No key, no protection: the pre-security behaviour, byte for byte.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            psk: None,
            protection: ProtectionKind::None,
            aad: AadBinding::HeaderBound,
        }
    }

    /// Authenticate this endpoint's submessages under `psk`, leaving them
    /// readable.
    #[must_use]
    pub const fn signed(psk: Psk) -> Self {
        Self {
            psk: Some(psk),
            protection: ProtectionKind::Sign,
            aad: AadBinding::HeaderBound,
        }
    }

    /// Encrypt and authenticate this endpoint's submessages under `psk`.
    #[must_use]
    pub const fn encrypted(psk: Psk) -> Self {
        Self {
            psk: Some(psk),
            protection: ProtectionKind::Encrypt,
            aad: AadBinding::HeaderBound,
        }
    }

    /// Replace the AAD profile.
    #[must_use]
    pub const fn with_aad(mut self, aad: AadBinding) -> Self {
        self.aad = aad;
        self
    }

    /// True when this endpoint transforms anything.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.protection.is_protecting() && self.psk.is_some()
    }

    /// Octets a protected submessage of this endpoint costs beyond the
    /// submessage itself.
    ///
    /// Zero when nothing is protected, so an unprotected writer's
    /// fragmentation threshold is unchanged.
    #[must_use]
    pub const fn overhead(&self) -> usize {
        if self.protection.is_protecting() {
            PROTECTION_OVERHEAD
        } else {
            0
        }
    }

    /// Check that the two halves of the setting agree.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Malformed`] for a protection level with no key, which
    /// is the configuration mistake that would otherwise send everything in
    /// the clear while the operator believed otherwise.
    pub const fn validate(&self) -> SecurityResult<()> {
        match (self.protection.is_protecting(), self.psk.is_some()) {
            (true, false) => Err(SecurityError::Malformed {
                reason: "an endpoint asks for protection but was given no pre-shared key",
            }),
            _ => Ok(()),
        }
    }
}

/// What one local endpoint's policy resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EndpointPolicy {
    key_id: u32,
    kind: TransformationKind,
    aad: AadBinding,
}

/// One receiving session: its replay window and its cached key.
///
/// There is one of these per *sending participant*, not per key — see
/// [`SecurityContext::inbound`].
#[derive(Debug)]
struct Inbound {
    window: ReplayWindow,
    key: SessionKey,
}

/// A submessage recovered from a datagram, and how it was protected.
#[derive(Debug)]
struct Arrival {
    submessage: Submessage<'static>,
    kind: Option<TransformationKind>,
}

/// Every key and policy a participant protects and verifies traffic with.
///
/// Cheap when empty: [`is_active`](Self::is_active) is a `BTreeMap` emptiness
/// check, and the caller is expected to consult it before doing anything
/// else, so a participant with no security configuration never decodes a
/// datagram twice.
#[derive(Debug)]
pub struct SecurityContext {
    policies: BTreeMap<EntityId, EndpointPolicy>,
    keys: BTreeMap<u32, KeyMaterial>,
    senders: BTreeMap<(u32, TransformationKind), SessionSender>,
    /// One replay window per (sending participant, key, session).
    ///
    /// The participant prefix in the key is load-bearing, and leaving it out
    /// is a bug that only shows up with two publishers. A key id is a pure
    /// function of the pre-shared key and the topic, so every publisher on a
    /// topic derives the *same* id, and each starts its counter at one. Keyed
    /// on `(key_id, session_id)` alone, the second publisher's first
    /// submessage collides with the first publisher's and is rejected as a
    /// replay — on `/tf`, `/rosout` and every other multi-publisher ROS 2
    /// topic.
    ///
    /// Per-prefix is also exactly the right granularity, not merely a finer
    /// one: the sending side shares a single [`SessionSender`] per
    /// `(key_id, kind)` across all of a participant's endpoints, so one
    /// participant emits one counter space under a given key. A per-*entity*
    /// window would split that single stream across several windows and
    /// reject the traffic it did not happen to route.
    ///
    /// Entries are only ever created by a datagram that *verified*, so
    /// unauthenticated traffic cannot grow this map — see
    /// [`open`](Self::open), whose vacant arm derives into a local and
    /// inserts only after the tag checks out. What does grow it is legitimate
    /// traffic: one entry per remote participant per session, reclaimed only
    /// by [`forget`](Self::forget). A participant that restarts comes back
    /// with a fresh [`GuidPrefix`] and takes a new entry, so this is a slow
    /// leak over a long-lived deployment rather than a bounded cache —
    /// sixteen octets of window plus a session key each. Ageing them out is
    /// left to a later release.
    inbound: BTreeMap<(GuidPrefix, u32, u32), Inbound>,
    budget: usize,
}

impl SecurityContext {
    /// An empty context that will split protected datagrams at `budget`
    /// octets.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            policies: BTreeMap::new(),
            keys: BTreeMap::new(),
            senders: BTreeMap::new(),
            inbound: BTreeMap::new(),
            budget,
        }
    }

    /// True when at least one endpoint is protected.
    ///
    /// The early-out every call site checks first. When this is false, no
    /// datagram is decoded, re-encoded or copied by this module.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.policies.is_empty()
    }

    /// How many endpoints are protected.
    #[must_use]
    pub fn protected_endpoints(&self) -> usize {
        self.policies.len()
    }

    /// How many (sending participant, key, session) triples this context is
    /// tracking a replay window for.
    ///
    /// Exposed because it is the thing a denial-of-service test has to
    /// measure: a datagram that does not verify must not add one. See the
    /// `inbound` field.
    #[must_use]
    pub fn tracked_sessions(&self) -> usize {
        self.inbound.len()
    }

    /// Octets a protected datagram may occupy.
    #[must_use]
    pub const fn budget(&self) -> usize {
        self.budget
    }

    /// Register (or replace) one local endpoint's policy.
    ///
    /// `context` separates endpoints that share a pre-shared key; pass the
    /// topic and type name. See
    /// [`KeyMaterial::from_psk`](crate::security::KeyMaterial::from_psk).
    ///
    /// A setting with no protection removes any policy the endpoint had,
    /// which is what makes this idempotent under a configuration reload.
    ///
    /// # Errors
    ///
    /// [`EndpointSecurity::validate`]'s error, and whatever key derivation
    /// returns.
    pub fn register(
        &mut self,
        entity: EntityId,
        security: &EndpointSecurity,
        context: &str,
    ) -> SecurityResult<()> {
        security.validate()?;
        let Some(psk) = security.psk.as_ref().filter(|_| security.is_active()) else {
            self.forget(entity);
            return Ok(());
        };
        let material = KeyMaterial::from_psk(psk, context)?;
        let policy = EndpointPolicy {
            key_id: material.key_id(),
            kind: security.protection.transformation(),
            aad: security.aad,
        };
        self.keys.insert(policy.key_id, material);
        self.policies.insert(entity, policy);
        Ok(())
    }

    /// Register key material directly, bypassing the pre-shared key.
    ///
    /// The seam an authenticated key agreement would use, and the seam a test
    /// uses to give two peers the same key id and different keys — the only
    /// way to reach the tag-failure rejection path rather than the cheap
    /// key-id-lookup one.
    pub fn register_material(
        &mut self,
        entity: EntityId,
        material: KeyMaterial,
        kind: TransformationKind,
        aad: AadBinding,
    ) {
        let policy = EndpointPolicy {
            key_id: material.key_id(),
            kind,
            aad,
        };
        self.keys.insert(policy.key_id, material);
        self.policies.insert(entity, policy);
    }

    /// Drop one endpoint's policy, and its key material if nothing else uses
    /// it.
    ///
    /// Returns whether there was a policy to drop.
    pub fn forget(&mut self, entity: EntityId) -> bool {
        let Some(policy) = self.policies.remove(&entity) else {
            return false;
        };
        if !self
            .policies
            .values()
            .any(|held| held.key_id == policy.key_id)
        {
            self.keys.remove(&policy.key_id);
            self.senders
                .retain(|(key_id, _), _| *key_id != policy.key_id);
            self.inbound
                .retain(|(_, key_id, _), _| *key_id != policy.key_id);
        }
        true
    }

    /// Protect a datagram, splitting it when the budget demands.
    ///
    /// Returns the datagrams to send, in order. An inactive context returns
    /// the input unchanged, so a caller that forgets the
    /// [`is_active`](Self::is_active) check is still correct, only slower.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Wire`] when the datagram will not decode or the
    /// result will not encode, [`SecurityError::OverBudget`] when one
    /// protected submessage cannot fit a datagram on its own, and whatever
    /// the AEAD returns.
    pub fn protect_datagram(&mut self, datagram: &[u8]) -> SecurityResult<Vec<Vec<u8>>> {
        if !self.is_active() {
            return Ok(vec![datagram.to_vec()]);
        }
        let message = Message::decode(datagram).map_err(SecurityError::Wire)?;
        let mut out = Vec::new();
        for protected in self.protect(&message)? {
            out.push(protected.encode().map_err(SecurityError::Wire)?);
        }
        Ok(out)
    }

    /// Protect one message, splitting it when the budget demands.
    ///
    /// # Errors
    ///
    /// As [`protect_datagram`](Self::protect_datagram).
    pub fn protect(&mut self, message: &Message<'_>) -> SecurityResult<Vec<Message<'static>>> {
        let mut out: Vec<Message<'static>> = Vec::new();
        let mut current = Message::new(message.header);
        let mut size = HEADER_LEN;
        // The interpreter state a split datagram has to repeat: `INFO_DST`
        // says who the rest is for and `INFO_TS` when it was written, and a
        // continuation that dropped them would deliver samples to the wrong
        // participant with the wrong timestamps.
        let mut interpreters: Vec<Submessage<'static>> = Vec::new();

        for submessage in message.iter() {
            let group = self.group_for(submessage)?;
            let group_len: usize = group.iter().map(Submessage::serialized_len).sum();
            let seed_len: usize = interpreters.iter().map(Submessage::serialized_len).sum();

            if HEADER_LEN + seed_len + group_len > self.budget {
                return Err(SecurityError::OverBudget {
                    len: HEADER_LEN + seed_len + group_len,
                    budget: self.budget,
                });
            }
            if size + group_len > self.budget {
                out.push(current);
                current = Message::new(message.header);
                size = HEADER_LEN;
                for info in &interpreters {
                    size += info.serialized_len();
                    current.push(info.clone());
                }
            }
            for one in group {
                size += one.serialized_len();
                current.push(one);
            }
            if submessage.is_interpreter() {
                remember(&mut interpreters, submessage);
            }
        }

        if !current.is_empty() || out.is_empty() {
            out.push(current);
        }
        Ok(out)
    }

    /// Verify and recover a datagram.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Wire`] when the datagram will not decode, and every
    /// rejection [`SecurityError`] names when a protected submessage does not
    /// verify.
    pub fn unprotect_datagram(&mut self, datagram: &[u8]) -> SecurityResult<Vec<u8>> {
        if !self.is_active() {
            return Ok(datagram.to_vec());
        }
        let message = Message::decode(datagram).map_err(SecurityError::Wire)?;
        self.unprotect(&message)?
            .encode()
            .map_err(SecurityError::Wire)
    }

    /// Verify and recover one message.
    ///
    /// # Errors
    ///
    /// As [`unprotect_datagram`](Self::unprotect_datagram).
    pub fn unprotect(&mut self, message: &Message<'_>) -> SecurityResult<Message<'static>> {
        let mut arrivals: Vec<Arrival> = Vec::new();
        let mut index = 0_usize;
        let submessages = &message.submessages;
        // Which participant sent this. The replay windows are per-sender, so
        // two publishers on one topic do not share a counter space.
        //
        // `INFO_SRC` can change the source prefix mid-message, but only for
        // the submessages that follow it, and this crate never emits one. A
        // peer that does would have its post-`INFO_SRC` submessages counted
        // against the datagram's own prefix, which is conservative: at worst
        // an authentic submessage is rejected, never a forged one accepted.
        let source = message.header.guid_prefix;

        while index < submessages.len() {
            let submessage = &submessages[index];
            if submessage.id() == SubmessageId::SecurePrefix {
                let consumed = self.open_envelope(source, submessages, index, &mut arrivals)?;
                index += consumed;
            } else {
                arrivals.push(Arrival {
                    submessage: submessage.clone().into_owned(),
                    kind: None,
                });
                index += 1;
            }
        }

        let mut out = Message::new(message.header);
        for arrival in arrivals {
            self.check_protection(&arrival)?;
            out.push(arrival.submessage);
        }
        Ok(out)
    }

    /// The submessages one input submessage becomes on the way out.
    fn group_for(
        &mut self,
        submessage: &Submessage<'_>,
    ) -> SecurityResult<Vec<Submessage<'static>>> {
        let Some(policy) = source_entity(submessage).and_then(|entity| self.policies.get(&entity))
        else {
            return Ok(vec![submessage.clone().into_owned()]);
        };
        let policy = *policy;
        self.wrap(submessage, policy)
    }

    /// Build the `SEC_PREFIX` / body / `SEC_POSTFIX` triple for one
    /// submessage.
    fn wrap(
        &mut self,
        submessage: &Submessage<'_>,
        policy: EndpointPolicy,
    ) -> SecurityResult<Vec<Submessage<'static>>> {
        let material = self
            .keys
            .get(&policy.key_id)
            .ok_or(SecurityError::UnknownKeyId {
                key_id: policy.key_id,
            })?;
        let sender = match self.senders.get_mut(&(policy.key_id, policy.kind)) {
            Some(sender) => sender,
            None => {
                let sender = SessionSender::new(material, policy.kind)?;
                self.senders
                    .entry((policy.key_id, policy.kind))
                    .or_insert(sender)
            }
        };
        let (session_id, counter, key) = sender.next(material)?;
        let header = CryptoHeader::new(policy.kind, policy.key_id, session_id, counter);
        let plaintext = encode_submessage(submessage)?;
        let sealed = crypto::protect(
            policy.kind,
            key,
            &header.nonce(),
            &header.to_octets(),
            policy.aad,
            &plaintext,
        )?;

        let mut group = Vec::with_capacity(3);
        group.push(header.to_submessage());
        if policy.kind.is_encrypting() {
            group.push(secure_body_submessage(&sealed.payload)?);
        } else if submessage.is_aligned() {
            // The DDS-Security shape: the submessage travels in the clear
            // between the prefix and the postfix.
            group.push(submessage.clone().into_owned());
        } else {
            // A submessage whose body is not a multiple of four cannot sit in
            // the middle of a datagram (§8.3.3), and a `PAD` cannot fix a
            // one-, two- or three-octet shortfall — its own body would then
            // be unaligned. So an authenticated submessage of that shape
            // travels *inside* a `SEC_BODY`, still in the clear, where the
            // length prefix makes the padding recoverable. Unreachable for a
            // CDR payload, which is always a multiple of four.
            group.push(secure_body_submessage(&plaintext)?);
        }
        group.push(CryptoFooter::new(sealed.tag).to_submessage()?);
        Ok(group)
    }

    /// Verify one `SEC_PREFIX` … `SEC_POSTFIX` envelope, appending what it
    /// recovered. Returns how many submessages it consumed.
    fn open_envelope(
        &mut self,
        source: GuidPrefix,
        submessages: &[Submessage<'_>],
        start: usize,
        out: &mut Vec<Arrival>,
    ) -> SecurityResult<usize> {
        let Submessage::Opaque(prefix) = &submessages[start] else {
            return Err(SecurityError::Malformed {
                reason: "a SEC_PREFIX must arrive as undecoded octets",
            });
        };
        let header = CryptoHeader::from_octets(&prefix.body)?;

        let mut body: Option<Vec<u8>> = None;
        let mut clear: Vec<&Submessage<'_>> = Vec::new();
        let mut footer: Option<CryptoFooter> = None;
        let mut index = start + 1;
        while index < submessages.len() {
            let submessage = &submessages[index];
            match submessage.id() {
                SubmessageId::SecureBody => {
                    let Submessage::Opaque(secure) = submessage else {
                        return Err(SecurityError::Malformed {
                            reason: "a SEC_BODY must arrive as undecoded octets",
                        });
                    };
                    if body.is_some() {
                        return Err(SecurityError::Malformed {
                            reason: "an envelope carries more than one SEC_BODY",
                        });
                    }
                    body = Some(read_secure_body(&secure.body, secure.flags.endianness())?);
                }
                SubmessageId::SecurePostfix => {
                    let Submessage::Opaque(tail) = submessage else {
                        return Err(SecurityError::Malformed {
                            reason: "a SEC_POSTFIX must arrive as undecoded octets",
                        });
                    };
                    footer = Some(CryptoFooter::from_octets(
                        &tail.body,
                        tail.flags.endianness(),
                    )?);
                    index += 1;
                    break;
                }
                SubmessageId::SecurePrefix => {
                    return Err(SecurityError::Malformed {
                        reason: "a SEC_PREFIX opened before the previous envelope closed",
                    });
                }
                _ => clear.push(submessage),
            }
            index += 1;
        }

        let footer = footer.ok_or(SecurityError::Malformed {
            reason: "a SEC_PREFIX was never closed by a SEC_POSTFIX",
        })?;

        let recovered = self.open(source, &header, body.as_deref(), &clear, &footer.common_mac)?;
        for submessage in recovered {
            out.push(Arrival {
                submessage,
                kind: Some(header.kind),
            });
        }
        Ok(index - start)
    }

    /// Verify the tag, check the replay window, and return the submessages.
    fn open(
        &mut self,
        source: GuidPrefix,
        header: &CryptoHeader,
        body: Option<&[u8]>,
        clear: &[&Submessage<'_>],
        tag: &[u8; COMMON_MAC_LEN],
    ) -> SecurityResult<Vec<Submessage<'static>>> {
        let material = self
            .keys
            .get(&header.key_id)
            .ok_or(SecurityError::UnknownKeyId {
                key_id: header.key_id,
            })?;
        let aad = self
            .policies
            .values()
            .find(|policy| policy.key_id == header.key_id)
            .map_or(AadBinding::HeaderBound, |policy| policy.aad);

        // Authenticate first, count second, and — the part that is easy to
        // leave out — authenticate before *allocating*. A crypto header names
        // its key id in the clear, so an attacker who has seen one datagram
        // can name that key while holding none of it, and pair it with any
        // participant prefix and any session id it likes. Deriving a session
        // key into the map before the tag is checked would let each such
        // datagram leave an entry behind, and the prefix and session axes are
        // 2^96 and 2^32 wide.
        //
        // So the vacant arm below derives into a *local*, verifies, and only
        // then inserts: the `?` on a forged datagram drops the `VacantEntry`
        // with nothing stored. The occupied arm is the steady-state path and
        // costs neither a derivation nor a copy.
        let cleartext = match body {
            Some(octets) => octets.to_vec(),
            None => {
                let mut octets = Vec::new();
                for submessage in clear {
                    octets.extend_from_slice(&encode_submessage(submessage)?);
                }
                octets
            }
        };
        let protected = if header.kind.is_encrypting() {
            crypto::Protected::Ciphertext(body.ok_or(SecurityError::Malformed {
                reason: "an encrypting transformation arrived without a SEC_BODY",
            })?)
        } else {
            crypto::Protected::Cleartext(&cleartext)
        };
        // The session key is cached with the replay window, so a long-running
        // session runs HKDF once rather than once per datagram. Keyed by the
        // sending participant as well as the key and session — see the field.
        let session = (source, header.key_id, header.session_id);
        let (inbound, plaintext) = match self.inbound.entry(session) {
            std::collections::btree_map::Entry::Occupied(held) => {
                let held = held.into_mut();
                let plaintext = crypto::unprotect(
                    header.kind,
                    &held.key,
                    &header.nonce(),
                    &header.to_octets(),
                    aad,
                    protected,
                    tag,
                )?;
                (held, plaintext)
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                let key = material.session_key(header.kind, header.session_id)?;
                // Nothing is stored until this succeeds: a forged datagram
                // takes the `?` and drops the `VacantEntry` untouched.
                let plaintext = crypto::unprotect(
                    header.kind,
                    &key,
                    &header.nonce(),
                    &header.to_octets(),
                    aad,
                    protected,
                    tag,
                )?;
                (
                    slot.insert(Inbound {
                        window: ReplayWindow::new(),
                        key,
                    }),
                    plaintext,
                )
            }
        };
        if !inbound.window.accept(header.counter()) {
            return Err(SecurityError::ReplayedCounter {
                counter: header.counter(),
                highest: inbound.window.highest(),
                window: inbound.window.window(),
            });
        }

        if header.kind.is_encrypting() || body.is_some() {
            Ok(vec![decode_submessage(&plaintext)?])
        } else {
            Ok(clear
                .iter()
                .map(|submessage| (*submessage).clone().into_owned())
                .collect())
        }
    }

    /// Refuse a plaintext submessage on a protected endpoint, and a protected
    /// one on an endpoint with no policy.
    fn check_protection(&self, arrival: &Arrival) -> SecurityResult<()> {
        let Some(entity) = destination_entity(&arrival.submessage) else {
            return Ok(());
        };
        if entity == EntityId::UNKNOWN {
            // A submessage addressed to `ENTITYID_UNKNOWN` is for *every*
            // matched reader of the writer, so it names no single local
            // endpoint whose policy could be consulted — and treating that as
            // "nothing to check" is the downgrade this module exists to
            // prevent, reachable by omitting a field rather than omitting the
            // transform. An attacker who cannot forge a tag can still clear
            // `reader_id`.
            //
            // So it is checked against the participant instead of against an
            // endpoint, and fails closed: reaching here at all means at least
            // one endpoint is protected (`unprotect` is only called on an
            // active context), and a broadcast that would reach that endpoint
            // in the clear is refused. A protected one is accepted — it
            // verified under a key this participant holds, which is the whole
            // proof required.
            //
            // The cost is narrow and worth naming: on a participant that
            // mixes protected and unprotected endpoints, a *plaintext*
            // broadcast meant for one of the unprotected ones is refused too,
            // because nothing in the submessage says which reader it was for.
            // This crate never emits `ENTITYID_UNKNOWN` in an entity
            // submessage — every `DATA`, `GAP` and `HEARTBEAT` names the
            // reader its proxy resolved — so only a third-party peer can
            // provoke it, and third-party protected traffic is already out of
            // scope (see the module documentation).
            //
            // The refusal is limited to a submessage a *user* endpoint sent.
            // Metatraffic is never protected — SPDP has to be readable by a
            // peer that has not been configured yet — so refusing a builtin
            // writer's broadcast would break discovery on any participant
            // that happened to have one protected topic, and would contradict
            // the promise [`ParticipantConfig::security`] makes. Since only
            // user endpoints are ever protected, only a user endpoint's
            // plaintext can be a downgrade.
            let from_user_endpoint =
                source_entity(&arrival.submessage).is_some_and(|from| !from.is_builtin());
            return match (from_user_endpoint, self.is_active(), arrival.kind) {
                (true, true, None) => Err(SecurityError::ProtectionMismatch {
                    expected: protection_word(true),
                    found: protection_word(false),
                }),
                _ => Ok(()),
            };
        }
        match (self.policies.get(&entity), arrival.kind) {
            (None, None) => Ok(()),
            (Some(policy), Some(kind)) if policy.kind == kind => Ok(()),
            (Some(policy), Some(kind)) => Err(SecurityError::ProtectionMismatch {
                expected: policy.kind.as_str(),
                found: kind.as_str(),
            }),
            (Some(_), None) => Err(SecurityError::ProtectionMismatch {
                expected: protection_word(true),
                found: protection_word(false),
            }),
            (None, Some(_)) => Err(SecurityError::ProtectionMismatch {
                expected: protection_word(false),
                found: protection_word(true),
            }),
        }
    }
}

/// Replace the remembered interpreter submessage of the same kind.
fn remember(state: &mut Vec<Submessage<'static>>, submessage: &Submessage<'_>) {
    let id = submessage.id();
    state.retain(|held| held.id() != id);
    state.push(submessage.clone().into_owned());
}

/// The local endpoint that *produced* a submessage.
#[must_use]
pub fn source_entity(submessage: &Submessage<'_>) -> Option<EntityId> {
    match submessage {
        Submessage::Data(body) => Some(body.writer_id),
        Submessage::DataFrag(body) => Some(body.writer_id),
        Submessage::Heartbeat(body) => Some(body.writer_id),
        Submessage::HeartbeatFrag(body) => Some(body.writer_id),
        Submessage::Gap(body) => Some(body.writer_id),
        Submessage::AckNack(body) => Some(body.reader_id),
        Submessage::NackFrag(body) => Some(body.reader_id),
        _ => None,
    }
}

/// The local endpoint a submessage is *addressed to*.
#[must_use]
pub fn destination_entity(submessage: &Submessage<'_>) -> Option<EntityId> {
    match submessage {
        Submessage::Data(body) => Some(body.reader_id),
        Submessage::DataFrag(body) => Some(body.reader_id),
        Submessage::Heartbeat(body) => Some(body.reader_id),
        Submessage::HeartbeatFrag(body) => Some(body.reader_id),
        Submessage::Gap(body) => Some(body.reader_id),
        Submessage::AckNack(body) => Some(body.writer_id),
        Submessage::NackFrag(body) => Some(body.writer_id),
        _ => None,
    }
}
