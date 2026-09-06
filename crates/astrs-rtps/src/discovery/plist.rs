//! Bounded readers for the parameter lists discovery samples are made of.
//!
//! Every discovery sample — SPDP participant data, SEDP publications and
//! subscriptions, the WLP participant message — is a `PL_CDR` parameter list,
//! and every one of them faces the same four questions: is this parameter
//! present, does its value decode, is there more than one of it, and is it
//! longer than anything sane. This module answers them once.
//!
//! The bounds are the point. A `PID_TOPIC_NAME` of four megabytes, or two
//! hundred `PID_UNICAST_LOCATOR` entries, is a datagram a hostile peer sends
//! to make a subscriber allocate. `astrs-cdr` already refuses lengths that
//! exceed the buffer; these helpers add the *semantic* ceilings on top, so a
//! well-formed but absurd sample is rejected with a diagnosis rather than
//! stored.
//!
//! Everything here takes `context` — `"SPDP participant data"`, `"SEDP
//! publication"` — so the error a caller sees names the sample as well as
//! the parameter.

use astrs_cdr::{CdrDeserialize, Encoding, ParameterList};

use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::structure::Locator;

/// Most locators of one kind a single discovery sample may carry.
///
/// Sixteen is generous: a participant with sixteen distinct unicast addresses
/// for one purpose is already unusual, and the bound is per parameter id, so
/// metatraffic and user locators are counted separately.
pub const MAX_ANNOUNCED_LOCATORS: usize = 16;

/// Longest topic or type name this decoder accepts.
///
/// DDS does not bound them; every real stack does. 256 octets holds the
/// longest name in `common_interfaces` several times over.
pub const MAX_NAME_LEN: usize = 256;

/// Decode the first parameter with base id `base`, if it is present.
///
/// # Errors
///
/// [`BehaviorError::MalformedParameter`] when the parameter is there but its
/// value does not decode as `T`.
pub fn decode_one<'de, T>(
    list: &'de ParameterList<'de>,
    base: u16,
    encoding: Encoding,
    context: &'static str,
) -> BehaviorResult<Option<T>>
where
    T: CdrDeserialize<'de>,
{
    match list.get_by_base(base) {
        None => Ok(None),
        Some(parameter) => parameter.decode_value(encoding).map(Some).map_err(|_| {
            BehaviorError::MalformedParameter {
                context,
                pid: base,
                reason: "value does not decode at the declared length",
            }
        }),
    }
}

/// Decode the first parameter with base id `base`, or fail when it is absent.
///
/// # Errors
///
/// [`BehaviorError::MissingParameter`] when absent, or
/// [`BehaviorError::MalformedParameter`] when present and undecodable.
pub fn decode_required<'de, T>(
    list: &'de ParameterList<'de>,
    base: u16,
    encoding: Encoding,
    context: &'static str,
) -> BehaviorResult<T>
where
    T: CdrDeserialize<'de>,
{
    decode_one(list, base, encoding, context)?
        .ok_or(BehaviorError::MissingParameter { context, pid: base })
}

/// Decode every repetition of a locator parameter, refusing an absurd count.
///
/// # Errors
///
/// [`BehaviorError::MalformedParameter`] when a value is not a locator, or
/// when there are more than [`MAX_ANNOUNCED_LOCATORS`] of them.
pub fn decode_locators(
    list: &ParameterList<'_>,
    base: u16,
    encoding: Encoding,
    context: &'static str,
) -> BehaviorResult<Vec<Locator>> {
    let mut locators = Vec::new();
    for parameter in list.all_by_base(base) {
        if locators.len() >= MAX_ANNOUNCED_LOCATORS {
            return Err(BehaviorError::MalformedParameter {
                context,
                pid: base,
                reason: "more locators than one sample may carry",
            });
        }
        let locator: Locator =
            parameter
                .decode_value(encoding)
                .map_err(|_| BehaviorError::MalformedParameter {
                    context,
                    pid: base,
                    reason: "locator is not twenty-four octets",
                })?;
        locators.push(locator);
    }
    Ok(locators)
}

/// Decode a CDR string parameter, refusing one longer than `limit`.
///
/// # Errors
///
/// [`BehaviorError::MalformedParameter`] when the value is not a string or is
/// too long.
pub fn decode_bounded_string(
    list: &ParameterList<'_>,
    base: u16,
    encoding: Encoding,
    limit: usize,
    context: &'static str,
) -> BehaviorResult<Option<String>> {
    let Some(parameter) = list.get_by_base(base) else {
        return Ok(None);
    };
    let text: String =
        parameter
            .decode_value(encoding)
            .map_err(|_| BehaviorError::MalformedParameter {
                context,
                pid: base,
                reason: "value is not a CDR string",
            })?;
    if text.len() > limit {
        return Err(BehaviorError::MalformedParameter {
            context,
            pid: base,
            reason: "string is longer than this decoder accepts",
        });
    }
    Ok(Some(text))
}

/// Decode a required CDR string parameter, refusing one longer than `limit`.
///
/// # Errors
///
/// [`BehaviorError::MissingParameter`] when absent, plus everything
/// [`decode_bounded_string`] reports.
pub fn decode_required_string(
    list: &ParameterList<'_>,
    base: u16,
    encoding: Encoding,
    limit: usize,
    context: &'static str,
) -> BehaviorResult<String> {
    decode_bounded_string(list, base, encoding, limit, context)?
        .ok_or(BehaviorError::MissingParameter { context, pid: base })
}

/// Decode an octet-sequence parameter, refusing one longer than `limit`.
///
/// An absent parameter reads as empty, which is what every octet-sequence QoS
/// defaults to.
///
/// # Errors
///
/// [`BehaviorError::MalformedParameter`] when the value is not an octet
/// sequence or is too long.
pub fn decode_octets(
    list: &ParameterList<'_>,
    base: u16,
    encoding: Encoding,
    limit: usize,
    context: &'static str,
) -> BehaviorResult<Vec<u8>> {
    let Some(parameter) = list.get_by_base(base) else {
        return Ok(Vec::new());
    };
    let octets: Vec<u8> =
        parameter
            .decode_value(encoding)
            .map_err(|_| BehaviorError::MalformedParameter {
                context,
                pid: base,
                reason: "value is not an octet sequence",
            })?;
    if octets.len() > limit {
        return Err(BehaviorError::MalformedParameter {
            context,
            pid: base,
            reason: "octet sequence is longer than this decoder accepts",
        });
    }
    Ok(octets)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use astrs_cdr::{ParameterId, pid};
    use std::net::Ipv4Addr;

    const CONTEXT: &str = "test sample";

    fn list() -> ParameterList<'static> {
        ParameterList::new(Encoding::DISCOVERY)
    }

    #[test]
    fn an_absent_parameter_is_none_not_an_error() {
        let empty = list();
        let value: Option<u32> =
            decode_one(&empty, pid::DOMAIN_ID, Encoding::DISCOVERY, CONTEXT).unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn a_required_absent_parameter_names_itself() {
        let empty = list();
        let error = decode_required::<u32>(&empty, pid::DOMAIN_ID, Encoding::DISCOVERY, CONTEXT)
            .expect_err("must reject");
        assert_eq!(
            error,
            BehaviorError::MissingParameter {
                context: CONTEXT,
                pid: pid::DOMAIN_ID,
            }
        );
    }

    #[test]
    fn a_truncated_value_is_malformed_not_missing() {
        let mut parameters = list();
        parameters
            .push_octets(ParameterId::new(pid::DOMAIN_ID), vec![0_u8])
            .unwrap();
        let error =
            decode_required::<Locator>(&parameters, pid::DOMAIN_ID, Encoding::DISCOVERY, CONTEXT)
                .expect_err("must reject");
        assert!(matches!(error, BehaviorError::MalformedParameter { .. }));
    }

    #[test]
    fn repeated_locators_accumulate_in_order() {
        let mut parameters = list();
        for port in 1..=3_u16 {
            parameters
                .push_value(
                    ParameterId::new(pid::UNICAST_LOCATOR),
                    &Locator::udpv4(Ipv4Addr::LOCALHOST, port),
                )
                .unwrap();
        }
        let locators = decode_locators(
            &parameters,
            pid::UNICAST_LOCATOR,
            Encoding::DISCOVERY,
            CONTEXT,
        )
        .unwrap();
        assert_eq!(locators.len(), 3);
        assert_eq!(locators[0].udp_port(), Some(1));
        assert_eq!(locators[2].udp_port(), Some(3));
    }

    #[test]
    fn too_many_locators_are_refused() {
        let mut parameters = list();
        for port in 0..=(MAX_ANNOUNCED_LOCATORS as u16) {
            parameters
                .push_value(
                    ParameterId::new(pid::UNICAST_LOCATOR),
                    &Locator::udpv4(Ipv4Addr::LOCALHOST, 7400 + port),
                )
                .unwrap();
        }
        let error = decode_locators(
            &parameters,
            pid::UNICAST_LOCATOR,
            Encoding::DISCOVERY,
            CONTEXT,
        )
        .expect_err("must reject");
        assert!(matches!(error, BehaviorError::MalformedParameter { .. }));
    }

    #[test]
    fn a_string_longer_than_the_bound_is_refused() {
        let mut parameters = list();
        let long = "x".repeat(MAX_NAME_LEN + 1);
        parameters
            .push_value(ParameterId::new(pid::TOPIC_NAME), long.as_str())
            .unwrap();
        let error = decode_bounded_string(
            &parameters,
            pid::TOPIC_NAME,
            Encoding::DISCOVERY,
            MAX_NAME_LEN,
            CONTEXT,
        )
        .expect_err("must reject");
        assert!(matches!(error, BehaviorError::MalformedParameter { .. }));
    }

    #[test]
    fn a_string_at_the_bound_is_accepted() {
        let mut parameters = list();
        let exact = "y".repeat(MAX_NAME_LEN);
        parameters
            .push_value(ParameterId::new(pid::TOPIC_NAME), exact.as_str())
            .unwrap();
        let read = decode_bounded_string(
            &parameters,
            pid::TOPIC_NAME,
            Encoding::DISCOVERY,
            MAX_NAME_LEN,
            CONTEXT,
        )
        .unwrap();
        assert_eq!(read.as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn absent_octets_read_as_empty() {
        let empty = list();
        let octets =
            decode_octets(&empty, pid::USER_DATA, Encoding::DISCOVERY, 16, CONTEXT).unwrap();
        assert!(octets.is_empty());
    }

    #[test]
    fn oversized_octets_are_refused() {
        let mut parameters = list();
        parameters
            .push_value(ParameterId::new(pid::USER_DATA), &vec![0_u8; 17])
            .unwrap();
        let error = decode_octets(
            &parameters,
            pid::USER_DATA,
            Encoding::DISCOVERY,
            16,
            CONTEXT,
        )
        .expect_err("must reject");
        assert!(matches!(error, BehaviorError::MalformedParameter { .. }));
    }

    #[test]
    fn a_required_string_reports_absence() {
        let empty = list();
        let error = decode_required_string(
            &empty,
            pid::TYPE_NAME,
            Encoding::DISCOVERY,
            MAX_NAME_LEN,
            CONTEXT,
        )
        .expect_err("must reject");
        assert_eq!(
            error,
            BehaviorError::MissingParameter {
                context: CONTEXT,
                pid: pid::TYPE_NAME,
            }
        );
    }
}
