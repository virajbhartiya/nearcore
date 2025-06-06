use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use actix::{Addr, System};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::{FutureExt, future};
use near_actix_test_utils::run_actix;
use near_async::time::Clock;
use near_chain::test_utils::{ValidatorSchedule, account_id_to_shard_id};
use near_chain_configs::TEST_STATE_SYNC_TIMEOUT;
use near_client::{Query, RpcHandlerActor};
use near_crypto::InMemorySigner;
use near_network::client::ProcessTxRequest;
use near_network::types::PeerInfo;
use near_network::types::{NetworkRequests, NetworkResponses, PeerManagerMessageRequest};
use near_o11y::WithSpanContextExt;
use near_o11y::testonly::init_integration_logger;
use near_primitives::hash::{CryptoHash, hash as hash_func};
use near_primitives::network::PeerId;
use near_primitives::sharding::ChunkHash;
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::{AccountId, BlockHeight, BlockHeightDelta, BlockReference, ShardId};
use near_primitives::views::QueryRequest;
use near_primitives::views::QueryResponseKind::ViewAccount;
use parking_lot::RwLock;

use crate::env::setup::{ActorHandlesForTesting, setup_mock_all_validators};

fn get_validators_and_key_pairs() -> (ValidatorSchedule, Vec<PeerInfo>) {
    let vs = ValidatorSchedule::new().num_shards(4).block_producers_per_epoch(vec![
        ["test1.1", "test1.2", "test1.3", "test1.4"]
            .iter()
            .map(|account_id| account_id.parse().unwrap())
            .collect(),
        ["test2.1", "test2.2", "test2.3", "test2.4"]
            .iter()
            .map(|account_id| account_id.parse().unwrap())
            .collect(),
        ["test3.1", "test3.2", "test3.3", "test3.4", "test3.5", "test3.6", "test3.7", "test3.8"]
            .iter()
            .map(|account_id| account_id.parse().unwrap())
            .collect(),
    ]);
    let key_pairs = vec![
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(), // 4
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(), // 8
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(),
        PeerInfo::random(), // 16
    ];
    (vs, key_pairs)
}

fn send_tx(
    connector: &Addr<RpcHandlerActor>,
    from: AccountId,
    to: AccountId,
    amount: u128,
    nonce: u64,
    block_hash: CryptoHash,
) {
    let signer = InMemorySigner::test_signer(&"test1".parse().unwrap());
    connector.do_send(
        ProcessTxRequest {
            transaction: SignedTransaction::send_money(
                nonce, from, to, &signer, amount, block_hash,
            ),
            is_forwarded: false,
            check_only: false,
        }
        .with_span_context(),
    );
}

enum ReceiptsSyncPhases {
    WaitingForFirstBlock,
    WaitingForSecondBlock,
    WaitingForDistantEpoch,
    VerifyingOutgoingReceipts,
    WaitingForValidate,
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct StateRequestStruct {
    pub shard_id: ShardId,
    pub sync_hash: CryptoHash,
    pub sync_prev_prev_hash: Option<CryptoHash>,
    pub part_id: Option<u64>,
    pub peer_id: Option<PeerId>,
}

/// Sanity checks that the incoming and outgoing receipts are properly sent and received
#[ignore = "flaky test"]
#[test]
fn ultra_slow_test_catchup_receipts_sync_third_epoch() {
    test_catchup_receipts_sync_common(13, 1, false)
}

/// The test aggressively blocks lots of state requests
/// and causes at least two timeouts per node (first for header, second for parts).
///
/// WARNING! For your convenience, set manually STATE_SYNC_TIMEOUT to 1 before running the test.
/// It will be executed 10 times faster.
/// The reason of increasing block_prod_time in the test is to allow syncing complete.
/// Otherwise epochs will be changing faster than state sync happen.
#[ignore = "flaky test"]
#[test]
fn ultra_slow_test_catchup_receipts_sync_hold() {
    test_catchup_receipts_sync_common(13, 1, true)
}

#[ignore = "flaky test"]
#[test]
fn ultra_slow_test_catchup_receipts_sync_last_block() {
    test_catchup_receipts_sync_common(13, 5, false)
}

#[ignore = "flaky test"]
#[test]
fn ultra_slow_test_catchup_receipts_sync_distant_epoch() {
    test_catchup_receipts_sync_common(35, 1, false)
}

fn test_catchup_receipts_sync_common(wait_till: u64, send: u64, sync_hold: bool) {
    init_integration_logger();
    run_actix(async move {
        let connectors: Arc<RwLock<Vec<ActorHandlesForTesting>>> = Arc::new(RwLock::new(vec![]));

        let (vs, key_pairs) = get_validators_and_key_pairs();
        let archive = vec![true; vs.all_block_producers().count()];

        let phase = Arc::new(RwLock::new(ReceiptsSyncPhases::WaitingForFirstBlock));
        let seen_heights_with_receipts = Arc::new(RwLock::new(HashSet::<BlockHeight>::new()));
        let seen_hashes_with_state = Arc::new(RwLock::new(HashSet::<CryptoHash>::new()));

        let connectors1 = connectors.clone();
        let mut block_prod_time: u64 = 3200;
        if sync_hold {
            block_prod_time *= TEST_STATE_SYNC_TIMEOUT as u64;
        }
        let (conn, _) = setup_mock_all_validators(
            Clock::real(),
            vs,
            key_pairs,
            true,
            block_prod_time,
            false,
            false,
            5,
            false,
            archive,
            false,
            None,
            Box::new(move |_, _account_id: _, msg: &PeerManagerMessageRequest| {
                let msg = msg.as_network_requests_ref();
                let account_from = "test3.3".parse().unwrap();
                let account_to = "test1.1".parse().unwrap();
                let source_shard_id = account_id_to_shard_id(&account_from, 4);
                let destination_shard_id = account_id_to_shard_id(&account_to, 4);

                let mut phase = phase.write();
                let mut seen_heights_with_receipts = seen_heights_with_receipts.write();
                let mut seen_hashes_with_state = seen_hashes_with_state.write();
                match *phase {
                    ReceiptsSyncPhases::WaitingForFirstBlock => {
                        if let NetworkRequests::Block { block } = msg {
                            assert!(block.header().height() <= send);
                            // This tx is rather fragile, specifically it's important that
                            //   1. the `from` and `to` account are not in the same shard;
                            //   2. ideally the producer of the chunk at height 3 for the shard
                            //      in which `from` resides should not also be a block producer
                            //      at height 3
                            //   3. The `from` shard should also not match the block producer
                            //      for height 1, because such block producer will produce
                            //      the chunk for height 2 right away, before we manage to send
                            //      the transaction.
                            if block.header().height() == send {
                                println!(
                                    "From shard: {}, to shard: {}",
                                    source_shard_id, destination_shard_id,
                                );
                                for i in 0..16 {
                                    send_tx(
                                        &connectors1.write()[i].rpc_handler_actor,
                                        account_from.clone(),
                                        account_to.clone(),
                                        111,
                                        1,
                                        *block.header().prev_hash(),
                                    );
                                }
                                *phase = ReceiptsSyncPhases::WaitingForSecondBlock;
                            }
                        }
                    }
                    ReceiptsSyncPhases::WaitingForSecondBlock => {
                        // This block now contains a chunk with the transaction sent above.
                        if let NetworkRequests::Block { block } = msg {
                            assert!(block.header().height() <= send + 1);
                            if block.header().height() == send + 1 {
                                *phase = ReceiptsSyncPhases::WaitingForDistantEpoch;
                            }
                        }
                    }
                    ReceiptsSyncPhases::WaitingForDistantEpoch => {
                        // This block now contains a chunk with the transaction sent above.
                        if let NetworkRequests::Block { block } = msg {
                            assert!(block.header().height() >= send + 1);
                            assert!(block.header().height() <= wait_till);
                            if block.header().height() == wait_till {
                                *phase = ReceiptsSyncPhases::VerifyingOutgoingReceipts;
                            }
                        }
                        if let NetworkRequests::PartialEncodedChunkMessage {
                            partial_encoded_chunk,
                            ..
                        } = msg
                        {
                            // The chunk producers in all epochs before `distant` need to be trying to
                            //     include the receipt. The `distant` epoch is the first one that
                            //     will get the receipt through the state sync.
                            let num_receipts = partial_encoded_chunk
                                .prev_outgoing_receipts
                                .iter()
                                .map(|x| x.0.clone())
                                .flatten()
                                .count();
                            if num_receipts > 0 {
                                assert_eq!(
                                    partial_encoded_chunk.header.shard_id(),
                                    source_shard_id
                                );
                                seen_heights_with_receipts
                                    .insert(partial_encoded_chunk.header.height_created());
                            } else {
                                assert_ne!(
                                    partial_encoded_chunk.header.shard_id(),
                                    source_shard_id
                                );
                            }
                            // Do not propagate any one parts, this will prevent any chunk from
                            //    being included in the block
                            return (NetworkResponses::NoResponse.into(), false);
                        }
                        if let NetworkRequests::StateRequestHeader {
                            shard_id,
                            sync_hash,
                            sync_prev_prev_hash,
                        } = msg
                        {
                            if sync_hold {
                                let srs = StateRequestStruct {
                                    shard_id: *shard_id,
                                    sync_hash: *sync_hash,
                                    sync_prev_prev_hash: Some(*sync_prev_prev_hash),
                                    part_id: None,
                                    peer_id: None,
                                };
                                if !seen_hashes_with_state
                                    .contains(&hash_func(&borsh::to_vec(&srs).unwrap()))
                                {
                                    seen_hashes_with_state
                                        .insert(hash_func(&borsh::to_vec(&srs).unwrap()));
                                    return (NetworkResponses::NoResponse.into(), false);
                                }
                            }
                        }
                        if let NetworkRequests::StateRequestPart {
                            shard_id,
                            sync_hash,
                            sync_prev_prev_hash,
                            part_id,
                        } = msg
                        {
                            if sync_hold {
                                let srs = StateRequestStruct {
                                    shard_id: *shard_id,
                                    sync_hash: *sync_hash,
                                    sync_prev_prev_hash: Some(*sync_prev_prev_hash),
                                    part_id: Some(*part_id),
                                    peer_id: None,
                                };
                                if !seen_hashes_with_state
                                    .contains(&hash_func(&borsh::to_vec(&srs).unwrap()))
                                {
                                    seen_hashes_with_state
                                        .insert(hash_func(&borsh::to_vec(&srs).unwrap()));
                                    return (NetworkResponses::NoResponse.into(), false);
                                }
                            }
                        }
                    }
                    ReceiptsSyncPhases::VerifyingOutgoingReceipts => {
                        for height in send + 2..=wait_till {
                            println!(
                                "checking height {:?} out of {:?}, result = {:?}",
                                height,
                                wait_till,
                                seen_heights_with_receipts.contains(&height)
                            );
                            if !sync_hold {
                                // If we don't delay the state, all heights should contain the same receipts
                                assert!(seen_heights_with_receipts.contains(&height));
                            }
                        }
                        *phase = ReceiptsSyncPhases::WaitingForValidate;
                    }
                    ReceiptsSyncPhases::WaitingForValidate => {
                        // This block now contains a chunk with the transaction sent above.
                        if let NetworkRequests::Block { block } = msg {
                            assert!(block.header().height() >= wait_till);
                            assert!(block.header().height() <= wait_till + 20);
                            if block.header().height() == wait_till + 20 {
                                System::current().stop();
                            }
                            if block.header().height() == wait_till + 10 {
                                for i in 0..16 {
                                    let actor = connectors1.write()[i].view_client_actor.send(
                                        Query::new(
                                            BlockReference::latest(),
                                            QueryRequest::ViewAccount {
                                                account_id: account_to.clone(),
                                            },
                                        )
                                        .with_span_context(),
                                    );
                                    let actor = actor.then(move |res| {
                                        let res_inner = res.unwrap();
                                        if let Ok(query_response) = res_inner {
                                            if let ViewAccount(view_account_result) =
                                                query_response.kind
                                            {
                                                assert_eq!(view_account_result.amount, 1111);
                                            }
                                        }
                                        future::ready(())
                                    });
                                    actix::spawn(actor);
                                }
                            }
                        }
                    }
                };
                (NetworkResponses::NoResponse.into(), true)
            }),
        );
        *connectors.write() = conn;
        let mut max_wait_ms = 240000;
        if sync_hold {
            max_wait_ms *= TEST_STATE_SYNC_TIMEOUT as u64;
        }

        near_network::test_utils::wait_or_panic(max_wait_ms);
    });
}

enum RandomSinglePartPhases {
    WaitingForFirstBlock,
    WaitingForThirdEpoch,
    WaitingForSixEpoch,
}

/// Verifies that fetching of random parts works properly by issuing transactions during the
/// third epoch, and then making sure that the balances are correct for the next three epochs.
/// If random one parts fetched during the epoch preceding the epoch a block producer is
/// assigned to were to have incorrect receipts, the balances in the fourth epoch would have
/// been incorrect due to wrong receipts applied during the third epoch.
#[test]
fn ultra_slow_test_catchup_random_single_part_sync() {
    test_catchup_random_single_part_sync_common(false, false, 13)
}

// Same test as `test_catchup_random_single_part_sync`, but skips the chunks on height 14 and 15
// It causes all the receipts to be applied only on height 16, which is the next epoch.
// It tests that the incoming receipts are property synced through epochs
#[test]
fn ultra_slow_test_catchup_random_single_part_sync_skip_15() {
    test_catchup_random_single_part_sync_common(true, false, 13)
}

#[test]
fn ultra_slow_test_catchup_random_single_part_sync_send_15() {
    test_catchup_random_single_part_sync_common(false, false, 15)
}

// Make sure that transactions are at least applied.
#[test]
fn ultra_slow_test_catchup_random_single_part_sync_non_zero_amounts() {
    test_catchup_random_single_part_sync_common(false, true, 13)
}

// Use another height to send txs.
#[test]
fn ultra_slow_test_catchup_random_single_part_sync_height_6() {
    test_catchup_random_single_part_sync_common(false, false, 6)
}

fn test_catchup_random_single_part_sync_common(skip_15: bool, non_zero: bool, height: u64) {
    init_integration_logger();
    run_actix(async move {
        let connectors: Arc<RwLock<Vec<ActorHandlesForTesting>>> = Arc::new(RwLock::new(vec![]));

        let (vs, key_pairs) = get_validators_and_key_pairs();
        let vs = vs.validator_groups(2);
        let validators = vs.all_block_producers().cloned().collect::<Vec<_>>();
        let phase = Arc::new(RwLock::new(RandomSinglePartPhases::WaitingForFirstBlock));
        let seen_heights_same_block = Arc::new(RwLock::new(HashSet::<CryptoHash>::new()));

        let amounts = Arc::new(RwLock::new(HashMap::new()));

        let check_amount =
            move |amounts: Arc<RwLock<HashMap<_, _, _>>>, account_id: AccountId, amount: u128| {
                match amounts.write().entry(account_id) {
                    Entry::Occupied(entry) => {
                        println!("OCCUPIED {:?}", entry);
                        assert_eq!(*entry.get(), amount);
                    }
                    Entry::Vacant(entry) => {
                        println!("VACANT {:?}", entry);
                        if non_zero {
                            assert_ne!(amount % 100, 0);
                        } else {
                            assert_eq!(amount % 100, 0);
                        }
                        entry.insert(amount);
                    }
                }
            };

        let connectors1 = connectors.clone();
        let (conn, _) = setup_mock_all_validators(
            Clock::real(),
            vs,
            key_pairs,
            true,
            6000,
            false,
            false,
            5,
            true,
            vec![false; validators.len()],
            false,
            None,
            Box::new(move |_, _account_id: _, msg: &PeerManagerMessageRequest| {
                let msg = msg.as_network_requests_ref();
                let mut seen_heights_same_block = seen_heights_same_block.write();
                let mut phase = phase.write();
                match *phase {
                    RandomSinglePartPhases::WaitingForFirstBlock => {
                        if let NetworkRequests::Block { block } = msg {
                            assert_eq!(block.header().height(), 1);
                            *phase = RandomSinglePartPhases::WaitingForThirdEpoch;
                        }
                    }
                    RandomSinglePartPhases::WaitingForThirdEpoch => {
                        if let NetworkRequests::Block { block } = msg {
                            if block.header().height() == 1 {
                                return (NetworkResponses::NoResponse.into(), false);
                            }
                            assert!(block.header().height() >= 2);
                            assert!(block.header().height() <= height);
                            let mut tx_count = 0;
                            if block.header().height() == height && block.header().height() >= 2 {
                                for (i, validator1) in validators.iter().enumerate() {
                                    for (j, validator2) in validators.iter().enumerate() {
                                        let mut amount = (((i + j + 17) * 701) % 42 + 1) as u128;
                                        if non_zero {
                                            if i > j {
                                                amount = 2;
                                            } else {
                                                amount = 1;
                                            }
                                        }
                                        println!(
                                            "VALUES {:?} {:?} {:?}",
                                            validator1.to_string(),
                                            validator2.to_string(),
                                            amount
                                        );
                                        for conn in 0..validators.len() {
                                            send_tx(
                                                &connectors1.write()[conn].rpc_handler_actor,
                                                validator1.clone(),
                                                validator2.clone(),
                                                amount,
                                                (12345 + tx_count) as u64,
                                                *block.header().prev_hash(),
                                            );
                                        }
                                        tx_count += 1;
                                    }
                                }
                                *phase = RandomSinglePartPhases::WaitingForSixEpoch;
                                assert_eq!(tx_count, 16 * 16);
                            }
                        }
                    }
                    RandomSinglePartPhases::WaitingForSixEpoch => {
                        if let NetworkRequests::Block { block } = msg {
                            assert!(block.header().height() >= height);
                            assert!(block.header().height() <= 32);
                            let check_height = if skip_15 { 28 } else { 26 };
                            if block.header().height() >= check_height {
                                println!("BLOCK HEIGHT {:?}", block.header().height());
                                for i in 0..16 {
                                    for j in 0..16 {
                                        let amounts1 = amounts.clone();
                                        let validator = validators[j].clone();
                                        let actor = connectors1.write()[i].view_client_actor.send(
                                            Query::new(
                                                BlockReference::latest(),
                                                QueryRequest::ViewAccount {
                                                    account_id: validators[j].clone(),
                                                },
                                            )
                                            .with_span_context(),
                                        );
                                        let actor = actor.then(move |res| {
                                            let res_inner = res.unwrap();
                                            if let Ok(query_response) = res_inner {
                                                if let ViewAccount(view_account_result) =
                                                    query_response.kind
                                                {
                                                    check_amount(
                                                        amounts1,
                                                        validator,
                                                        view_account_result.amount,
                                                    );
                                                }
                                            }
                                            future::ready(())
                                        });
                                        actix::spawn(actor);
                                    }
                                }
                            }
                            if block.header().height() == 32 {
                                println!(
                                    "SEEN HEIGHTS SAME BLOCK {:?}",
                                    seen_heights_same_block.len()
                                );
                                assert_eq!(seen_heights_same_block.len(), 1);
                                let amounts1 = amounts.clone();
                                for flat_validator in &validators {
                                    match amounts1.write().entry(flat_validator.clone()) {
                                        Entry::Occupied(_) => {
                                            continue;
                                        }
                                        Entry::Vacant(entry) => {
                                            println!(
                                                "VALIDATOR = {:?}, ENTRY = {:?}",
                                                flat_validator, entry
                                            );
                                            assert!(false);
                                        }
                                    };
                                }
                                System::current().stop();
                            }
                        }
                        if let NetworkRequests::PartialEncodedChunkMessage {
                            partial_encoded_chunk,
                            ..
                        } = msg
                        {
                            if partial_encoded_chunk.header.height_created() == 22 {
                                seen_heights_same_block
                                    .insert(*partial_encoded_chunk.header.prev_block_hash());
                            }
                            if skip_15 {
                                if partial_encoded_chunk.header.height_created() == 14
                                    || partial_encoded_chunk.header.height_created() == 15
                                {
                                    return (NetworkResponses::NoResponse.into(), false);
                                }
                            }
                        }
                    }
                };
                (NetworkResponses::NoResponse.into(), true)
            }),
        );
        *connectors.write() = conn;

        near_network::test_utils::wait_or_panic(480000);
    });
}

/// Makes sure that 24 consecutive blocks are produced by 12 validators split into three epochs.
/// For extra coverage doesn't allow block propagation of some heights (and expects the blocks
/// to be skipped)
/// This test would fail if at any point validators got stuck with state sync, or block
/// production stalled for any other reason.
#[test]
fn ultra_slow_test_catchup_sanity_blocks_produced() {
    init_integration_logger();
    run_actix(async move {
        let connectors: Arc<RwLock<Vec<ActorHandlesForTesting>>> = Arc::new(RwLock::new(vec![]));

        let heights = Arc::new(RwLock::new(HashMap::new()));
        let heights1 = heights;

        let check_height = move |hash: CryptoHash, height| match heights1.write().entry(hash) {
            Entry::Occupied(entry) => {
                assert_eq!(*entry.get(), height);
            }
            Entry::Vacant(entry) => {
                entry.insert(height);
            }
        };

        let (vs, key_pairs) = get_validators_and_key_pairs();
        let vs = vs.validator_groups(2);
        let archive = vec![false; vs.all_block_producers().count()];

        let (conn, _) = setup_mock_all_validators(
            Clock::real(),
            vs,
            key_pairs,
            true,
            2000,
            false,
            false,
            5,
            true,
            archive,
            false,
            None,
            Box::new(move |_, _account_id: _, msg: &PeerManagerMessageRequest| {
                let msg = msg.as_network_requests_ref();
                let propagate = if let NetworkRequests::Block { block } = msg {
                    check_height(*block.hash(), block.header().height());

                    if block.header().height() % 10 == 5 {
                        check_height(*block.header().prev_hash(), block.header().height() - 2);
                    } else {
                        check_height(*block.header().prev_hash(), block.header().height() - 1);
                    }

                    if block.header().height() >= 25 {
                        System::current().stop();
                    }

                    // Do not propagate blocks at heights %10=4
                    block.header().height() % 10 != 4
                } else {
                    true
                };

                (NetworkResponses::NoResponse.into(), propagate)
            }),
        );
        *connectors.write() = conn;

        near_network::test_utils::wait_or_panic(60000);
    });
}

#[test]
fn ultra_slow_test_all_chunks_accepted_1000() {
    test_all_chunks_accepted_common(1000, 3000, 5)
}

#[test]
fn ultra_slow_test_all_chunks_accepted_1000_slow() {
    test_all_chunks_accepted_common(1000, 6000, 5)
}

#[test]
fn ultra_slow_test_all_chunks_accepted_1000_rare_epoch_changing() {
    test_all_chunks_accepted_common(1000, 1500, 100)
}

fn test_all_chunks_accepted_common(
    last_height: BlockHeight,
    block_prod_time: u64,
    epoch_length: BlockHeightDelta,
) {
    init_integration_logger();
    run_actix(async move {
        let connectors: Arc<RwLock<Vec<ActorHandlesForTesting>>> = Arc::new(RwLock::new(vec![]));

        let (vs, key_pairs) = get_validators_and_key_pairs();
        let archive = vec![false; vs.all_block_producers().count()];

        let verbose = false;

        let seen_chunk_same_sender =
            Arc::new(RwLock::new(HashSet::<(AccountId, u64, ShardId)>::new()));
        let requested = Arc::new(RwLock::new(HashSet::<(AccountId, Vec<u64>, ChunkHash)>::new()));
        let responded = Arc::new(RwLock::new(HashSet::<(CryptoHash, Vec<u64>, ChunkHash)>::new()));

        let (conn, _) = setup_mock_all_validators(
            Clock::real(),
            vs,
            key_pairs,
            true,
            block_prod_time,
            false,
            false,
            epoch_length,
            true,
            archive,
            false,
            None,
            Box::new(move |_, sender_account_id: AccountId, msg: &PeerManagerMessageRequest| {
                let msg = msg.as_network_requests_ref();
                let mut seen_chunk_same_sender = seen_chunk_same_sender.write();
                let mut requested = requested.write();
                let mut responded = responded.write();
                if let NetworkRequests::PartialEncodedChunkMessage {
                    account_id,
                    partial_encoded_chunk,
                } = msg
                {
                    let header = &partial_encoded_chunk.header;
                    if seen_chunk_same_sender.contains(&(
                        account_id.clone(),
                        header.height_created(),
                        header.shard_id(),
                    )) {
                        println!("=== SAME CHUNK AGAIN!");
                        assert!(false);
                    };
                    seen_chunk_same_sender.insert((
                        account_id.clone(),
                        header.height_created(),
                        header.shard_id(),
                    ));
                }
                if let NetworkRequests::PartialEncodedChunkRequest { request, .. } = msg {
                    if verbose {
                        if requested.contains(&(
                            sender_account_id.clone(),
                            request.part_ords.clone(),
                            request.chunk_hash.clone(),
                        )) {
                            println!("=== SAME REQUEST AGAIN!");
                        };
                        requested.insert((
                            sender_account_id,
                            request.part_ords.clone(),
                            request.chunk_hash.clone(),
                        ));
                    }
                }
                if let NetworkRequests::PartialEncodedChunkResponse { route_back, response } = msg {
                    if verbose {
                        if responded.contains(&(
                            *route_back,
                            response.parts.iter().map(|x| x.part_ord).collect(),
                            response.chunk_hash.clone(),
                        )) {
                            println!("=== SAME RESPONSE AGAIN!");
                        }
                        responded.insert((
                            *route_back,
                            response.parts.iter().map(|x| x.part_ord).collect(),
                            response.chunk_hash.clone(),
                        ));
                    }
                }
                if let NetworkRequests::Block { block } = msg {
                    // There is no chunks at height 1
                    if block.header().height() > 1 {
                        if block.header().height() % epoch_length != 1 {
                            if block.header().chunks_included() != 4 {
                                println!(
                                    "BLOCK WITH {:?} CHUNKS, {:?}",
                                    block.header().chunks_included(),
                                    block
                                );
                                assert!(false);
                            }
                        }
                        if block.header().height() == last_height {
                            System::current().stop();
                        }
                    }
                }
                (NetworkResponses::NoResponse.into(), true)
            }),
        );
        *connectors.write() = conn;
        let max_wait_ms = block_prod_time * last_height / 10 * 18 + 20000;

        near_network::test_utils::wait_or_panic(max_wait_ms);
    });
}
