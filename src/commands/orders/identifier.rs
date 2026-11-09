//! How a command names an order that already exists: by order id or by client
//! order id. Below validation and planning, which both read it.

use either::Either;
use hypersdk::Decimal;
use hypersdk::hypercore::{Cloid, OidOrCloid};

use super::args::ModifyArgs;
use super::cloid::{parse_cloid, parse_lookup_cloid};
use crate::errors::CliError;

#[derive(Debug, Clone)]
pub(super) enum OrderIdentifier {
    Oid(u64),
    Cloid { raw: String, parsed: Cloid },
}

impl OrderIdentifier {
    /// The order a cancel names. A short or unprefixed CLOID is padded, like a
    /// lookup.
    pub(super) fn for_cancel(order_id: Option<u64>, cloid: Option<&str>) -> Result<Self, CliError> {
        Self::from_flags(order_id, cloid, "cancel", parse_lookup_cloid)
    }

    /// The order a modify names. The CLOID must be exactly 16 bytes, since the
    /// replacement is sent under it.
    pub(super) fn for_modify(order_id: Option<u64>, cloid: Option<&str>) -> Result<Self, CliError> {
        Self::from_flags(order_id, cloid, "modify", parse_cloid)
    }

    fn from_flags(
        order_id: Option<u64>,
        cloid: Option<&str>,
        command: &str,
        parse: fn(&str) -> Result<Cloid, CliError>,
    ) -> Result<Self, CliError> {
        if let Some(order_id) = order_id {
            return Ok(Self::Oid(order_id));
        }
        let raw = cloid.ok_or_else(|| {
            CliError::Configuration(format!("orders {command} requires ORDER_ID or --cloid"))
        })?;
        Ok(Self::Cloid {
            raw: raw.to_string(),
            parsed: parse(raw)?,
        })
    }

    /// The order the way the exchange keys it: an order id, or a client order
    /// id. Two identifiers name the same order when these are equal.
    pub(super) fn to_status_identifier(&self) -> OidOrCloid {
        match self {
            Self::Oid(oid) => Either::Left(*oid),
            Self::Cloid { parsed, .. } => Either::Right(*parsed),
        }
    }

    pub(super) fn display(&self) -> String {
        match self {
            Self::Oid(oid) => oid.to_string(),
            Self::Cloid { raw, .. } => raw.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct BatchModifyOrder {
    pub(super) identifier: OrderIdentifier,
    pub(super) price: Option<Decimal>,
    pub(super) trigger_price: Option<Decimal>,
    pub(super) size: Option<Decimal>,
}

impl BatchModifyOrder {
    /// The flags `orders modify` would have been given for this order, so a
    /// batch leg is planned by the one modify planner.
    pub(super) fn modify_args(&self, always_place: bool) -> ModifyArgs {
        let (order_id, cloid) = match &self.identifier {
            OrderIdentifier::Oid(oid) => (Some(*oid), None),
            OrderIdentifier::Cloid { raw, .. } => (None, Some(raw.clone())),
        };
        ModifyArgs {
            order_id,
            cloid,
            price: self.price,
            trigger_price: self.trigger_price,
            size: self.size,
            always_place,
            on_behalf_of: None,
        }
    }
}

/// Fail when a batch names one order twice: the exchange would apply one
/// modification over the other. `earlier` are the keys of the orders before
/// the one at `index`. The check runs in two phases, which differ in what
/// they can know of an order:
///
/// - reading the orders file, the key is the identifier as written
///   ([`OrderIdentifier::to_status_identifier`]), so the same order id or the
///   same cloid repeats;
/// - at lookup, the key is the order id each order resolved to, so an order
///   id and a cloid turn out to name one order.
pub(super) fn ensure_distinct<K: PartialEq>(
    index: usize,
    key: K,
    earlier: impl Iterator<Item = K>,
) -> Result<(), CliError> {
    match earlier.enumerate().find(|(_, earlier)| *earlier == key) {
        Some((earlier, _)) => Err(CliError::Configuration(format!(
            "orders[{index}] modifies the same order as orders[{earlier}]"
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_pads_a_short_cloid_and_a_modify_does_not() {
        let padded = "0x000000000000000000000000deadbeef"
            .parse::<Cloid>()
            .unwrap();
        match OrderIdentifier::for_cancel(None, Some("0xdeadbeef")).unwrap() {
            OrderIdentifier::Cloid { raw, parsed } => {
                assert_eq!(raw, "0xdeadbeef");
                assert_eq!(parsed, padded);
            }
            OrderIdentifier::Oid(_) => panic!("a cloid names the order"),
        }
        let err = OrderIdentifier::for_modify(None, Some("0xdeadbeef")).unwrap_err();
        assert!(err.to_string().contains("exactly 16 bytes"), "{err}");
    }

    #[test]
    fn an_order_id_wins_over_a_cloid_and_one_of_them_is_required() {
        assert!(matches!(
            OrderIdentifier::for_modify(Some(5), Some("not-a-cloid")).unwrap(),
            OrderIdentifier::Oid(5)
        ));
        let err = OrderIdentifier::for_cancel(None, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Configuration error: orders cancel requires ORDER_ID or --cloid"
        );
    }
}
