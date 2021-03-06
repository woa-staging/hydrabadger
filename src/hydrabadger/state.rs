//! Hydrabadger state.
//!
//! FIXME: Reorganize `Handler` and `State` to more clearly separate concerns.
//!

#![allow(dead_code)]

use std::{
    fmt,
    collections::BTreeMap,
};
use crossbeam::queue::SegQueue;
use hbbft::{
    crypto::{PublicKey, SecretKey},
    sync_key_gen::{SyncKeyGen, Part, PartOutcome, Ack},
    messaging::{DistAlgorithm, NetworkInfo},
    queueing_honey_badger::{Error as QhbError, QueueingHoneyBadger},
    dynamic_honey_badger::{DynamicHoneyBadger, JoinPlan},

};
use peer::Peers;
use ::{Uid, NetworkState, NetworkNodeInfo, Message, Transaction, Step, Input};
use super::{InputOrMessage, Error, Config};
// use super::{BATCH_SIZE, config.keygen_peer_count};


/// A `State` discriminant.
#[derive(Copy, Clone, Debug)]
pub enum StateDsct {
    Disconnected,
    DeterminingNetworkState,
    AwaitingMorePeersForKeyGeneration,
    GeneratingKeys,
    Observer,
    Validator,
}

impl fmt::Display for StateDsct {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<StateDsct> for usize {
    fn from(dsct: StateDsct) -> usize {
        match dsct {
            StateDsct::Disconnected => 0,
            StateDsct::DeterminingNetworkState => 1,
            StateDsct::AwaitingMorePeersForKeyGeneration => 2,
            StateDsct::GeneratingKeys => 3,
            StateDsct::Observer => 4,
            StateDsct::Validator => 5,
        }
    }
}

impl From<usize> for StateDsct {
    fn from(val: usize) -> StateDsct {
        match val {
            0 => StateDsct::Disconnected,
            1 => StateDsct::DeterminingNetworkState,
            2 => StateDsct::AwaitingMorePeersForKeyGeneration,
            3 => StateDsct::GeneratingKeys,
            4 => StateDsct::Observer,
            5 => StateDsct::Validator,
            _ => panic!("Invalid state discriminant."),
        }
    }
}


/// The current hydrabadger state.
//
// TODO: Make this into a struct and move the `state_dsct: AtomicUsize` field
// into it.
//
pub(crate) enum State {
    Disconnected { },
    DeterminingNetworkState {
        ack_queue: Option<SegQueue<(Uid, Ack)>>,
        iom_queue: Option<SegQueue<InputOrMessage>>,
        network_state: Option<NetworkState>,
    },
    AwaitingMorePeersForKeyGeneration {
        // Queued input to HoneyBadger:
        // FIXME: ACTUALLY USE THIS QUEUED INPUT!
        ack_queue: Option<SegQueue<(Uid, Ack)>>,
        iom_queue: Option<SegQueue<InputOrMessage>>,
    },
    GeneratingKeys {
        sync_key_gen: Option<SyncKeyGen<Uid>>,
        public_key: Option<PublicKey>,
        public_keys: BTreeMap<Uid, PublicKey>,

        ack_queue: Option<SegQueue<(Uid, Ack)>>,
        part_count: usize,
        ack_count: usize,

        // Queued input to HoneyBadger:
        iom_queue: Option<SegQueue<InputOrMessage>>,
    },
    Observer {
        qhb: Option<QueueingHoneyBadger<Vec<Transaction>, Uid>>,
    },
    Validator {
        qhb: Option<QueueingHoneyBadger<Vec<Transaction>, Uid>>,
    },
}

impl State {
    /// Returns the state discriminant.
    pub(super) fn discriminant(&self) -> StateDsct {
        match self {
            State::Disconnected { .. } => StateDsct::Disconnected,
            State::DeterminingNetworkState { .. } => StateDsct::DeterminingNetworkState,
            State::AwaitingMorePeersForKeyGeneration { .. } =>
                StateDsct::AwaitingMorePeersForKeyGeneration,
            State::GeneratingKeys{ .. } => StateDsct::GeneratingKeys,
            State::Observer { .. } => StateDsct::Observer,
            State::Validator { .. } => StateDsct::Validator,
        }
    }

    /// Returns a new `State::Disconnected`.
    pub(super) fn disconnected(/*local_uid: Uid, local_addr: InAddr,*/ /*secret_key: SecretKey*/)
            -> State {
        State::Disconnected { /*secret_key: secret_key*/ }
    }

    // /// Sets the state to `DeterminingNetworkState`.
    // //
    // // TODO: Add proper error handling:
    // fn set_determining_network_state(&mut self) {
    //     *self = match self {
    //         State::Disconnected { } => {
    //             warn!("Setting state: `DeterminingNetworkState`.");
    //             State::DeterminingNetworkState { }
    //         },
    //         _ => panic!("Must be disconnected before calling `::peer_connection_added`."),
    //     };
    // }

    /// Sets the state to `AwaitingMorePeersForKeyGeneration`.
    pub(super) fn set_awaiting_more_peers(&mut self) {
        *self = match self {
            State::Disconnected { } => {
                warn!("Setting state: `AwaitingMorePeersForKeyGeneration`.");
                State::AwaitingMorePeersForKeyGeneration {
                    ack_queue: Some(SegQueue::new()),
                    iom_queue: Some(SegQueue::new()),
                }
            },
            State::DeterminingNetworkState { ref mut iom_queue, ref mut ack_queue,
                    ref network_state } => {
                assert!(!network_state.is_some(),
                    "State::set_awaiting_more_peers: Network is active!");
                warn!("Setting state: `AwaitingMorePeersForKeyGeneration`.");
                State::AwaitingMorePeersForKeyGeneration {
                    ack_queue: ack_queue.take(),
                    iom_queue: iom_queue.take(),
                }
            },
            s @ _ => {
                debug!("State::set_awaiting_more_peers: Attempted to set \
                    `State::AwaitingMorePeersForKeyGeneration` while {}.", s.discriminant());
                return
            }
        };
    }

    /// Sets the state to `AwaitingMorePeersForKeyGeneration`.
    pub(super) fn set_generating_keys(&mut self, local_uid: &Uid, local_sk: SecretKey, peers: &Peers,
            config: &Config) -> Result<(Part, Ack), Error> {
        let (part, ack);
        *self = match self {
            State::AwaitingMorePeersForKeyGeneration { ref mut iom_queue, ref mut ack_queue } => {
                // let secret_key = secret_key.clone();
                let threshold = config.keygen_peer_count / 3;

                let mut public_keys: BTreeMap<Uid, PublicKey> = peers.validators().map(|p| {
                    p.pub_info().map(|(uid, _, pk)| (*uid, *pk)).unwrap()
                }).collect();

                let pk = local_sk.public_key();
                public_keys.insert(*local_uid, pk);

                let (mut sync_key_gen, opt_part) = SyncKeyGen::new(*local_uid, local_sk,
                    public_keys.clone(), threshold).map_err(Error::SyncKeyGenNew)?;
                part = opt_part.expect("This node is not a validator (somehow)!");

                warn!("KEY GENERATION: Handling our own `Part`...");
                ack = match sync_key_gen.handle_part(&local_uid, part.clone()) {
                    Some(PartOutcome::Valid(ack)) => ack,
                    Some(PartOutcome::Invalid(faults)) => panic!("Invalid part \
                        (FIXME: handle): {:?}", faults),
                    None => unimplemented!(),
                };

                // warn!("KEY GENERATION: Handling our own `Ack`...");
                // let fault_log = sync_key_gen.handle_ack(local_uid, ack.clone());
                // if !fault_log.is_empty() {
                //     error!("Errors acknowledging part (from self):\n {:?}", fault_log);
                // }

                warn!("KEY GENERATION: Queueing our own `Ack`...");
                ack_queue.as_ref().unwrap().push((*local_uid, ack.clone()));

                State::GeneratingKeys {
                    sync_key_gen: Some(sync_key_gen),
                    public_key: Some(pk),
                    public_keys,
                    ack_queue: ack_queue.take(),
                    part_count: 1,
                    ack_count: 0,
                    iom_queue: iom_queue.take(),
                }
            },
            _ => panic!("State::set_generating_keys: \
                Must be State::AwaitingMorePeersForKeyGeneration"),
        };

        Ok((part, ack))
    }

    /// Changes the variant (in-place) of this `State` to `Observer`.
    //
    // TODO: Add proper error handling:
    #[must_use]
    pub(super) fn set_observer(&mut self, local_uid: Uid, local_sk: SecretKey,
            jp: JoinPlan<Uid>, cfg: &Config, step_queue: &SegQueue<Step>)
            -> Result<SegQueue<InputOrMessage>, Error> {
        let iom_queue_ret;
        *self = match self {
            State::DeterminingNetworkState { ref mut iom_queue, .. } => {
                let (dhb, dhb_step) = DynamicHoneyBadger::builder()
                    .build_joining(local_uid, local_sk, jp)?;
                step_queue.push(dhb_step.convert());

                let (qhb, qhb_step) = QueueingHoneyBadger::builder(dhb)
                    .batch_size(cfg.batch_size)
                    .build();
                step_queue.push(qhb_step);

                iom_queue_ret = iom_queue.take().unwrap();

                warn!("");
                warn!("== HONEY BADGER INITIALIZED ==");
                warn!("");

                { // TODO: Consolidate or remove:
                    let pk_set = qhb.dyn_hb().netinfo().public_key_set();
                    let pk_map = qhb.dyn_hb().netinfo().public_key_map();
                    warn!("");
                    warn!("");
                    warn!("PUBLIC KEY: {:?}", pk_set.public_key());
                    warn!("PUBLIC KEY SET: \n{:?}", pk_set);
                    warn!("PUBLIC KEY MAP: \n{:?}", pk_map);
                    warn!("");
                    warn!("");
                }

                State::Observer { qhb: Some(qhb) }
            },
            s @ _ => panic!("State::set_observer: State must be `GeneratingKeys`. \
                State: {}", s.discriminant()),
        };
        Ok(iom_queue_ret)
    }

    /// Changes the variant (in-place) of this `State` to `Observer`.
    //
    // TODO: Add proper error handling:
    #[must_use]
    pub(super) fn set_validator(&mut self, local_uid: Uid, local_sk: SecretKey, peers: &Peers,
            cfg: &Config, step_queue: &SegQueue<Step>)
            -> Result<SegQueue<InputOrMessage>, Error> {
        let iom_queue_ret;
        *self = match self {
            State::GeneratingKeys { ref mut sync_key_gen, mut public_key,
                    ref mut iom_queue, .. } => {
                let mut sync_key_gen = sync_key_gen.take().unwrap();
                assert_eq!(public_key.take().unwrap(), local_sk.public_key());

                let (pk_set, sk_share_opt) = sync_key_gen.generate()
                    .map_err(Error::SyncKeyGenGenerate)?;
                let sk_share = sk_share_opt.unwrap();

                assert!(peers.count_validators() >= cfg.keygen_peer_count);

                let mut node_ids: BTreeMap<Uid, PublicKey> = peers.validators().map(|p| {
                    (p.uid().cloned().unwrap(), p.public_key().cloned().unwrap())
                }).collect();
                node_ids.insert(local_uid, local_sk.public_key());

                let netinfo = NetworkInfo::new(
                    local_uid,
                    sk_share,
                    pk_set,
                    local_sk,
                    node_ids,
                );

                let dhb = DynamicHoneyBadger::builder()
                    .build(netinfo);

                let (qhb, qhb_step) = QueueingHoneyBadger::builder(dhb)
                    .batch_size(cfg.batch_size)
                    .build();
                step_queue.push(qhb_step);

                warn!("");
                warn!("== HONEY BADGER INITIALIZED ==");
                warn!("");

                { // TODO: Consolidate or remove:
                    let pk_set = qhb.dyn_hb().netinfo().public_key_set();
                    let pk_map = qhb.dyn_hb().netinfo().public_key_map();
                    warn!("");
                    warn!("");
                    warn!("PUBLIC KEY: {:?}", pk_set.public_key());
                    warn!("PUBLIC KEY SET: \n{:?}", pk_set);
                    warn!("PUBLIC KEY MAP: \n{:?}", pk_map);
                    warn!("");
                    warn!("");
                }


                iom_queue_ret = iom_queue.take().unwrap();
                State::Validator { qhb: Some(qhb) }
            },
            s @ _ => panic!("State::set_validator: State must be `GeneratingKeys`. State: {}",
                s.discriminant()),
        };
        Ok(iom_queue_ret)
    }

    #[must_use]
    pub(super) fn promote_to_validator(&mut self) -> Result<(), Error> {
        *self = match self {
            State::Observer { ref mut qhb } => {
                warn!("=== PROMOTING NODE TO VALIDATOR ===");
                State::Validator { qhb: qhb.take() }
            },
            s @ _ => panic!("State::promote_to_validator: State must be `Observer`. State: {}",
                s.discriminant()),
        };
        Ok(())
    }

    /// Sets state to `DeterminingNetworkState` if `Disconnected`, otherwise does
    /// nothing.
    pub(super) fn update_peer_connection_added(&mut self, _peers: &Peers) {
        let _dsct = self.discriminant();
        *self = match self {
            State::Disconnected { } => {
                warn!("Setting state: `DeterminingNetworkState`.");
                State::DeterminingNetworkState {
                    ack_queue: Some(SegQueue::new()),
                    iom_queue: Some(SegQueue::new()),
                    network_state: None,
                }
            },
            _ => return,
        };
    }

    /// Sets state to `Disconnected` if peer count is zero, otherwise does nothing.
    pub(super) fn update_peer_connection_dropped(&mut self, peers: &Peers) {
        *self = match self {
            State::DeterminingNetworkState { .. } => {
                if peers.count_total() == 0 {
                    State::Disconnected { }
                } else {
                    return;
                }
            },
            State::Disconnected { .. } => {
                error!("Received peer disconnection when `State::Disconnected`.");
                assert_eq!(peers.count_total(), 0);
                return;
            },
            State::AwaitingMorePeersForKeyGeneration { .. } => {
                debug!("Ignoring peer disconnection when \
                    `State::AwaitingMorePeersForKeyGeneration`.");
                return;
            },
            State::GeneratingKeys { .. } => {
                panic!("FIXME: RESTART KEY GENERATION PROCESS AFTER PEER DISCONNECTS.");
            }
            State::Observer { qhb: _, .. } => {
                debug!("Ignoring peer disconnection when `State::Observer`.");
                return;
            },
            State::Validator { qhb: _, .. } => {
                debug!("Ignoring peer disconnection when `State::Validator`.");
                return;
            },
        }
    }

    /// Returns the network state, if possible.
    pub(super) fn network_state(&self, peers: &Peers) -> NetworkState {
        let peer_infos = peers.peers().filter_map(|peer| {
            peer.pub_info().map(|(&uid, &in_addr, &pk)| {
                NetworkNodeInfo { uid, in_addr, pk }
            })
        }).collect::<Vec<_>>();
        match self {
            State::AwaitingMorePeersForKeyGeneration { .. } => {
                NetworkState::AwaitingMorePeersForKeyGeneration(peer_infos)
            },
            State::GeneratingKeys{ ref public_keys, .. } => {
                NetworkState::GeneratingKeys(peer_infos, public_keys.clone())
            },
            State::Observer { ref qhb } | State::Validator { ref qhb } => {
                // FIXME: Ensure that `peer_info` matches `NetworkInfo` from HB.
                let pk_set = qhb.as_ref().unwrap().dyn_hb().netinfo().public_key_set().clone();
                let pk_map = qhb.as_ref().unwrap().dyn_hb().netinfo().public_key_map().clone();
                NetworkState::Active((peer_infos, pk_set, pk_map))
            },
            _ => NetworkState::Unknown(peer_infos),
        }
    }

    /// Returns a reference to the internal HB instance.
    pub(super) fn qhb(&self) -> Option<&QueueingHoneyBadger<Vec<Transaction>, Uid>> {
        match self {
            State::Observer { ref qhb, .. } => qhb.as_ref(),
            State::Validator { ref qhb, .. } => qhb.as_ref(),
            _ => None,
        }
    }

    /// Returns a reference to the internal HB instance.
    pub(super) fn qhb_mut(&mut self) -> Option<&mut QueueingHoneyBadger<Vec<Transaction>, Uid>> {
        match self {
            State::Observer { ref mut qhb, .. } => qhb.as_mut(),
            State::Validator { ref mut qhb, .. } => qhb.as_mut(),
            _ => None,
        }
    }

    /// Presents input to HoneyBadger or queues it for later.
    ///
    /// Cannot be called while disconnected or connection-pending.
    pub(super) fn input(&mut self, input: Input) -> Option<Result<Step, QhbError>> {
        match self {
            State::Observer { ref mut qhb, .. } | State::Validator { ref mut qhb, .. } => {
                trace!("State::input: Inputting: {:?}", input);
                let step_opt = Some(qhb.as_mut().unwrap().input(input));

                match step_opt {
                    Some(ref step) => match step {
                        Ok(s) => trace!("State::input: QHB output: {:?}", s.output),
                        Err(err) => error!("State::input: QHB output error: {:?}", err),
                    },
                    None => trace!("State::input: QHB Output is `None`"),
                }

                return step_opt;
            },
            | State::AwaitingMorePeersForKeyGeneration { ref iom_queue, .. }
            | State::GeneratingKeys { ref iom_queue, .. }
            | State::DeterminingNetworkState { ref iom_queue, .. } => {
                trace!("State::input: Queueing input: {:?}", input);
                iom_queue.as_ref().unwrap().push(InputOrMessage::Input(input));
            },
            s @ _ => panic!("State::handle_message: Must be connected in order to input to \
                honey badger. State: {}", s.discriminant()),
        }
        None
    }

    /// Presents a message to HoneyBadger or queues it for later.
    ///
    /// Cannot be called while disconnected or connection-pending.
    pub(super) fn handle_message(&mut self, src_uid: &Uid, msg: Message)
            -> Option<Result<Step, QhbError>> {
        match self {
            | State::Observer { ref mut qhb, .. }
            | State::Validator { ref mut qhb, .. } => {
                trace!("State::handle_message: Handling message: {:?}", msg);
                let step_opt = Some(qhb.as_mut().unwrap().handle_message(src_uid, msg));

                match step_opt {
                    Some(ref step) => match step {
                        Ok(s) => trace!("State::handle_message: QHB output: {:?}", s.output),
                        Err(err) => error!("State::handle_message: QHB output error: {:?}", err),
                    },
                    None => trace!("State::handle_message: QHB Output is `None`"),
                }

                return step_opt;
            },
            | State::AwaitingMorePeersForKeyGeneration { ref iom_queue, .. }
            | State::GeneratingKeys { ref iom_queue, .. }
            | State::DeterminingNetworkState { ref iom_queue, .. } => {
                trace!("State::handle_message: Queueing message: {:?}", msg);
                iom_queue.as_ref().unwrap().push(InputOrMessage::Message(*src_uid, msg));
            },
            // State::GeneratingKeys { ref iom_queue, .. } => {
            //     iom_queue.as_ref().unwrap().push(InputOrMessage::Message(msg));
            // },
            s @ _ => panic!("State::handle_message: Must be connected in order to input to \
                honey badger. State: {}", s.discriminant()),
        }
        None
    }
}
