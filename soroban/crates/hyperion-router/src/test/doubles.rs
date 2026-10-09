//! Test doubles for the pieces of the world the router talks to but does not own.
//!
//! Two of these exist because the real thing cannot produce the failure we need to test. A
//! Stellar Asset Contract registered in the test environment creates trustlines on demand, so
//! it can never refuse a transfer, which makes the parked-claim path unreachable with real
//! assets. And a real rail cannot be summoned into a unit test at all, so `MockRail` stands in
//! for the adapter on the way out and the rail's own receiver on the way in.

use hyperion_core::{AddressKind, HyperionError, RouteKind};
use soroban_sdk::{
    auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation},
    contract, contracterror, contractimpl, contracttype,
    testutils::Events,
    vec,
    xdr::ContractEventBody,
    Address, Bytes, BytesN, Env, IntoVal, Map, String, Symbol, TryFromVal, Val, Vec,
};

use crate::types::{Origin, Recipient};
use crate::HyperionRouterClient;

// ------------------------------------------------------------------------------------------
// MockRail
// ------------------------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchRecord {
    pub caller: Address,
    pub token: Address,
    pub amount: i128,
    pub destination_chain: String,
    pub destination: BytesN<32>,
    pub nonce: u64,
}

#[contracttype]
#[derive(Clone)]
enum RailKey {
    Router,
    Last,
    Count,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum RailError {
    NotRouter = 1,
    NoDispatch = 2,
}

/// One inbound message, in the shape a rail adapter hands it over.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delivery {
    pub route: RouteKind,
    pub token: Address,
    pub amount: i128,
    pub recipient: Address,
    pub recipient_kind: AddressKind,
    pub source_chain: String,
    pub source_nonce: u64,
}

/// Stands in for both halves of a rail integration.
///
/// On the way out the router pushes funds here and calls `dispatch`, which is the moment the
/// real adapter would hand them to Circle or Axelar. On the way in this contract plays the
/// rail's receiver: it pre-authorises the router to pull the funds back out of it and then calls
/// `bridge_in`, which is exactly the shape the production adapters use.
#[contract]
pub struct MockRail;

#[contractimpl]
impl MockRail {
    pub fn init(env: Env, router: Address) {
        env.storage().instance().set(&RailKey::Router, &router);
    }

    pub fn dispatch(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        destination_chain: String,
        destination: BytesN<32>,
        nonce: u64,
    ) -> Result<(), RailError> {
        caller.require_auth();
        let router: Address = env.storage().instance().get(&RailKey::Router).unwrap();
        if caller != router {
            return Err(RailError::NotRouter);
        }
        let record = DispatchRecord {
            caller,
            token,
            amount,
            destination_chain,
            destination,
            nonce,
        };
        env.storage().instance().set(&RailKey::Last, &record);
        let count: u32 = env.storage().instance().get(&RailKey::Count).unwrap_or(0);
        env.storage().instance().set(&RailKey::Count, &(count + 1));
        Ok(())
    }

    /// Play the rail's receiver: authorise the pull, then hand the delivery to the router.
    ///
    /// The router's error is passed straight back out rather than aborting, so a test can name
    /// the refusal it expects instead of squinting at a generic panic.
    pub fn deliver(env: Env, delivery: Delivery) -> Result<u64, HyperionError> {
        let Delivery {
            route,
            token,
            amount,
            recipient,
            recipient_kind,
            source_chain,
            source_nonce,
        } = delivery;
        let router: Address = env.storage().instance().get(&RailKey::Router).unwrap();
        let this = env.current_contract_address();

        // The router pulls rather than trusts, so the receiver has to say in advance that it is
        // good for the amount. This is the documented way for one contract to authorise a
        // transfer that a different contract will actually invoke, and it is not covered by
        // `mock_all_auths`, which only ever stands in for root level signatures.
        env.authorize_as_current_contract(vec![
            &env,
            InvokerContractAuthEntry::Contract(SubContractInvocation {
                context: ContractContext {
                    contract: token.clone(),
                    fn_name: Symbol::new(&env, "transfer"),
                    args: (this.clone(), router.clone(), amount).into_val(&env),
                },
                sub_invocations: vec![&env],
            }),
        ]);

        let outcome = HyperionRouterClient::new(&env, &router).try_bridge_in(
            &this,
            &route,
            &token,
            &amount,
            &Recipient {
                address: recipient,
                kind: recipient_kind,
                raw: Bytes::new(&env),
            },
            &Origin {
                chain: source_chain,
                nonce: source_nonce,
                message_id: message_id(&env, source_nonce),
                sender: BytesN::from_array(&env, &[7u8; 32]),
            },
        );

        match outcome {
            Ok(Ok(claim_id)) => Ok(claim_id),
            Err(Ok(err)) => Err(err),
            // Anything else means the router did not get far enough to name a reason, which on
            // this path is the token itself refusing the pull. Let it abort loudly.
            _ => panic!("the router could not take the delivery"),
        }
    }

    pub fn last_dispatch(env: Env) -> Result<DispatchRecord, RailError> {
        env.storage()
            .instance()
            .get(&RailKey::Last)
            .ok_or(RailError::NoDispatch)
    }

    pub fn dispatch_count(env: Env) -> u32 {
        env.storage().instance().get(&RailKey::Count).unwrap_or(0)
    }
}

// ------------------------------------------------------------------------------------------
// FailableToken
// ------------------------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
enum TokenKey {
    Balance(Address),
    Blocked(Address),
    Decimals,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum TokenError {
    InsufficientBalance = 1,
    /// What a missing trustline looks like from the caller's side: the transfer simply fails.
    RecipientCannotReceive = 2,
    NegativeAmount = 3,
}

/// A token that can be told to refuse a specific recipient.
///
/// This is the only honest way to reach the parked-claim path in a unit test. On the real
/// network the refusal comes from a classic account having no trustline for the asset, which the
/// test environment's own asset contracts will never reproduce because they open trustlines for
/// you.
#[contract]
pub struct FailableToken;

#[contractimpl]
impl FailableToken {
    pub fn init(env: Env, decimals: u32) {
        env.storage().instance().set(&TokenKey::Decimals, &decimals);
    }

    pub fn mint(env: Env, to: Address, amount: i128) {
        let current = Self::balance(env.clone(), to.clone());
        env.storage()
            .instance()
            .set(&TokenKey::Balance(to), &(current + amount));
    }

    /// Stop accepting transfers into `who`, or start again.
    pub fn set_blocked(env: Env, who: Address, blocked: bool) {
        env.storage()
            .instance()
            .set(&TokenKey::Blocked(who), &blocked);
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .instance()
            .get(&TokenKey::Balance(id))
            .unwrap_or(0i128)
    }

    pub fn decimals(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&TokenKey::Decimals)
            .unwrap_or(7)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) -> Result<(), TokenError> {
        from.require_auth();
        if amount < 0 {
            return Err(TokenError::NegativeAmount);
        }
        let blocked: bool = env
            .storage()
            .instance()
            .get(&TokenKey::Blocked(to.clone()))
            .unwrap_or(false);
        if blocked {
            return Err(TokenError::RecipientCannotReceive);
        }
        let from_balance = Self::balance(env.clone(), from.clone());
        if from_balance < amount {
            return Err(TokenError::InsufficientBalance);
        }
        let to_balance = Self::balance(env.clone(), to.clone());
        env.storage()
            .instance()
            .set(&TokenKey::Balance(from), &(from_balance - amount));
        env.storage()
            .instance()
            .set(&TokenKey::Balance(to), &(to_balance + amount));
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------
// Helpers shared by the test modules
// ------------------------------------------------------------------------------------------

/// Every route turned on and pointed at the same mock rail, which is what most tests want.
pub fn all_routes() -> [RouteKind; 4] {
    [
        RouteKind::Cctp,
        RouteKind::AxelarIts,
        RouteKind::AxelarGmp,
        RouteKind::Allbridge,
    ]
}

/// The thirty two byte message identifier a test nonce stands for.
///
/// Rails hand over wide identifiers and the tests talk in small numbers, so this is the one
/// place the two meet. Writing the number into the tail mirrors how a CCTP v2 nonce or an
/// Axelar message id actually looks once it reaches the router.
pub fn message_id(env: &Env, nonce: u64) -> BytesN<32> {
    let mut raw = [0u8; 32];
    raw[24..].copy_from_slice(&nonce.to_be_bytes());
    BytesN::from_array(env, &raw)
}

/// A left-padded EVM address, the shape the router insists on for an outbound destination.
pub fn evm_destination(env: &Env, last_byte: u8) -> BytesN<32> {
    let mut raw = [0u8; 32];
    for (i, slot) in raw.iter_mut().enumerate().skip(12) {
        *slot = if i == 31 { last_byte } else { 0xAB };
    }
    BytesN::from_array(env, &raw)
}

/// A 32 byte word with a nonzero byte in the padding region, which is what a Stellar key looks
/// like if somebody drops it into an EVM address field by mistake.
pub fn not_an_evm_address(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[0x5Au8; 32])
}

// ------------------------------------------------------------------------------------------
// Event assertions
// ------------------------------------------------------------------------------------------
//
// The router's published events are part of its spec: the indexer keys transfers and claims off
// them and the monitoring job watches the privileged ones. These helpers read them back out of
// the test environment and decode them to the `Val` form they were published as, so a test can
// name a topic or a field and fail if it moves. Events published by anybody else the router
// calls, such as the token contract's own `transfer` events, are filtered out by contract.

/// Every event the router itself published during the most recent invocation, as
/// `(topics, data)` pairs.
pub fn router_events(env: &Env, router: &Address) -> Vec<(Vec<Val>, Val)> {
    let mut out = Vec::new(env);
    let events = env.events().all().filter_by_contract(router);
    for event in events.events() {
        let ContractEventBody::V0(body) = &event.body;
        let mut topics = Vec::new(env);
        for topic in body.topics.iter() {
            topics.push_back(Val::try_from_val(env, &topic).unwrap());
        }
        out.push_back((topics, Val::try_from_val(env, &body.data).unwrap()));
    }
    out
}

/// The topics and data of the one router event whose second topic is `kind`.
///
/// Panics when the event is absent or was published more than once, so a test that expects an
/// announcement fails if the router ever stops making it.
pub fn router_event(env: &Env, router: &Address, kind: &str) -> (Vec<Val>, Val) {
    let wanted = Symbol::new(env, kind);
    let mut found: Option<(Vec<Val>, Val)> = None;
    for (topics, data) in router_events(env, router).iter() {
        let second = topics
            .get(1)
            .and_then(|t| Symbol::try_from_val(env, &t).ok());
        if second == Some(wanted.clone()) {
            assert!(found.is_none(), "more than one `{}` event", kind);
            found = Some((topics, data));
        }
    }
    match found {
        Some(pair) => pair,
        None => panic!("no `{}` event was published", kind),
    }
}

/// Assert that an event opens with exactly the two topics `first` and `second`.
///
/// The names are deliberately literals at the call site rather than read back off the `events::`
/// structs: the whole point is to catch a topic string being renamed in `events.rs`, and reading
/// it back off the same struct would rename in step and never fail.
pub fn assert_topics(env: &Env, topics: &Vec<Val>, first: &str, second: &str) {
    let leading = Symbol::try_from_val(env, &topics.get(0).unwrap()).unwrap();
    let trailing = Symbol::try_from_val(env, &topics.get(1).unwrap()).unwrap();
    assert_eq!(leading, Symbol::new(env, first));
    assert_eq!(trailing, Symbol::new(env, second));
}

/// Read a named field out of an event's data, or out of one of the records embedded in it.
///
/// Panics if the name is not there, which is what turns an embedded field rename into a failing
/// test rather than a silently different read.
pub fn field(env: &Env, value: &Val, name: &str) -> Val {
    let map = Map::<Symbol, Val>::try_from_val(env, value).unwrap();
    match map.get(Symbol::new(env, name)) {
        Some(v) => v,
        None => panic!("no `{}` field on the event", name),
    }
}

/// Decode an event topic or field back into the type it was published as.
pub fn decode<T>(env: &Env, value: &Val) -> T
where
    T: TryFromVal<Env, Val>,
{
    T::try_from_val(env, value).unwrap()
}
