// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{sync::*, time::Duration};

use api_version::{KvFormat, test_kv_format_impl};
use cdc::{Task, Validate, metrics::CDC_RESOLVED_TS_ADVANCE_METHOD};
use concurrency_manager::ConcurrencyManager;
use futures::{SinkExt, executor::block_on};
use grpcio::WriteFlags;
use kvproto::{cdcpb::*, kvrpcpb::*};
use pd_client::PdClient;
use raft::eraftpb::MessageType;
use test_raftstore::*;
use tikv::server::DEFAULT_CLUSTER_ID;
use tikv_util::{HandyRwLock, config::ReadableDuration};
use txn_types::{Key, Lock, LockType, TimeStamp};

use crate::{TestSuite, TestSuiteBuilder, new_event_feed, new_event_feed_v2};

#[test]
fn test_cdc_basic() {
    test_kv_format_impl!(test_cdc_basic_impl<ApiV1 ApiV2>);
}

fn test_cdc_basic_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let event = receive_event(false);
    event.events.into_iter().for_each(|e| {
        match e.event.unwrap() {
            // Even if there is no write,
            // it should always outputs an Initialized event.
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        }
    });

    // Sleep a while to make sure the stream is registered.
    sleep_ms(1000);
    // There must be a delegate.
    let scheduler = suite.endpoints.values().next().unwrap().scheduler();
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(|delegate| {
                let d = delegate.unwrap();
                assert_eq!(d.downstreams().len(), 1);
            }),
        )))
        .unwrap();

    // If tikv enable ApiV2, txn key needs to start with 'x';
    let (k, v) = ("xkey1".to_owned(), "value".to_owned());
    // Prewrite
    let start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone().into_bytes();
    mutation.value = v.into_bytes();
    suite.must_kv_prewrite(1, vec![mutation], k.clone().into_bytes(), start_ts);
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1, "{:?}", events);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(entries) => {
            assert_eq!(entries.entries.len(), 1);
            assert_eq!(entries.entries[0].get_type(), EventLogType::Prewrite);
        }
        other => panic!("unknown event {:?}", other),
    }

    let mut counter = 0;
    loop {
        // Even if there is no write,
        // resolved ts should be advanced regularly.
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            assert_ne!(0, resolved_ts.ts);
            counter += 1;
        }
        if counter > 5 {
            break;
        }
    }
    // Commit
    let commit_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k.into_bytes()], start_ts, commit_ts);
    let mut event = receive_event(false);
    let mut events = event.take_events();
    assert_eq!(events.len(), 1, "{:?}", event);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(entries) => {
            assert_eq!(entries.entries.len(), 1);
            assert_eq!(entries.entries[0].get_type(), EventLogType::Commit);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Split region 1
    let region1 = suite.cluster.get_region(&[]);
    suite.cluster.must_split(&region1, b"xkey2");
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_epoch_not_match(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }
    // The delegate must be removed.
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(|delegate| {
                assert!(delegate.is_none());
            }),
        )))
        .unwrap();

    // request again.
    let req = suite.new_changedata_request(1);
    let (mut req_tx, resp_rx) = suite.get_region_cdc_client(1).event_feed().unwrap();
    event_feed_wrap.replace(Some(resp_rx));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    // Sleep a while to make sure the stream is registered.
    sleep_ms(200);
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(|delegate| {
                let d = delegate.unwrap();
                assert_eq!(d.downstreams().len(), 1);
            }),
        )))
        .unwrap();

    // Drop stream and cancel its server streaming.
    event_feed_wrap.replace(None);
    // Sleep a while to make sure the stream is deregistered.
    sleep_ms(200);
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(|delegate| {
                assert!(delegate.is_none());
            }),
        )))
        .unwrap();

    // Stale region epoch.
    let mut req = suite.new_changedata_request(1);
    req.set_region_epoch(Default::default()); // Zero region epoch.
    let (mut req_tx, resp_rx) = suite.get_region_cdc_client(1).event_feed().unwrap();
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    event_feed_wrap.replace(Some(resp_rx));
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_epoch_not_match(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }

    suite.stop();
}

#[test]
fn test_cdc_rawkv_basic() {
    let mut suite = TestSuite::new(1, ApiVersion::V2);

    // rawkv
    let mut req = suite.new_changedata_request(1);
    req.set_kv_api(ChangeDataRequestKvApi::RawKv);
    let (mut req_tx, _event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    let event = receive_event(false);
    event.events.into_iter().for_each(|e| {
        match e.event.unwrap() {
            // Even if there is no write,
            // it should always outputs an Initialized event.
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        }
    });
    // Sleep a while to make sure the stream is registered.
    sleep_ms(1000);
    // There must be a delegate.
    let scheduler = suite.endpoints.values().next().unwrap().scheduler();
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(|delegate| {
                let d = delegate.unwrap();
                assert_eq!(d.downstreams().len(), 1);
            }),
        )))
        .unwrap();

    // If tikv enable ApiV2, raw key needs to start with 'r';
    let (k, v) = (b"rkey1".to_vec(), b"value".to_vec());
    suite.must_kv_put(1, k, v);
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1, "{:?}", events);

    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(entries) => {
            assert_eq!(entries.entries.len(), 1);
            assert_eq!(entries.entries[0].get_type(), EventLogType::Committed);
        }
        other => panic!("unknown event {:?}", other),
    }

    // boundary case
    let (k, v) = (b"r\0".to_vec(), b"value".to_vec());
    suite.must_kv_put(1, k, v);
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1, "{:?}", events);

    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(entries) => {
            assert_eq!(entries.entries.len(), 1);
            assert_eq!(entries.entries[0].get_type(), EventLogType::Committed);
        }
        other => panic!("unknown event {:?}", other),
    }
}

#[test]
fn test_cdc_not_leader() {
    test_kv_format_impl!(test_cdc_not_leader_impl<ApiV1 ApiV2>);
}

fn test_cdc_not_leader_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(3, F::TAG);

    let leader = suite.cluster.leader_of_region(1).unwrap();
    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    // Make sure region 1 is registered.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    // Sleep a while to make sure the stream is registered.
    sleep_ms(1000);
    // There must be a delegate.
    let scheduler = suite
        .endpoints
        .get(&leader.get_store_id())
        .unwrap()
        .scheduler();
    let (tx, rx) = mpsc::channel();
    let tx_ = tx.clone();
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(move |delegate| {
                let d = delegate.unwrap();
                assert_eq!(d.downstreams().len(), 1);
                tx_.send(()).unwrap();
            }),
        )))
        .unwrap();
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        suite
            .obs
            .get(&leader.get_store_id())
            .unwrap()
            .is_subscribed(1)
            .is_some()
    );

    // Transfer leader.
    let peer = suite
        .cluster
        .get_region(&[])
        .take_peers()
        .into_iter()
        .find(|p| *p != leader)
        .unwrap();
    suite.cluster.must_transfer_leader(1, peer.clone());
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_not_leader(), "{:?}", err);
            assert_eq!(*err.get_not_leader().get_leader(), peer, "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }
    assert!(
        suite
            .obs
            .get(&leader.get_store_id())
            .unwrap()
            .is_subscribed(1)
            .is_none()
    );

    // Sleep a while to make sure the stream is deregistered.
    sleep_ms(200);
    scheduler
        .schedule(Task::Validate(Validate::Region(
            1,
            Box::new(move |delegate| {
                assert!(delegate.is_none());
                tx.send(()).unwrap();
            }),
        )))
        .unwrap();
    rx.recv_timeout(Duration::from_millis(200)).unwrap();

    // Try to subscribe again.
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    // Should failed with not leader error.
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_not_leader(), "{:?}", err);
            assert_eq!(*err.get_not_leader().get_leader(), peer, "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }
    assert!(
        suite
            .obs
            .get(&leader.get_store_id())
            .unwrap()
            .is_subscribed(1)
            .is_none()
    );

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_cluster_id_mismatch() {
    test_kv_format_impl!(test_cdc_cluster_id_mismatch_impl<ApiV1 ApiV2>);
}

fn test_cdc_cluster_id_mismatch_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(3, F::TAG);

    // Send request with mismatched cluster id.
    let mut req = suite.new_changedata_request(1);
    req.mut_header().set_ticdc_version("5.3.0".into());
    req.mut_header().set_cluster_id(DEFAULT_CLUSTER_ID + 1);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();

    // Assert mismatch.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_cluster_id_mismatch(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Low version request.
    req.mut_header().set_ticdc_version("4.0.8".into());
    req.mut_header().set_cluster_id(DEFAULT_CLUSTER_ID + 1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);

    // Should without error.
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_stale_epoch_after_region_ready() {
    test_kv_format_impl!(test_cdc_stale_epoch_after_region_ready_impl<ApiV1 ApiV2>);
}

fn test_cdc_stale_epoch_after_region_ready_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(3, F::TAG);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    // Make sure region 1 is registered.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    let mut req = suite.new_changedata_request(1);
    req.set_region_epoch(Default::default()); // zero epoch is always stale.
    let (mut req_tx, resp_rx) = suite.get_region_cdc_client(1).event_feed().unwrap();
    let _resp_rx = event_feed_wrap.replace(Some(resp_rx));
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    // Must receive epoch not match error.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_epoch_not_match(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }

    req.set_region_epoch(suite.get_context(1).take_region_epoch());
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    // Must receive epoch not match error.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        Event_oneof_event::Error(err) => {
            assert!(err.has_epoch_not_match(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Cancel event feed before finishing test.
    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_scan() {
    test_kv_format_impl!(test_cdc_scan_impl<ApiV1 ApiV2>);
}

fn test_cdc_scan_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(3, F::TAG);

    let (k, v) = (b"xkey1".to_vec(), b"value".to_vec());
    // Prewrite
    let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v.clone();
    suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts1);
    // Commit
    let commit_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k.clone()], start_ts1, commit_ts1);

    // Prewrite again
    let start_ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v.clone();
    suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts2);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    if events.len() == 1 {
        events.extend(receive_event(false).events.into_iter());
    }
    assert_eq!(events.len(), 2, "{:?}", events);
    match events.remove(0).event.unwrap() {
        // Batch size is set to 2.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 2, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Prewrite, "{:?}", es);
            assert_eq!(e.start_ts, start_ts2.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, 0, "{:?}", es);
            assert_eq!(e.key, k, "{:?}", es);
            assert_eq!(e.value, v, "{:?}", es);
            let e = &es.entries[1];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.start_ts, start_ts1.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, commit_ts1.into_inner(), "{:?}", es);
            assert_eq!(e.key, k, "{:?}", es);
            assert_eq!(e.value, v, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    match events.pop().unwrap().event.unwrap() {
        // Then it outputs Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    // checkpoint_ts = 6;
    let checkpoint_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    // Commit = 7;
    let commit_ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k.clone()], start_ts2, commit_ts2);
    // Prewrite delete
    // Start = 8;
    let start_ts3 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Del);
    mutation.key = k.clone();
    suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts3);

    let mut req = suite.new_changedata_request(1);
    req.checkpoint_ts = checkpoint_ts.into_inner();
    let (mut req_tx, resp_rx) = suite.get_region_cdc_client(1).event_feed().unwrap();
    event_feed_wrap.replace(Some(resp_rx));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    if events.len() == 1 {
        events.extend(receive_event(false).events.to_vec());
    }
    assert_eq!(events.len(), 2, "{:?}", events);
    match events.remove(0).event.unwrap() {
        // Batch size is set to 2.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 2, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Prewrite, "{:?}", es);
            assert_eq!(e.get_op_type(), EventRowOpType::Delete, "{:?}", es);
            assert_eq!(e.start_ts, start_ts3.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, 0, "{:?}", es);
            assert_eq!(e.key, k, "{:?}", es);
            assert!(e.value.is_empty(), "{:?}", es);
            let e = &es.entries[1];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.get_op_type(), EventRowOpType::Put, "{:?}", es);
            assert_eq!(e.start_ts, start_ts2.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, commit_ts2.into_inner(), "{:?}", es);
            assert_eq!(e.key, k, "{:?}", es);
            assert_eq!(e.value, v, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    assert_eq!(events.len(), 1, "{:?}", events);
    match events.pop().unwrap().event.unwrap() {
        // Then it outputs Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_rawkv_scan() {
    let mut suite = TestSuite::new(3, ApiVersion::V2);

    let (k1, v1) = (b"rkey1".to_vec(), b"value1".to_vec());
    suite.must_kv_put(1, k1, v1);

    let (k2, v2) = (b"rkey2".to_vec(), b"value2".to_vec());
    suite.must_kv_put(1, k2, v2);

    let ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.flush_causal_timestamp_for_region(1);

    let (k3, v3) = (b"rkey3".to_vec(), b"value3".to_vec());
    suite.must_kv_put(1, k3.clone(), v3.clone());

    let (k4, v4) = (b"rkey4".to_vec(), b"value4".to_vec());
    suite.must_kv_put(1, k4.clone(), v4.clone());

    let mut req = suite.new_changedata_request(1);
    req.set_kv_api(ChangeDataRequestKvApi::RawKv);
    req.set_checkpoint_ts(ts.into_inner());
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    if events.len() == 1 {
        events.extend(receive_event(false).events.into_iter());
    }
    assert_eq!(events.len(), 2, "{:?}", events);

    match events.remove(0).event.unwrap() {
        // Batch size is set to 3.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 2, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.key, k3, "{:?}", es);
            assert_eq!(e.value, v3, "{:?}", es);

            let e = &es.entries[1];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.key, k4, "{:?}", es);
            assert_eq!(e.value, v4, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    match events.pop().unwrap().event.unwrap() {
        // Then it outputs Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_tso_failure() {
    test_kv_format_impl!(test_cdc_tso_failure_impl<ApiV1 ApiV2>);
}

fn test_cdc_tso_failure_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(3, F::TAG);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    // Make sure region 1 is registered.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    suite.cluster.pd_client.trigger_tso_failure();

    // Make sure resolved ts can be advanced normally even with few tso failures.
    let mut counter = 0;
    let mut previous_ts = 0;
    loop {
        // Even if there is no write,
        // resolved ts should be advanced regularly.
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            assert!(resolved_ts.ts >= previous_ts);
            assert_eq!(resolved_ts.regions, vec![1]);
            previous_ts = resolved_ts.ts;
            counter += 1;
        }
        if counter > 5 {
            break;
        }
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_region_split() {
    let cluster = new_server_cluster(1, 1);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();

    let region = suite.cluster.get_region(&[]);
    let mut req = suite.new_changedata_request(region.get_id());
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(region.get_id()));
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    // Make sure region 1 is registered.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    // Split region.
    suite.cluster.must_split(&region, b"k0");
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_epoch_not_match(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }
    // Try to subscribe region again.
    let region = suite.cluster.get_region(b"k0");
    // Ensure it is the previous region.
    assert_eq!(req.get_region_id(), region.get_id());
    req.set_region_epoch(region.get_region_epoch().clone());
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Try to subscribe region again.
    let region1 = suite.cluster.get_region(&[]);
    req.region_id = region1.get_id();
    req.set_region_epoch(region1.get_region_epoch().clone());
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Make sure resolved ts can be advanced normally.
    let mut counter = 0;
    let mut previous_ts = 0;
    loop {
        // Even if there is no write,
        // resolved ts should be advanced regularly.
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            assert!(resolved_ts.ts >= previous_ts);
            assert!(
                resolved_ts.regions == vec![region.id, region1.id]
                    || resolved_ts.regions == vec![region1.id, region.id]
            );
            previous_ts = resolved_ts.ts;
            counter += 1;
        }
        if counter > 5 {
            break;
        }
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_duplicate_subscribe() {
    test_kv_format_impl!(test_duplicate_subscribe_impl<ApiV1 ApiV2>);
}

fn test_duplicate_subscribe_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(3, F::TAG);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    // Make sure region 1 is registered.
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    // Try to subscribe again.
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1);
    // Should receive duplicate request error.
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Error(err) => {
            assert!(err.has_duplicate_request(), "{:?}", err);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_batch_size_limit() {
    test_kv_format_impl!(test_cdc_batch_size_limit_impl<ApiV1 ApiV2>);
}

fn test_cdc_batch_size_limit_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);

    // Prewrite
    let start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Put);
    m1.key = k1.clone();
    m1.value = vec![0; 6 * 1024 * 1024];
    let mut m2 = Mutation::default();
    let k2 = b"xk2".to_vec();
    m2.set_op(Op::Put);
    m2.key = k2.clone();
    m2.value = b"v2".to_vec();
    suite.must_kv_prewrite(1, vec![m1, m2], k1.clone(), start_ts);
    // Commit
    let commit_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1, k2], start_ts, commit_ts);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1, "{:?}", events.len());
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", e.get_type());
            assert_eq!(e.key, b"xk1", "{:?}", e.key);
        }
        other => panic!("unknown event {:?}", other),
    }
    // For the rest 2 events, Committed and Initialized.
    let mut entries = vec![];
    while entries.len() < 2 {
        match receive_event(false).events.remove(0).event.unwrap() {
            Event_oneof_event::Entries(es) => {
                entries.extend(es.entries.into_iter());
            }
            other => panic!("unknown event {:?}", other),
        }
    }
    assert_eq!(entries.len(), 2, "{:?}", entries);
    let e = &entries[0];
    assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", e.get_type());
    assert_eq!(e.key, b"xk2", "{:?}", e.key);
    let e = &entries[1];
    assert_eq!(
        e.get_type(),
        EventLogType::Initialized,
        "{:?}",
        e.get_type()
    );

    // Prewrite
    let start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut m3 = Mutation::default();
    let k3 = b"xk3".to_vec();
    m3.set_op(Op::Put);
    m3.key = k3.clone();
    m3.value = vec![0; 7 * 1024 * 1024];
    let mut m4 = Mutation::default();
    let k4 = b"xk4".to_vec();
    m4.set_op(Op::Put);
    m4.key = k4;
    m4.value = b"v4".to_vec();
    suite.must_kv_prewrite(1, vec![m3, m4], k3, start_ts);

    let mut events = receive_event(false).events.to_vec();
    assert_eq!(events.len(), 1, "{:?}", events);
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let mut entries = es.take_entries().into_vec();
            assert_eq!(entries.len(), 2);
            entries.sort_by(|a, b| a.key.cmp(&b.key));

            let e = &entries[0];
            assert_eq!(e.get_type(), EventLogType::Prewrite, "{:?}", e.get_type());
            assert_eq!(e.key, b"xk3", "{:?}", e.key);
            let e = &entries[1];
            assert_eq!(e.get_type(), EventLogType::Prewrite, "{:?}", e.get_type());
            assert_eq!(e.key, b"xk4", "{:?}", e.key);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_old_value_basic() {
    test_kv_format_impl!(test_old_value_basic_impl<ApiV1 ApiV2>);
}

fn test_old_value_basic_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req.clone(), WriteFlags::default()))).unwrap();
    sleep_ms(1000);

    // Insert value
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Insert);
    m1.key = k1.clone();
    m1.value = b"v1".to_vec();
    let ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m1], k1.clone(), ts1);
    let ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1.clone()], ts1, ts2);
    // Rollback
    let mut m2 = Mutation::default();
    m2.set_op(Op::Put);
    m2.key = k1.clone();
    m2.value = b"v2".to_vec();
    let ts3 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m2], k1.clone(), ts3);
    suite.must_kv_rollback(1, vec![k1.clone()], ts3);
    // Update value
    let mut m3 = Mutation::default();
    m3.set_op(Op::Put);
    m3.key = k1.clone();
    m3.value = vec![b'3'; 5120];
    let ts4 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m3], k1.clone(), ts4);
    let ts5 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1.clone()], ts4, ts5);
    // Lock
    let mut m4 = Mutation::default();
    m4.set_op(Op::Lock);
    m4.key = k1.clone();
    let ts6 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m4], k1.clone(), ts6);
    let ts7 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1.clone()], ts6, ts7);
    // Delete value and rollback
    let mut m5 = Mutation::default();
    m5.set_op(Op::Del);
    m5.key = k1.clone();
    let ts8 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m5], k1.clone(), ts8);
    suite.must_kv_rollback(1, vec![k1.clone()], ts8);
    // Update value
    let mut m6 = Mutation::default();
    m6.set_op(Op::Put);
    m6.key = k1.clone();
    m6.value = b"v6".to_vec();
    let ts9 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let ts10 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m6], k1.clone(), ts10);
    let ts11 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1.clone()], ts10, ts11);
    // Delete value in pessimistic txn.
    // In pessimistic txn, CDC must use for_update_ts to read the old value.
    let mut m7 = Mutation::default();
    m7.set_op(Op::PessimisticLock);
    m7.key = k1.clone();
    let ts12 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_acquire_pessimistic_lock(1, vec![m7.clone()], k1.clone(), ts9, ts12);
    m7.set_op(Op::Del);
    suite.must_kv_pessimistic_prewrite(1, vec![m7], k1.clone(), ts9, ts12);
    let ts13 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1.clone()], ts9, ts13);
    // Insert value again
    let mut m8 = Mutation::default();
    m8.set_op(Op::Insert);
    m8.key = k1.clone();
    m8.value = b"v1".to_vec();
    let ts14 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m8], k1, ts14);

    let mut event_count = 0;
    loop {
        let events = receive_event(false).events.to_vec();
        for event in events.into_iter() {
            match event.event.unwrap() {
                Event_oneof_event::Entries(mut es) => {
                    for row in es.take_entries().to_vec() {
                        if row.get_type() == EventLogType::Prewrite {
                            if row.get_start_ts() == ts3.into_inner()
                                || row.get_start_ts() == ts4.into_inner()
                            {
                                assert_eq!(row.get_old_value(), b"v1", "{:?}", row);
                                event_count += 1;
                            } else if row.get_start_ts() == ts8.into_inner() {
                                assert_eq!(
                                    row.get_old_value(),
                                    vec![b'3'; 5120].as_slice(),
                                    "{:?}",
                                    row
                                );
                                event_count += 1;
                            } else if row.get_start_ts() == ts9.into_inner() {
                                assert_eq!(row.get_old_value(), b"v6", "{:?}", row);
                                event_count += 1;
                            }
                        }
                    }
                }
                other => panic!("unknown event {:?}", other),
            }
        }
        if event_count >= 4 {
            break;
        }
    }

    let (mut req_tx, resp_rx) = suite.get_region_cdc_client(1).event_feed().unwrap();
    event_feed_wrap.replace(Some(resp_rx));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut event_count = 0;
    loop {
        let event = receive_event(false);
        for e in event.events.into_iter() {
            match e.event.unwrap() {
                Event_oneof_event::Entries(mut es) => {
                    for row in es.take_entries().to_vec() {
                        if row.get_type() == EventLogType::Committed
                            && row.get_start_ts() == ts1.into_inner()
                        {
                            assert_eq!(row.get_old_value(), b"");
                            event_count += 1;
                        } else if row.get_type() == EventLogType::Committed
                            && row.get_start_ts() == ts4.into_inner()
                        {
                            assert_eq!(row.get_old_value(), b"v1");
                            event_count += 1;
                        } else if row.get_type() == EventLogType::Prewrite
                            && row.get_start_ts() == ts14.into_inner()
                        {
                            assert_eq!(row.get_old_value(), b"");
                            event_count += 1;
                        }
                    }
                }
                other => panic!("unknown event {:?}", other),
            }
        }
        if event_count >= 3 {
            break;
        }
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_old_value_multi_changefeeds() {
    test_kv_format_impl!(test_old_value_multi_changefeeds_impl<ApiV1 ApiV2>);
}

fn test_old_value_multi_changefeeds_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx_1, event_feed_wrap_1, receive_event_1) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx_1.send((req.clone(), WriteFlags::default()))).unwrap();

    req.set_extra_op(ExtraOp::Noop);
    let (mut req_tx_2, event_feed_wrap_2, receive_event_2) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx_2.send((req, WriteFlags::default()))).unwrap();
    sleep_ms(1000);

    // Insert value
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Put);
    m1.key = k1.clone();
    m1.value = b"v1".to_vec();
    let ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m1], k1.clone(), ts1);
    let ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1.clone()], ts1, ts2);

    // Update value
    let mut m2 = Mutation::default();
    m2.set_op(Op::Put);
    m2.key = k1.clone();
    m2.value = vec![b'3'; 5120];
    let ts3 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prewrite(1, vec![m2], k1.clone(), ts3);
    let ts4 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![k1], ts3, ts4);

    // The downstream 1 can get old values as expected.
    let mut event_count = 0;
    loop {
        let events = receive_event_1(false).events.to_vec();
        for event in events.into_iter() {
            match event.event.unwrap() {
                Event_oneof_event::Entries(mut es) => {
                    for row in es.take_entries().to_vec() {
                        if row.get_type() == EventLogType::Prewrite {
                            if row.get_start_ts() == ts3.into_inner() {
                                assert_eq!(row.get_old_value(), b"v1");
                            } else {
                                assert_eq!(row.get_old_value(), b"");
                            }
                            event_count += 1;
                        }
                    }
                }
                other => panic!("unknown event {:?}", other),
            }
        }
        if event_count >= 2 {
            break;
        }
    }

    // The downstream 2 can also get old values because `req`.`extra_op` field is
    // ignored now.
    event_count = 0;
    loop {
        let events = receive_event_2(false).events.to_vec();
        for event in events.into_iter() {
            match event.event.unwrap() {
                Event_oneof_event::Entries(mut es) => {
                    for row in es.take_entries().to_vec() {
                        if row.get_type() == EventLogType::Prewrite {
                            if row.get_start_ts() == ts3.into_inner() {
                                assert_eq!(row.get_old_value(), b"v1");
                            } else {
                                assert_eq!(row.get_old_value(), b"");
                            }
                            event_count += 1;
                        }
                    }
                }
                other => panic!("unknown event {:?}", other),
            }
        }
        if event_count >= 2 {
            break;
        }
    }

    event_feed_wrap_1.replace(None);
    event_feed_wrap_2.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_resolve_ts_checking_concurrency_manager() {
    test_kv_format_impl!(test_cdc_resolve_ts_checking_concurrency_manager_impl<ApiV1 ApiV2>);
}

fn test_cdc_resolve_ts_checking_concurrency_manager_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let cm: ConcurrencyManager = suite.get_txn_concurrency_manager(1).unwrap();
    let lock_key = |key: &[u8], ts: u64| {
        let guard = block_on(cm.lock_key(&Key::from_raw(key)));
        guard.with_lock(|l| {
            *l = Some(Lock::new(
                LockType::Put,
                key.to_vec(),
                ts.into(),
                0,
                None,
                0.into(),
                1,
                ts.into(),
                false,
            ))
        });
        guard
    };

    cm.update_max_ts(20.into(), "").unwrap();

    let guard = lock_key(b"a", 80);
    suite.set_tso(99);

    let mut req = suite.new_changedata_request(1);
    req.set_checkpoint_ts(100);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    // Make sure region 1 is registered.
    let mut events = receive_event(false).events;
    assert_eq!(events.len(), 1);
    match events.pop().unwrap().event.unwrap() {
        // Even if there is no write,
        // it should always outputs an Initialized event.
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    fn check_resolved_ts(event: ChangeDataEvent, check_fn: impl Fn(u64)) {
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            check_fn(resolved_ts.ts)
        }
    }

    check_resolved_ts(receive_event(true), |ts| assert_eq!(ts, 80));
    assert!(cm.max_ts() >= 100.into());

    drop(guard);
    for retry in 0.. {
        let event = receive_event(true);
        let mut current_rts = 0;
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            current_rts = resolved_ts.ts;
            if resolved_ts.ts >= 100 {
                break;
            }
        }
        if retry >= 5 {
            panic!(
                "resolved ts didn't push properly after unlocking memlock. current resolved_ts: {}",
                current_rts
            );
        }
    }

    let _guard = lock_key(b"xa", 90);
    // The resolved_ts should be blocked by the mem lock but it's already greater
    // than 90. Retry until receiving an unchanged resolved_ts because the first
    // several resolved ts received might be updated before acquiring the lock.
    let mut last_resolved_ts = 0;
    let mut success = false;
    for _ in 0..5 {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            let ts = resolved_ts.ts;
            assert!(ts > 100);
            if ts == last_resolved_ts {
                success = true;
                break;
            }
            assert!(ts > last_resolved_ts);
            last_resolved_ts = ts;
        }
    }
    assert!(success, "resolved_ts not blocked by the memory lock");

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_1pc() {
    test_kv_format_impl!(test_cdc_1pc_impl<ApiV1 ApiV2>);
}

fn test_cdc_1pc_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let req = suite.new_changedata_request(1);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // Wait until the region subscription is initialized.
    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        });

    let (k1, v1) = (b"xk1", b"v1");
    let (k2, v2) = (b"xk2", &[0u8; 512]);

    let start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();

    // Let resolved_ts update.
    sleep_ms(500);

    // Prewrite
    let mut prewrite_req = PrewriteRequest::default();
    let region_id = 1;
    prewrite_req.set_context(suite.get_context(region_id));
    let mut m1 = Mutation::default();
    m1.set_op(Op::Put);
    m1.key = k1.to_vec();
    m1.value = v1.to_vec();
    prewrite_req.mut_mutations().push(m1);
    let mut m2 = Mutation::default();
    m2.set_op(Op::Put);
    m2.key = k2.to_vec();
    m2.value = v2.to_vec();
    prewrite_req.mut_mutations().push(m2);
    prewrite_req.primary_lock = k1.to_vec();
    prewrite_req.start_version = start_ts.into_inner();
    prewrite_req.lock_ttl = prewrite_req.start_version + 1;
    prewrite_req.set_try_one_pc(true);
    let prewrite_resp = suite
        .get_tikv_client(region_id)
        .kv_prewrite(&prewrite_req)
        .unwrap();
    assert!(prewrite_resp.get_one_pc_commit_ts() > 0);

    let mut resolved_ts = 0;
    loop {
        let mut cde = receive_event(true);
        if cde.get_resolved_ts().get_ts() > resolved_ts {
            resolved_ts = cde.get_resolved_ts().get_ts();
        }
        let events = cde.mut_events();
        if !events.is_empty() {
            assert_eq!(events.len(), 1);
            match events.pop().unwrap().event.unwrap() {
                Event_oneof_event::Entries(mut es) => {
                    let mut entries = es.take_entries().into_vec();
                    assert_eq!(entries.len(), 2);
                    entries.sort_by(|a, b| a.key.cmp(&b.key));

                    let (e0, e1) = (&entries[0], &entries[1]);
                    assert_eq!(e0.get_type(), EventLogType::Committed);
                    assert_eq!(e0.get_key(), k1);
                    assert_eq!(e0.get_value(), v1);
                    assert!(e0.commit_ts > resolved_ts);
                    assert_eq!(e1.get_type(), EventLogType::Committed);
                    assert_eq!(e1.get_key(), k2);
                    assert_eq!(e1.get_value(), v2);
                    assert!(e1.commit_ts > resolved_ts);
                    break;
                }
                other => panic!("unknown event {:?}", other),
            }
        }
    }

    suite.stop();
}

#[test]
fn test_old_value_1pc() {
    test_kv_format_impl!(test_old_value_1pc_impl<ApiV1 ApiV2>);
}

fn test_old_value_1pc_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // Insert value
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Put);
    m1.key = k1.clone();
    m1.value = b"v1".to_vec();
    suite.must_kv_prewrite(1, vec![m1], k1.clone(), 10.into());
    suite.must_kv_commit(1, vec![k1.clone()], 10.into(), 15.into());

    // Prewrite with 1PC
    let start_ts = 20;
    let mut prewrite_req = PrewriteRequest::default();
    let region_id = 1;
    prewrite_req.set_context(suite.get_context(region_id));
    let mut m2 = Mutation::default();
    m2.set_op(Op::Put);
    m2.key = k1.clone();
    m2.value = b"v2".to_vec();
    prewrite_req.mut_mutations().push(m2);
    prewrite_req.primary_lock = k1;
    prewrite_req.start_version = start_ts;
    prewrite_req.lock_ttl = 1000;
    prewrite_req.set_try_one_pc(true);
    let prewrite_resp = suite
        .get_tikv_client(region_id)
        .kv_prewrite(&prewrite_req)
        .unwrap();
    assert!(prewrite_resp.get_one_pc_commit_ts() > 0);

    'outer: loop {
        let events = receive_event(false).events.to_vec();
        for event in events.into_iter() {
            match event.event.unwrap() {
                Event_oneof_event::Entries(mut es) => {
                    for row in es.take_entries().to_vec() {
                        if row.get_type() == EventLogType::Committed
                            && row.get_start_ts() == start_ts
                        {
                            assert_eq!(row.get_old_value(), b"v1");
                            break 'outer;
                        }
                    }
                }
                other => panic!("unknown event {:?}", other),
            }
        }
    }

    suite.stop();
}

#[test]
fn test_old_value_cache_hit() {
    test_kv_format_impl!(test_old_value_cache_hit_impl<ApiV1 ApiV2>);
}

fn test_old_value_cache_hit_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let scheduler = suite.endpoints.values().next().unwrap().scheduler();
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Initialized);
        }
        other => panic!("unknown event {:?}", other),
    }
    let (tx, rx) = mpsc::channel();

    // Insert value, simulate INSERT INTO.
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Insert);
    m1.key = k1.clone();
    m1.value = b"v1".to_vec();
    suite.must_kv_prewrite(1, vec![m1], k1.clone(), 10.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_value(), b"v1");
            assert_eq!(row.get_old_value(), b"");
            assert_eq!(row.get_type(), EventLogType::Prewrite);
            assert_eq!(row.get_start_ts(), 10);
        }
        other => panic!("unknown event {:?}", other),
    }
    // k1 old value must be cached.
    let tx_ = tx.clone();
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 1);
    assert_eq!(miss_count, 0);
    suite.must_kv_commit(1, vec![k1], 10.into(), 15.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Commit);
            assert_eq!(row.get_commit_ts(), 15);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Update a noexist value, simulate INSERT IGNORE INTO.
    let mut m2 = Mutation::default();
    let k2 = b"xk2".to_vec();
    m2.set_op(Op::Put);
    m2.key = k2.clone();
    m2.value = b"v2".to_vec();
    suite.must_kv_prewrite(1, vec![m2], k2.clone(), 10.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_value(), b"v2");
            assert_eq!(row.get_old_value(), b"");
            assert_eq!(row.get_type(), EventLogType::Prewrite);
            assert_eq!(row.get_start_ts(), 10);
        }
        other => panic!("unknown event {:?}", other),
    }
    // k2 old value must be cached.
    let tx_ = tx.clone();
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 2);
    assert_eq!(miss_count, 0);
    suite.must_kv_commit(1, vec![k2], 10.into(), 15.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Commit);
            assert_eq!(row.get_commit_ts(), 15);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Update an exist value, simulate UPDATE.
    let mut m2 = Mutation::default();
    let k2 = b"xk2".to_vec();
    m2.set_op(Op::Put);
    m2.key = k2.clone();
    m2.value = b"v3".to_vec();
    suite.must_kv_prewrite(1, vec![m2], k2.clone(), 20.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_value(), b"v3");
            assert_eq!(row.get_old_value(), b"v2");
            assert_eq!(row.get_type(), EventLogType::Prewrite);
            assert_eq!(row.get_start_ts(), 20);
        }
        other => panic!("unknown event {:?}", other),
    }
    // k2 old value must be cached.
    let tx_ = tx;
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 3);
    assert_eq!(miss_count, 0);
    suite.must_kv_commit(1, vec![k2], 20.into(), 25.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Commit);
            assert_eq!(row.get_commit_ts(), 25);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_old_value_cache_hit_pessimistic() {
    test_kv_format_impl!(test_old_value_cache_hit_pessimistic_impl<ApiV1 ApiV2>);
}

fn test_old_value_cache_hit_pessimistic_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let scheduler = suite.endpoints.values().next().unwrap().scheduler();
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Initialized);
        }
        other => panic!("unknown event {:?}", other),
    }
    let (tx, rx) = mpsc::channel();

    // Insert a value in pessimistic txn.
    let mut m3 = Mutation::default();
    let k3 = b"xk3".to_vec();
    m3.set_op(Op::PessimisticLock);
    m3.key = k3.clone();
    suite.must_acquire_pessimistic_lock(1, vec![m3.clone()], k3.clone(), 10.into(), 10.into());
    // CDC does not outputs PessimisticLock.
    // No cache access.
    let tx_ = tx.clone();
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 0);
    assert_eq!(miss_count, 0);
    m3.set_op(Op::Put);
    m3.value = b"v1".to_vec();
    suite.must_kv_pessimistic_prewrite(1, vec![m3], k3.clone(), 10.into(), 10.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_value(), b"v1");
            assert_eq!(row.get_old_value(), b"");
            assert_eq!(row.get_type(), EventLogType::Prewrite);
            assert_eq!(row.get_start_ts(), 10);
        }
        other => panic!("unknown event {:?}", other),
    }
    // k3 old value must be cached.
    let tx_ = tx.clone();
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 1);
    assert_eq!(miss_count, 0);

    suite.must_kv_commit(1, vec![k3], 10.into(), 15.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Commit);
            assert_eq!(row.get_commit_ts(), 15);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Update a value in pessimistic txn.
    let mut m3 = Mutation::default();
    let k3 = b"xk3".to_vec();
    m3.set_op(Op::PessimisticLock);
    m3.key = k3.clone();
    suite.must_acquire_pessimistic_lock(1, vec![m3.clone()], k3.clone(), 20.into(), 20.into());
    // CDC does not outputs PessimisticLock.
    // No cache access.
    let tx_ = tx.clone();
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 1);
    assert_eq!(miss_count, 0);
    m3.set_op(Op::Put);
    m3.value = b"v2".to_vec();
    suite.must_kv_pessimistic_prewrite(1, vec![m3], k3.clone(), 20.into(), 20.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_value(), b"v2");
            assert_eq!(row.get_old_value(), b"v1");
            assert_eq!(row.get_type(), EventLogType::Prewrite);
            assert_eq!(row.get_start_ts(), 20);
        }
        other => panic!("unknown event {:?}", other),
    }
    // k3 old value must be cached.
    let tx_ = tx;
    scheduler
        .schedule(Task::Validate(Validate::OldValueCache(Box::new(
            move |old_value_cache| {
                tx_.send((old_value_cache.access_count(), old_value_cache.miss_count()))
                    .unwrap();
            },
        ))))
        .unwrap();
    let (access_count, miss_count) = rx.recv().unwrap();
    assert_eq!(access_count, 2);
    assert_eq!(miss_count, 0);
    suite.must_kv_commit(1, vec![k3], 20.into(), 25.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Commit);
            assert_eq!(row.get_commit_ts(), 25);
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_region_created_replicate() {
    let cluster = new_server_cluster(0, 2);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();

    let region = suite.cluster.get_region(&[]);
    suite
        .cluster
        .must_transfer_leader(region.id, new_peer(2, 2));
    suite
        .cluster
        .pd_client
        .must_remove_peer(region.id, new_peer(1, 1));

    let recv_filter = Box::new(
        RegionPacketFilter::new(region.get_id(), 1)
            .direction(Direction::Recv)
            .msg_type(MessageType::MsgAppend),
    );
    suite.cluster.sim.wl().add_recv_filter(1, recv_filter);
    suite
        .cluster
        .pd_client
        .must_add_peer(region.id, new_peer(1, 1));
    let region = suite.cluster.get_region(&[]);
    let req = suite.new_changedata_request(region.id);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(region.id));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    sleep_ms(1000);
    suite.cluster.sim.wl().clear_recv_filters(1);

    let mut counter = 0;
    let mut previous_ts = 0;
    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            assert!(resolved_ts.ts >= previous_ts);
            assert!(resolved_ts.regions == vec![region.id]);
            previous_ts = resolved_ts.ts;
            counter += 1;
        }
        if counter > 5 {
            break;
        }
    }
    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_scan_ignore_gc_fence() {
    test_kv_format_impl!(test_cdc_scan_ignore_gc_fence_impl<ApiV1 ApiV2>);
}

fn test_cdc_scan_ignore_gc_fence_impl<F: KvFormat>() {
    // This case is similar to `test_cdc_scan` but constructs a case with GC Fence.
    let mut suite = TestSuite::new(1, F::TAG);

    let (key, v1, v2) = (b"xkey", b"value1", b"value2");

    // Write two versions to the key.
    let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = key.to_vec();
    mutation.value = v1.to_vec();
    suite.must_kv_prewrite(1, vec![mutation], key.to_vec(), start_ts1);

    let commit_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![key.to_vec()], start_ts1, commit_ts1);

    let start_ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mutation = Mutation {
        key: key.to_vec(),
        value: v2.to_vec(),
        ..Default::default()
    };
    suite.must_kv_prewrite(1, vec![mutation], key.to_vec(), start_ts2);

    let commit_ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![key.to_vec()], start_ts2, commit_ts2);

    // Assume the first version above is written by async commit and it's commit_ts
    // is not unique. Use it's commit_ts as another transaction's start_ts.
    // Run check_txn_status on commit_ts1 so that gc_fence will be set on the first
    // version.
    let caller_start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let action = suite.must_check_txn_status(
        1,
        key.to_vec(),
        commit_ts1,
        caller_start_ts,
        caller_start_ts,
        true,
    );
    assert_eq!(action, Action::LockNotExistRollback);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    if events.len() == 1 {
        events.extend(receive_event(false).events.into_iter());
    }
    assert_eq!(events.len(), 2, "{:?}", events);
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 2, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.start_ts, start_ts2.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, commit_ts2.into_inner(), "{:?}", es);
            assert_eq!(e.key, key.to_vec(), "{:?}", es);
            assert_eq!(e.value, v2.to_vec(), "{:?}", es);
            let e = &es.entries[1];
            assert_eq!(e.get_type(), EventLogType::Committed, "{:?}", es);
            assert_eq!(e.start_ts, start_ts1.into_inner(), "{:?}", es);
            assert_eq!(e.commit_ts, commit_ts1.into_inner(), "{:?}", es);
            assert_eq!(e.key, key.to_vec(), "{:?}", es);
            assert_eq!(e.value, v1.to_vec(), "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }
    match events.pop().unwrap().event.unwrap() {
        Event_oneof_event::Entries(es) => {
            assert!(es.entries.len() == 1, "{:?}", es);
            let e = &es.entries[0];
            assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
        }
        other => panic!("unknown event {:?}", other),
    }

    suite.stop();
}

#[test]
fn test_cdc_extract_rollback_if_gc_fence_set() {
    test_kv_format_impl!(test_cdc_extract_rollback_if_gc_fence_set_impl<ApiV1 ApiV2>);
}

fn test_cdc_extract_rollback_if_gc_fence_set_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);

    let req = suite.new_changedata_request(1);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    for e in receive_event(false).events.into_vec() {
        match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        };
    }

    sleep_ms(1000);

    // Write two versions of a key
    let (key, v1, v2, v3) = (b"xkey", b"value1", b"value2", b"value3");
    let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = key.to_vec();
    mutation.value = v1.to_vec();
    suite.must_kv_prewrite(1, vec![mutation], key.to_vec(), start_ts1);

    let commit_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![key.to_vec()], start_ts1, commit_ts1);

    let start_ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = key.to_vec();
    mutation.value = v2.to_vec();
    suite.must_kv_prewrite(1, vec![mutation], key.to_vec(), start_ts2);

    let commit_ts2 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_commit(1, vec![key.to_vec()], start_ts2, commit_ts2);

    // We don't care about the events caused by the previous writings in this test
    // case, and it's too complicated to check them. Just skip them here, and
    // wait for resolved_ts to be pushed to a greater value than the two
    // versions' commit_ts-es.
    let skip_to_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    loop {
        let e = receive_event(true);
        if let Some(r) = e.resolved_ts.as_ref() {
            if r.ts > skip_to_ts.into_inner() {
                break;
            }
        }
    }

    // Assume the two versions of the key are written by async commit transactions,
    // and their commit_ts-es are also other transaction's start_ts-es. Run
    // check_txn_status on the commit_ts-es of the two versions to cause
    // overlapping rollback.
    let caller_start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_check_txn_status(
        1,
        key.to_vec(),
        commit_ts1,
        caller_start_ts,
        caller_start_ts,
        true,
    );

    // Expects receiving rollback
    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Rollback, "{:?}", es);
                assert_eq!(e.get_start_ts(), commit_ts1.into_inner());
                assert_eq!(e.get_commit_ts(), 0);
            }
            other => panic!("unknown event {:?}", other),
        });

    suite.must_check_txn_status(
        1,
        key.to_vec(),
        commit_ts2,
        caller_start_ts,
        caller_start_ts,
        true,
    );

    // Expects receiving rollback
    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Rollback, "{:?}", es);
                assert_eq!(e.get_start_ts(), commit_ts2.into_inner());
                assert_eq!(e.get_commit_ts(), 0);
            }
            other => panic!("unknown event {:?}", other),
        });

    // In some special cases, a newly committed record may carry an overlapped
    // rollback initially. In this case, gc_fence shouldn't be set, and CDC
    // ignores the rollback and handles the committing normally.
    let start_ts3 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = key.to_vec();
    mutation.value = v3.to_vec();
    suite.must_kv_prewrite(1, vec![mutation], key.to_vec(), start_ts3);
    // Consume the prewrite event.
    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Prewrite, "{:?}", es);
                assert_eq!(e.get_start_ts(), start_ts3.into_inner());
            }
            other => panic!("unknown event {:?}", other),
        });

    // Again, assume the transaction is committed with async commit protocol, and
    // the commit_ts is also another transaction's start_ts.
    let commit_ts3 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    // Rollback another transaction before committing, then the rolling back
    // information will be recorded in the lock.
    let caller_start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_check_txn_status(
        1,
        key.to_vec(),
        commit_ts3,
        caller_start_ts,
        caller_start_ts,
        true,
    );
    // Expects receiving rollback
    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Rollback, "{:?}", es);
                assert_eq!(e.get_start_ts(), commit_ts3.into_inner());
                assert_eq!(e.get_commit_ts(), 0);
            }
            other => panic!("unknown event {:?}", other),
        });
    // Commit the transaction, then it will have overlapped rollback initially.
    suite.must_kv_commit(1, vec![key.to_vec()], start_ts3, commit_ts3);
    // Expects receiving a normal committing event.
    let event = receive_event(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Commit, "{:?}", es);
                assert_eq!(e.get_start_ts(), start_ts3.into_inner());
                assert_eq!(e.get_commit_ts(), commit_ts3.into_inner());
                assert_eq!(e.get_value(), v3);
            }
            other => panic!("unknown event {:?}", other),
        });

    suite.stop();
}

// This test is created for covering the case that term was increased without
// leader change. Ideally leader id and term in StoreMeta should be updated
// together with a yielded SoftState, but sometimes the leader was transferred
// to another store and then changed back, a follower would not get a new
// SoftState.
#[test]
fn test_term_change() {
    let cluster = new_server_cluster(0, 3);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    suite
        .cluster
        .must_transfer_leader(region.id, new_peer(2, 2));
    // Simulate network partition.
    let recv_filter =
        Box::new(RegionPacketFilter::new(region.get_id(), 1).direction(Direction::Recv));
    suite.cluster.sim.wl().add_recv_filter(1, recv_filter);
    // Transfer leader to peer 3 and then change it back to peer 2.
    // Peer 1 would not get a new SoftState.
    suite
        .cluster
        .must_transfer_leader(region.id, new_peer(3, 3));
    suite
        .cluster
        .must_transfer_leader(region.id, new_peer(2, 2));
    suite.cluster.sim.wl().clear_recv_filters(1);

    suite
        .cluster
        .pd_client
        .must_remove_peer(region.id, new_peer(3, 3));
    let region = suite.cluster.get_region(&[]);
    let req = suite.new_changedata_request(region.id);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(region.id));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut counter = 0;
    let mut previous_ts = 0;
    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            assert!(resolved_ts.ts >= previous_ts);
            assert!(resolved_ts.regions == vec![region.id]);
            previous_ts = resolved_ts.ts;
            counter += 1;
        }
        if counter > 5 {
            break;
        }
    }
    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_cdc_no_write_corresponding_to_lock() {
    test_kv_format_impl!(test_cdc_no_write_corresponding_to_lock_impl<ApiV1 ApiV2>);
}

fn test_cdc_no_write_corresponding_to_lock_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // Txn1 commit_ts = 15
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Put);
    m1.key = k1.clone();
    m1.value = b"v1".to_vec();
    suite.must_kv_prewrite(1, vec![m1.clone()], k1.clone(), 10.into());
    suite.must_kv_commit(1, vec![k1.clone()], 10.into(), 15.into());

    // Txn2 start_ts = 15
    m1.value = b"v2".to_vec();
    suite.must_kv_prewrite(1, vec![m1.clone()], k1.clone(), 15.into());
    // unprotected rollback, no write is written
    suite.must_kv_rollback(1, vec![k1.clone()], 15.into());

    // Write a new txn
    m1.value = b"v3".to_vec();
    suite.must_kv_prewrite(1, vec![m1], k1.clone(), 20.into());
    suite.must_kv_commit(1, vec![k1], 20.into(), 25.into());

    let mut advance_cnt = 0;
    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            advance_cnt += 1;
            if resolved_ts.ts >= 25 {
                break;
            }
            if advance_cnt > 50 {
                panic!("resolved_ts is not advanced, stuck at {}", resolved_ts.ts);
            }
        }
    }

    suite.stop();
}

#[test]
fn test_cdc_write_rollback_when_no_lock() {
    test_kv_format_impl!(test_cdc_write_rollback_when_no_lock_impl<ApiV1 ApiV2>);
}

fn test_cdc_write_rollback_when_no_lock_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // Txn1 commit_ts = 15
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Put);
    m1.key = k1.clone();
    m1.value = vec![b'x'; 16];
    suite.must_kv_prewrite(1, vec![m1], k1.clone(), 10.into());

    // Wait until resolved_ts advanced to 10
    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            if resolved_ts.ts == 10 {
                break;
            }
        }
    }

    // Do a rollback on the same key, but the start_ts is different.
    suite.must_kv_rollback(1, vec![k1.clone()], 5.into());

    // resolved_ts shouldn't be advanced beyond 10
    for _ in 0..10 {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            if resolved_ts.ts > 10 {
                panic!("resolved_ts shouldn't be advanced beyond 10");
            }
        }
    }

    suite.must_kv_commit(1, vec![k1], 10.into(), 15.into());

    let mut advance_cnt = 0;
    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            advance_cnt += 1;
            if resolved_ts.ts > 15 {
                break;
            }
            if advance_cnt > 10 {
                panic!("resolved_ts is not advanced, stuck at {}", resolved_ts.ts);
            }
        }
    }

    suite.stop();
}

#[test]
fn test_resolved_ts_cluster_upgrading() {
    let cluster = new_server_cluster(0, 3);
    cluster.pd_client.disable_default_operator();
    unsafe {
        cluster
            .pd_client
            .feature_gate()
            .reset_version("4.0.0")
            .unwrap();
    }
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();

    let region = suite.cluster.get_region(&[]);
    let req = suite.new_changedata_request(region.id);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(region.id));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let event = receive_event(true);
    if let Some(resolved_ts) = event.resolved_ts.as_ref() {
        assert!(resolved_ts.regions == vec![region.id]);
        assert_eq!(CDC_RESOLVED_TS_ADVANCE_METHOD.get(), 0);
    }
    suite
        .cluster
        .pd_client
        .feature_gate()
        .set_version("5.0.0")
        .unwrap();

    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            assert!(resolved_ts.regions == vec![region.id]);
            if CDC_RESOLVED_TS_ADVANCE_METHOD.get() == 1 {
                break;
            }
        }
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_resolved_ts_with_learners() {
    let cluster = new_server_cluster(0, 2);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new()
        .cluster(cluster)
        .build_with_cluster_runner(|cluster| {
            let r = cluster.run_conf_change();
            cluster.pd_client.must_add_peer(r, new_learner_peer(2, 2));
        });

    let rid = suite.cluster.get_region(&[]).id;
    let req = suite.new_changedata_request(rid);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(rid));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    for _ in 0..10 {
        let event = receive_event(true);
        if event.has_resolved_ts() {
            assert!(event.get_resolved_ts().regions == vec![rid]);
            drop(receive_event);
            suite.stop();
            return;
        }
    }
    panic!("resolved timestamp should be advanced correctly");
}

#[test]
fn test_prewrite_without_value() {
    let cluster = new_server_cluster(0, 2);
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let rid = suite.cluster.get_region(&[]).id;
    let ctx = suite.get_context(rid);
    let client = suite.get_tikv_client(rid).clone();
    let large_value = vec![b'x'; 2 * txn_types::SHORT_VALUE_MAX_LEN];

    // Perform a pessimistic prewrite with a large value.
    let mut muts = vec![Mutation::default()];
    muts[0].set_op(Op::Put);
    muts[0].key = b"key".to_vec();
    muts[0].value = large_value.clone();
    try_kv_prewrite_pessimistic(&client, ctx.clone(), muts, b"key".to_vec(), 10);

    let req = suite.new_changedata_request(rid);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(rid));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    // The prewrite can be retrieved from incremental scan.
    let event = receive_event(false);
    assert_eq!(
        event.get_events()[0].get_entries().entries[0].value,
        large_value
    );

    // check_txn_status will put the lock again, but without value.
    must_check_txn_status(&client, ctx.clone(), b"key", 10, 12, 12);
    must_kv_commit(&client, ctx, vec![b"key".to_vec()], 10, 14, 14);
    // The lock without value shouldn't be retrieved.
    let event = receive_event(false);
    assert_eq!(event.get_events()[0].get_entries().entries[0].commit_ts, 14);
}

#[test]
fn test_filter_loop() {
    test_kv_format_impl!(test_filter_loop_impl<ApiV1 ApiV2>);
}

fn test_filter_loop_impl<F: KvFormat>() {
    let mut suite = TestSuite::new(1, F::TAG);
    let mut req = suite.new_changedata_request(1);
    req.set_extra_op(ExtraOp::ReadOldValue);
    req.set_filter_loop(true);
    let (mut req_tx, event_feed_wrap, receive_event) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let row = &es.take_entries().to_vec()[0];
            assert_eq!(row.get_type(), EventLogType::Initialized);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Insert value, simulate INSERT INTO.
    let mut m1 = Mutation::default();
    let k1 = b"xk1".to_vec();
    m1.set_op(Op::Insert);
    m1.key = k1.clone();
    m1.value = b"v1".to_vec();
    suite.must_kv_prewrite_with_source(1, vec![m1], k1.clone(), 10.into(), 1);
    let mut m2 = Mutation::default();
    let k2 = b"xk2".to_vec();
    m2.set_op(Op::Insert);
    m2.key = k2.clone();
    m2.value = b"v2".to_vec();
    suite.must_kv_prewrite_with_source(1, vec![m2], k2.clone(), 12.into(), 0);
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let events = es.take_entries().to_vec();
            assert_eq!(events.len(), 1);
            let row = &events[0];
            assert_eq!(row.get_value(), b"v2");
            assert_eq!(row.get_old_value(), b"");
            assert_eq!(row.get_type(), EventLogType::Prewrite);
            assert_eq!(row.get_start_ts(), 12);
        }
        other => panic!("unknown event {:?}", other),
    }
    suite.must_kv_commit_with_source(1, vec![k1], 10.into(), 15.into(), 1);
    suite.must_kv_commit_with_source(1, vec![k2], 12.into(), 17.into(), 0);
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let events = es.take_entries().to_vec();
            assert_eq!(events.len(), 1);
            let row = &events[0];
            assert_eq!(row.get_type(), EventLogType::Commit);
            assert_eq!(row.get_commit_ts(), 17);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Rollback
    let mut m3 = Mutation::default();
    let k3 = b"xk3".to_vec();
    m3.set_op(Op::Put);
    m3.key = k3.clone();
    m3.value = b"v3".to_vec();
    suite.must_kv_prewrite_with_source(1, vec![m3], k3.clone(), 30.into(), 1);
    suite.must_kv_rollback(1, vec![k3], 30.into());
    let mut events = receive_event(false).events.to_vec();
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let events = es.take_entries().to_vec();
            assert_eq!(events.len(), 1);
            let row = &events[0];
            assert_eq!(row.get_type(), EventLogType::Rollback);
            assert_eq!(row.get_commit_ts(), 0);
        }
        other => panic!("unknown event {:?}", other),
    }

    // Update value
    let k1 = b"xk1".to_vec();
    let mut m4 = Mutation::default();
    m4.set_op(Op::Put);
    m4.key = k1.clone();
    m4.value = vec![b'3'; 5120];
    suite.must_kv_prewrite_with_source(1, vec![m4], k1.clone(), 40.into(), 1);
    suite.must_kv_commit_with_source(1, vec![k1], 40.into(), 42.into(), 1);
    let k2 = b"xk2".to_vec();
    let mut m5 = Mutation::default();
    m5.set_op(Op::Put);
    m5.key = k2.clone();
    m5.value = vec![b'4'; 5121];
    suite.must_kv_prewrite(1, vec![m5], k2.clone(), 44.into());
    suite.must_kv_commit(1, vec![k2.clone()], 44.into(), 46.into());
    let mut events = receive_event(false).events.to_vec();
    if events.len() == 1 {
        events.extend(receive_event(false).events.into_iter());
    }
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let events = es.take_entries().to_vec();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].get_type(), EventLogType::Prewrite);
            assert_eq!(events[0].get_start_ts(), 44);
            assert_eq!(events[0].get_key(), k2.as_slice());
        }
        other => panic!("unknown event {:?}", other),
    }
    match events.remove(0).event.unwrap() {
        Event_oneof_event::Entries(mut es) => {
            let events = es.take_entries().to_vec();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].get_type(), EventLogType::Commit);
            assert_eq!(events[0].get_commit_ts(), 46);
            assert_eq!(events[0].get_key(), k2.as_slice());
        }
        other => panic!("unknown event {:?}", other),
    }

    event_feed_wrap.replace(None);
    suite.stop();
}

#[test]
fn test_flashback() {
    let mut cluster = new_server_cluster(0, 1);
    cluster.cfg.resolved_ts.advance_ts_interval = ReadableDuration::millis(50);
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();

    let key = Key::from_raw(b"a");
    let region = suite.cluster.get_region(key.as_encoded());
    let region_id = region.get_id();
    let req = suite.new_changedata_request(region_id);
    let (mut req_tx, _, receive_event) = new_event_feed(suite.get_region_cdc_client(region_id));
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();
    let event = receive_event(false);
    event.events.into_iter().for_each(|e| {
        match e.event.unwrap() {
            // Even if there is no write,
            // it should always outputs an Initialized event.
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        }
    });
    // Sleep a while to make sure the stream is registered.
    sleep_ms(1000);
    let start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    for i in 0..2 {
        let (k, v) = (
            format!("key{}", i).as_bytes().to_vec(),
            format!("value{}", i).as_bytes().to_vec(),
        );
        // Prewrite
        let start_ts1 = block_on(suite.cluster.pd_client.get_tso()).unwrap();
        let mut mutation = Mutation::default();
        mutation.set_op(Op::Put);
        mutation.key = k.clone();
        mutation.value = v;
        suite.must_kv_prewrite(1, vec![mutation], k.clone(), start_ts1);
        // Commit
        let commit_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
        suite.must_kv_commit(1, vec![k.clone()], start_ts1, commit_ts);
    }
    let (start_key, end_key) = (b"key0".to_vec(), b"key2".to_vec());
    // Prepare flashback.
    let flashback_start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_prepare_flashback(region_id, &start_key, &end_key, flashback_start_ts);
    // resolved ts should not be advanced anymore.
    let mut counter = 0;
    let mut last_resolved_ts = 0;
    loop {
        let event = receive_event(true);
        if let Some(resolved_ts) = event.resolved_ts.as_ref() {
            if resolved_ts.ts == last_resolved_ts {
                counter += 1;
            }
            last_resolved_ts = resolved_ts.ts;
        }
        if counter > 20 {
            break;
        }
        sleep_ms(50);
    }
    // Flashback.
    let flashback_commit_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    suite.must_kv_flashback(
        region_id,
        &start_key,
        &end_key,
        flashback_start_ts,
        flashback_commit_ts,
        start_ts,
    );
    // Check the flashback event.
    let mut resolved_ts = 0;
    let mut event_counter = 0;
    loop {
        let mut cde = receive_event(true);
        if cde.get_resolved_ts().get_ts() > resolved_ts {
            resolved_ts = cde.get_resolved_ts().get_ts();
        }
        let events = cde.mut_events();
        if !events.is_empty() {
            assert_eq!(events.len(), 1);
            match events.pop().unwrap().event.unwrap() {
                Event_oneof_event::Entries(entries) => {
                    assert_eq!(entries.entries.len(), 1);
                    event_counter += 1;
                    let e = &entries.entries[0];
                    assert!(e.commit_ts > resolved_ts);
                    assert_eq!(e.get_op_type(), EventRowOpType::Delete);
                    match e.get_type() {
                        EventLogType::Committed => {
                            // First entry should be a 1PC flashback.
                            assert_eq!(e.get_key(), b"key1");
                            assert_eq!(event_counter, 1);
                        }
                        EventLogType::Commit => {
                            // Second entry should be a 2PC commit.
                            assert_eq!(e.get_key(), b"key0");
                            assert_eq!(event_counter, 2);
                            break;
                        }
                        _ => panic!("unknown event type {:?}", e.get_type()),
                    }
                }
                other => panic!("unknown event {:?}", other),
            }
        }
    }
}

#[test]
fn test_cdc_filter_key_range() {
    let mut suite = TestSuite::new(1, ApiVersion::V1);

    let req = suite.new_changedata_request(1);

    // Observe range [key1, key3).
    let mut req_1_3 = req.clone();
    req_1_3.request_id = 13;
    req_1_3.start_key = Key::from_raw(b"key1").into_encoded();
    req_1_3.end_key = Key::from_raw(b"key3").into_encoded();
    let (mut req_tx13, _event_feed_wrap13, receive_event13) =
        new_event_feed(suite.get_region_cdc_client(1));
    block_on(req_tx13.send((req_1_3, WriteFlags::default()))).unwrap();
    let event = receive_event13(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        });

    let (mut req_tx24, _event_feed_wrap24, receive_event24) =
        new_event_feed(suite.get_region_cdc_client(1));
    let mut req_2_4 = req;
    req_2_4.request_id = 24;
    req_2_4.start_key = Key::from_raw(b"key2").into_encoded();
    req_2_4.end_key = Key::from_raw(b"key4").into_encoded();
    block_on(req_tx24.send((req_2_4, WriteFlags::default()))).unwrap();
    let event = receive_event24(false);
    event
        .events
        .into_iter()
        .for_each(|e| match e.event.unwrap() {
            Event_oneof_event::Entries(es) => {
                assert!(es.entries.len() == 1, "{:?}", es);
                let e = &es.entries[0];
                assert_eq!(e.get_type(), EventLogType::Initialized, "{:?}", es);
            }
            other => panic!("unknown event {:?}", other),
        });

    // Sleep a while to make sure the stream is registered.
    sleep_ms(1000);

    let receive_and_check_events = |is13: bool, is24: bool| -> Vec<Event> {
        if is13 && is24 {
            let mut events = receive_event13(false).events.to_vec();
            let mut events24 = receive_event24(false).events.to_vec();
            events.append(&mut events24);
            events
        } else if is13 {
            let events = receive_event13(false).events.to_vec();
            let event = receive_event24(true);
            assert!(event.resolved_ts.is_some(), "{:?}", event);
            events
        } else if is24 {
            let events = receive_event24(false).events.to_vec();
            let event = receive_event13(true);
            assert!(event.resolved_ts.is_some(), "{:?}", event);
            events
        } else {
            let event = receive_event13(true);
            assert!(event.resolved_ts.is_some(), "{:?}", event);
            let event = receive_event24(true);
            assert!(event.resolved_ts.is_some(), "{:?}", event);
            vec![]
        }
    };
    for case in &[
        ("key1", true, false, true /* commit */),
        ("key1", true, false, false /* rollback */),
        ("key2", true, true, true),
        ("key3", false, true, true),
        ("key4", false, false, true),
    ] {
        let (k, v) = (case.0.to_owned(), "value".to_owned());
        // Prewrite
        let start_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
        let mut mutation = Mutation::default();
        mutation.set_op(Op::Put);
        mutation.key = k.clone().into_bytes();
        mutation.value = v.into_bytes();
        suite.must_kv_prewrite(1, vec![mutation], k.clone().into_bytes(), start_ts);
        let mut events = receive_and_check_events(case.1, case.2);
        while let Some(event) = events.pop() {
            match event.event.unwrap() {
                Event_oneof_event::Entries(entries) => {
                    assert_eq!(entries.entries.len(), 1);
                    assert_eq!(entries.entries[0].get_type(), EventLogType::Prewrite);
                }
                other => panic!("unknown event {:?}", other),
            }
        }

        if case.3 {
            // Commit
            let commit_ts = block_on(suite.cluster.pd_client.get_tso()).unwrap();
            suite.must_kv_commit(1, vec![k.into_bytes()], start_ts, commit_ts);
            let mut events = receive_and_check_events(case.1, case.2);
            while let Some(event) = events.pop() {
                match event.event.unwrap() {
                    Event_oneof_event::Entries(entries) => {
                        assert_eq!(entries.entries.len(), 1);
                        assert_eq!(entries.entries[0].get_type(), EventLogType::Commit);
                    }
                    other => panic!("unknown event {:?}", other),
                }
            }
        } else {
            // Rollback
            suite.must_kv_rollback(1, vec![k.into_bytes()], start_ts);
            let mut events = receive_and_check_events(case.1, case.2);
            while let Some(event) = events.pop() {
                match event.event.unwrap() {
                    Event_oneof_event::Entries(entries) => {
                        assert_eq!(entries.entries.len(), 1);
                        assert_eq!(entries.entries[0].get_type(), EventLogType::Rollback);
                    }
                    other => panic!("unknown event {:?}", other),
                }
            }
        }
    }

    suite.stop();
}

#[test]
fn test_cdc_partial_subscription() {
    let mut cluster = new_server_cluster(0, 1);
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    let rid = region.id;

    let prewrite_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let (k, v) = (b"key".to_vec(), vec![b'x'; 16]);
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_prewrite(rid, vec![mutation], k.clone(), prewrite_tso);

    let cf_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();
    let (mut req_tx, _, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 1;
    req.checkpoint_ts = cf_tso.into_inner();
    req.set_start_key(Key::from_raw(b"x").into_encoded());
    req.set_end_key(Key::from_raw(b"z").into_encoded());
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    let cdc_event = receive_event(false);
    'WaitInit: for event in cdc_event.get_events() {
        for entry in event.get_entries().get_entries() {
            match entry.get_type() {
                EventLogType::Prewrite => {}
                EventLogType::Initialized => break 'WaitInit,
                _ => unreachable!(),
            }
        }
    }

    for _ in 0..10 {
        let cdc_event = receive_event(true);
        if cdc_event.has_resolved_ts() {
            let resolved_ts = cdc_event.get_resolved_ts();
            if resolved_ts.ts > prewrite_tso.into_inner() {
                return;
            }
        }
    }
    panic!("resolved_ts should exceed prewrite_tso");
}

#[test]
fn test_cdc_rollback_prewrites_with_txn_source() {
    let mut cluster = new_server_cluster(0, 1);
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    let rid = region.id;
    let cf_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();

    let (mut req_tx, _, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 1;
    req.checkpoint_ts = cf_tso.into_inner();
    req.filter_loop = true;
    req.set_start_key(Key::from_raw(b"a").into_encoded());
    req.set_end_key(Key::from_raw(b"z").into_encoded());
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    let cdc_event = receive_event(false);
    'WaitInit: for event in cdc_event.get_events() {
        for entry in event.get_entries().get_entries() {
            match entry.get_type() {
                EventLogType::Prewrite => {}
                EventLogType::Initialized => break 'WaitInit,
                _ => unreachable!(),
            }
        }
    }

    let start_tso = cf_tso.next();
    let k = b"key".to_vec();
    let v = vec![b'x'; 16 * 1024];
    let mut mutation = Mutation::default();
    mutation.set_op(Op::Put);
    mutation.key = k.clone();
    mutation.value = v;
    suite.must_kv_prewrite_with_source(rid, vec![mutation], k.clone(), start_tso, 1);

    loop {
        let cdc_event = receive_event(true);
        if cdc_event.has_resolved_ts() {
            let resolved_ts = cdc_event.get_resolved_ts().get_ts();
            assert_eq!(resolved_ts, start_tso.into_inner());
            break;
        }
    }

    suite.must_kv_rollback(rid, vec![k.clone()], start_tso);

    // We can't receive the prewrite because it's with a txn_source,
    // but we can receive the rollback.
    let mut rollbacked = false;
    for _ in 0..5 {
        let cdc_event = receive_event(true);
        if !rollbacked {
            for event in cdc_event.get_events() {
                for entry in event.get_entries().get_entries() {
                    match entry.get_type() {
                        EventLogType::Rollback => rollbacked = true,
                        _ => unreachable!(),
                    }
                }
            }
        } else {
            let resolved_ts = cdc_event.get_resolved_ts().get_ts();
            if resolved_ts > 5 {
                return;
            }
        }
    }
    panic!("resolved ts must be advanced correctly");
}

#[test]
fn test_cdc_pessimistic_lock_unlock() {
    let mut cluster = new_server_cluster(0, 1);
    configure_for_lease_read(&mut cluster.cfg, Some(100), Some(10));
    cluster.pd_client.disable_default_operator();
    let mut suite = TestSuiteBuilder::new().cluster(cluster).build();
    let region = suite.cluster.get_region(&[]);
    let rid = region.id;
    let cf_tso = block_on(suite.cluster.pd_client.get_tso()).unwrap();

    let (mut req_tx, _, receive_event) = new_event_feed_v2(suite.get_region_cdc_client(rid));
    let mut req = suite.new_changedata_request(rid);
    req.request_id = 1;
    req.checkpoint_ts = cf_tso.into_inner();
    req.filter_loop = true;
    req.set_start_key(Key::from_raw(b"a").into_encoded());
    req.set_end_key(Key::from_raw(b"z").into_encoded());
    block_on(req_tx.send((req, WriteFlags::default()))).unwrap();

    let cdc_event = receive_event(false);
    'WaitInit: for event in cdc_event.get_events() {
        for entry in event.get_entries().get_entries() {
            match entry.get_type() {
                EventLogType::Prewrite => {}
                EventLogType::Initialized => break 'WaitInit,
                _ => unreachable!(),
            }
        }
    }

    let start_tso = cf_tso.next();
    let k = b"key".to_vec();
    let v = vec![b'x'; 16 * 1024];
    let mut mutation = Mutation::default();
    mutation.set_op(Op::PessimisticLock);
    mutation.key = k.clone();
    mutation.value = v;
    let for_update_tso = TimeStamp::from(start_tso.into_inner() + 10);
    suite.must_acquire_pessimistic_lock(rid, vec![mutation], k.clone(), start_tso, for_update_tso);
    std::thread::sleep(Duration::from_millis(500));

    suite.must_release_pessimistic_lock(rid, k.clone(), start_tso, for_update_tso);
    std::thread::sleep(Duration::from_millis(500));
}
