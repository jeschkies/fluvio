use std::{
    env::temp_dir,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

use flate2::{Compression, bufread::GzEncoder};
use fluvio_controlplane_metadata::{
    partition::Replica,
    smartmodule::{SmartModule, SmartModuleWasm, SmartModuleWasmFormat},
};
use fluvio_storage::{FileReplica, ReplicaStorage};
use flv_util::fixture::ensure_clean_dir;
use futures_util::{Future, StreamExt};

use fluvio_future::timer::sleep;
use fluvio_socket::{FluvioSocket, MultiplexerSocket};
use dataplane::{
    Isolation,
    fetch::DefaultFetchRequest,
    fixture::BatchProducer,
    record::{RecordData, Record},
};
use dataplane::fixture::{create_batch, TEST_RECORD};
use dataplane::smartstream::SmartStreamType;
use fluvio_spu_schema::server::{
    stream_fetch::{SmartModuleInvocation, SmartModuleInvocationWasm},
    update_offset::{UpdateOffsetsRequest, OffsetUpdate},
};
use fluvio_spu_schema::server::stream_fetch::SmartStreamWasm;
use fluvio_spu_schema::server::stream_fetch::SmartStreamPayload;
use crate::core::GlobalContext;
use crate::config::SpuConfig;
use crate::replication::leader::LeaderReplicaState;
use crate::services::public::create_public_server;

use std::sync::Arc;

use tracing::{debug};

use dataplane::{
    ErrorCode,
    api::{RequestMessage},
    record::RecordSet,
    SmartStreamError,
};
use fluvio_spu_schema::server::stream_fetch::{DefaultStreamFetchRequest, SmartStreamKind};

static NEXT_PORT: AtomicU16 = AtomicU16::new(12000);

#[fluvio_future::test(ignore)]
async fn test_stream_fetch() {
    let test_path = temp_dir().join("test_stream_fetch");
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path;
    let ctx = GlobalContext::new_shared_context(spu_config);

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    for version in 10..11 {
        let topic = format!("test{}", version);
        let test = Replica::new((topic.clone(), 0), 5001, vec![5001]);
        let test_id = test.id.clone();
        let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
            .await
            .expect("replica");
        ctx.leaders_state().insert(test_id, replica.clone());

        let stream_request = DefaultStreamFetchRequest {
            topic: topic.clone(),
            partition: 0,
            fetch_offset: 0,
            isolation: Isolation::ReadUncommitted,
            max_bytes: 1000,
            ..Default::default()
        };

        let mut stream = client_socket
            .create_stream(RequestMessage::new_request(stream_request), version)
            .await
            .expect("create stream");

        let mut records = RecordSet::default().add(create_batch());
        // write records, base offset = 0 since we are starting from 0
        replica
            .write_record_set(&mut records, ctx.follower_notifier())
            .await
            .expect("write");

        let response = stream.next().await.expect("first").expect("response");
        debug!("response: {:#?}", response);
        let stream_id = response.stream_id;
        {
            debug!("received first message");
            assert_eq!(response.topic, topic);

            let partition = &response.partition;
            assert_eq!(partition.error_code, ErrorCode::None);
            assert_eq!(partition.high_watermark, 2);
            assert_eq!(partition.next_offset_for_fetch(), Some(2)); // shoule be same as HW

            assert_eq!(partition.records.batches.len(), 1);
            let batch = &partition.records.batches[0];
            assert_eq!(batch.base_offset, 0);
            assert_eq!(batch.get_last_offset(), 1);
            assert_eq!(batch.records().len(), 2);
            assert_eq!(batch.records()[0].value().as_ref(), TEST_RECORD);
            assert_eq!(batch.records()[1].value().as_ref(), TEST_RECORD);
            assert_eq!(batch.records()[1].get_offset_delta(), 1);
        }

        drop(response);

        // consumer can send back to same offset to read back again
        debug!("send back offset ack to SPU");
        client_socket
            .send_and_receive(RequestMessage::new_request(UpdateOffsetsRequest {
                offsets: vec![OffsetUpdate {
                    offset: 1,
                    session_id: stream_id,
                }],
            }))
            .await
            .expect("send offset");

        let response = stream.next().await.expect("2nd").expect("response");
        {
            debug!("received 2nd message");
            assert_eq!(response.topic, topic);
            let partition = &response.partition;
            assert_eq!(partition.error_code, ErrorCode::None);
            assert_eq!(partition.high_watermark, 2);
            assert_eq!(partition.next_offset_for_fetch(), Some(2)); // shoule be same as HW

            // we got whole batch rather than individual batches
            assert_eq!(partition.records.batches.len(), 1);
            let batch = &partition.records.batches[0];
            assert_eq!(batch.base_offset, 0);
            assert_eq!(batch.get_last_offset(), 1);
            assert_eq!(batch.records().len(), 2);
            assert_eq!(batch.records()[0].value().as_ref(), TEST_RECORD);
            assert_eq!(batch.records()[1].value().as_ref(), TEST_RECORD);
        }

        drop(response);

        // send back that consume has processed all current bacthes
        client_socket
            .send_and_receive(RequestMessage::new_request(UpdateOffsetsRequest {
                offsets: vec![OffsetUpdate {
                    offset: 2,
                    session_id: stream_id,
                }],
            }))
            .await
            .expect("send offset");

        debug!("writing 2nd batch");
        // base offset should be 2
        replica
            .write_record_set(&mut records, ctx.follower_notifier())
            .await
            .expect("write");
        assert_eq!(replica.hw(), 4);

        let response = stream.next().await.expect("first").expect("response");
        debug!("received 3nd response");
        assert_eq!(response.stream_id, stream_id);
        assert_eq!(response.topic, topic);

        {
            let partition = &response.partition;
            assert_eq!(partition.error_code, ErrorCode::None);
            assert_eq!(partition.high_watermark, 4);

            assert_eq!(partition.next_offset_for_fetch(), Some(4));
            assert_eq!(partition.records.batches.len(), 1);
            let batch = &partition.records.batches[0];
            assert_eq!(batch.base_offset, 2);
            assert_eq!(batch.get_last_offset(), 3);
            assert_eq!(batch.records().len(), 2);
            assert_eq!(batch.records()[0].value().as_ref(), TEST_RECORD);
            assert_eq!(batch.records()[1].value().as_ref(), TEST_RECORD);
        }
    }

    server_end_event.notify();
    debug!("terminated controller");
}

/// create records that can be filtered
fn create_filter_records(records: u16) -> RecordSet {
    BatchProducer::builder()
        .records(records)
        .record_generator(Arc::new(generate_record))
        .build()
        .expect("batch")
        .records()
}

fn generate_record(record_index: usize, _producer: &BatchProducer) -> Record {
    let msg = match record_index {
        0 => "b".repeat(100),
        1 => "a".repeat(100),
        _ => "z".repeat(100),
    };

    Record::new(RecordData::from(msg))
}

fn read_filter_from_path(filter_path: impl AsRef<Path>) -> Vec<u8> {
    let path = filter_path.as_ref();
    std::fs::read(path).unwrap_or_else(|_| panic!("Unable to read file {}", path.display()))
}

fn zip(raw_buffer: Vec<u8>) -> Vec<u8> {
    use std::io::Read;
    let mut encoder = GzEncoder::new(raw_buffer.as_slice(), Compression::default());
    let mut buffer = Vec::with_capacity(raw_buffer.len());
    encoder
        .read_to_end(&mut buffer)
        .unwrap_or_else(|_| panic!("Unable to gzip file"));
    buffer
}

fn read_wasm_module(module_name: &str) -> Vec<u8> {
    let spu_dir = std::env::var("CARGO_MANIFEST_DIR").expect("target");
    let wasm_path = PathBuf::from(spu_dir)
        .parent()
        .expect("parent")
        .join(format!(
            "fluvio-smartstream/examples/target/wasm32-unknown-unknown/release/{}.wasm",
            module_name
        ));
    read_filter_from_path(wasm_path)
}

fn load_wasm_module<S: ReplicaStorage>(ctx: &GlobalContext<S>, module_name: &str) {
    let wasm = zip(read_wasm_module(module_name));
    ctx.smart_module_localstore().insert(SmartModule {
        name: module_name.to_owned(),
        wasm: SmartModuleWasm {
            format: SmartModuleWasmFormat::Binary,
            payload: wasm,
        },
        ..Default::default()
    });
}

async fn legacy_test<Fut, TestFn>(
    test_name: &str,
    module_name: &str,
    stream_kind: SmartStreamKind,
    test_fn: TestFn,
) where
    Fut: Future<Output = ()>,
    TestFn: FnOnce(
        Arc<GlobalContext<FileReplica>>,
        PathBuf,
        Option<SmartStreamPayload>,
        Option<SmartModuleInvocation>,
    ) -> Fut,
{
    let test_path = temp_dir().join(test_name);
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    let wasm = read_wasm_module(module_name);
    let wasm_payload = SmartStreamPayload {
        wasm: SmartStreamWasm::Raw(wasm),
        kind: stream_kind,
        ..Default::default()
    };

    test_fn(ctx, test_path, Some(wasm_payload), None).await
}

async fn adhoc_test<Fut, TestFn>(
    test_name: &str,
    module_name: &str,
    stream_kind: SmartStreamKind,
    test_fn: TestFn,
) where
    Fut: Future<Output = ()>,
    TestFn: FnOnce(
        Arc<GlobalContext<FileReplica>>,
        PathBuf,
        Option<SmartStreamPayload>,
        Option<SmartModuleInvocation>,
    ) -> Fut,
{
    let test_path = temp_dir().join(test_name);
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    let wasm = zip(read_wasm_module(module_name));
    let smart_module = SmartModuleInvocation {
        wasm: SmartModuleInvocationWasm::AdHoc(wasm),
        kind: stream_kind,
        ..Default::default()
    };

    test_fn(ctx, test_path, None, Some(smart_module)).await
}

async fn predefined_test<Fut, TestFn>(
    test_name: &str,
    module_name: &str,
    stream_kind: SmartStreamKind,
    test_fn: TestFn,
) where
    Fut: Future<Output = ()>,
    TestFn: FnOnce(
        Arc<GlobalContext<FileReplica>>,
        PathBuf,
        Option<SmartStreamPayload>,
        Option<SmartModuleInvocation>,
    ) -> Fut,
{
    let test_path = temp_dir().join(test_name);
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    load_wasm_module(&ctx, module_name);
    let smart_module = SmartModuleInvocation {
        wasm: SmartModuleInvocationWasm::Predefined(module_name.to_owned()),
        kind: stream_kind,
        ..Default::default()
    };

    test_fn(ctx, test_path, None, Some(smart_module)).await
}

const FLUVIO_WASM_FILTER: &str = "fluvio_wasm_filter";

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_legacy() {
    legacy_test(
        "test_stream_fetch_filter_legacy",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_fetch_filter,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_adhoc() {
    adhoc_test(
        "test_stream_fetch_filter_adhoc",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_fetch_filter,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_predefined() {
    predefined_test(
        "test_stream_fetch_filter_predefined",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_fetch_filter,
    )
    .await;
}

async fn test_stream_fetch_filter(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions

    let topic = "testfilter";

    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    // 1 out of 2 are filtered
    let mut records = create_filter_records(2);
    //debug!("records: {:#?}", records);
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    debug!("first filter fetch");
    let response = stream.next().await.expect("first").expect("response");
    //debug!("respose: {:#?}", response);
    let stream_id = response.stream_id;
    {
        debug!("received first message");
        assert_eq!(response.topic, topic);

        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 2);
        assert_eq!(partition.next_offset_for_fetch(), Some(2)); // shoule be same as HW

        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 0);
        assert_eq!(batch.records().len(), 1);
        assert_eq!(
            batch.records()[0].value().as_ref(),
            "a".repeat(100).as_bytes()
        );
        assert_eq!(batch.records()[0].get_offset_delta(), 1);
    }

    drop(response);

    // firt write 2 non filterable records
    let mut records = RecordSet::default().add(create_batch());
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    // another 1 of 3, here base offset should be = 4
    let mut records = create_filter_records(3);
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    // create another 4, base should be 4 + 3 = 7 and total 10 records
    let mut records = create_filter_records(3);
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");
    assert_eq!(replica.hw(), 10);

    debug!("2nd filter batch, hw=10");
    // consumer can send back to same offset to read back again
    debug!("send back offset ack to SPU");
    client_socket
        .send_and_receive(RequestMessage::new_request(UpdateOffsetsRequest {
            offsets: vec![OffsetUpdate {
                offset: 2,
                session_id: stream_id,
            }],
        }))
        .await
        .expect("send offset");

    let response = stream.next().await.expect("2nd").expect("response");
    {
        debug!("received 2nd message");
        assert_eq!(response.topic, topic);
        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 10);
        assert_eq!(partition.next_offset_for_fetch(), Some(10)); // shoule be same as HW

        // we got whole batch rather than individual batches
        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 4); // first base offset where we had filtered records
        assert_eq!(batch.records().len(), 2);
        assert_eq!(
            batch.records()[0].value().as_ref(),
            "a".repeat(100).as_bytes()
        );
    }

    drop(response);

    server_end_event.notify();
    debug!("terminated controller");
}

const FLUVIO_WASM_FILTER_ODD: &str = "fluvio_wasm_filter_odd";

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_individual_legacy() {
    legacy_test(
        "test_stream_fetch_filter_individual_legacy",
        FLUVIO_WASM_FILTER_ODD,
        SmartStreamKind::Filter,
        test_stream_fetch_filter_individual,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_individual_adhoc() {
    adhoc_test(
        "test_stream_fetch_filter_individual_adhoc",
        FLUVIO_WASM_FILTER_ODD,
        SmartStreamKind::Filter,
        test_stream_fetch_filter_individual,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_individual_predefined() {
    predefined_test(
        "test_stream_fetch_filter_individual_predefined",
        FLUVIO_WASM_FILTER_ODD,
        SmartStreamKind::Filter,
        test_stream_fetch_filter_individual,
    )
    .await;
}

async fn test_stream_fetch_filter_individual(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    let topic = "testfilter";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    // First, open the consumer stream
    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let mut records: RecordSet = BatchProducer::builder()
        .records(1u16)
        .record_generator(Arc::new(|_, _| Record::new("1")))
        .build()
        .expect("batch")
        .records();
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    tokio::select! {
        _ = stream.next() => panic!("Should not receive response here"),
        _ = fluvio_future::timer::sleep(std::time::Duration::from_millis(1000)) => (),
    }

    let mut records: RecordSet = BatchProducer::builder()
        .records(1u16)
        .record_generator(Arc::new(|_, _| Record::new("2")))
        .build()
        .expect("batch")
        .records();
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let response = stream.next().await.expect("first").expect("response");
    let records = response.partition.records.batches[0].records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].value.as_ref(), "2".as_bytes());

    match response.partition.error_code {
        ErrorCode::None => (),
        _ => panic!("Should not have gotten an error"),
    }

    drop(response);

    server_end_event.notify();
    debug!("terminated controller");
}

#[fluvio_future::test(ignore)]
async fn test_stream_filter_error_fetch_legacy() {
    legacy_test(
        "test_stream_filter_error_fetch_legacy",
        FLUVIO_WASM_FILTER_ODD,
        SmartStreamKind::Filter,
        test_stream_filter_error_fetch,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_filter_error_fetch_adhoc() {
    adhoc_test(
        "test_stream_filter_error_fetch_adhoc",
        FLUVIO_WASM_FILTER_ODD,
        SmartStreamKind::Filter,
        test_stream_filter_error_fetch,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_filter_error_fetch_predefined() {
    predefined_test(
        "test_stream_filter_error_fetch_predefined",
        FLUVIO_WASM_FILTER_ODD,
        SmartStreamKind::Filter,
        test_stream_filter_error_fetch,
    )
    .await;
}

async fn test_stream_filter_error_fetch(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions

    let topic = "test_filter_error";

    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    fn generate_record(record_index: usize, _producer: &BatchProducer) -> Record {
        let value = if record_index < 10 {
            record_index.to_string()
        } else {
            "ten".to_string()
        };

        Record::new(value)
    }

    let mut records: RecordSet = BatchProducer::builder()
        .records(11u16)
        .record_generator(Arc::new(generate_record))
        .build()
        .expect("batch")
        .records();

    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    debug!("first filter fetch");
    let response = stream.next().await.expect("first").expect("response");

    assert_eq!(response.partition.records.batches.len(), 1);
    let records = response.partition.records.batches[0].records();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0].value.as_ref(), "0".as_bytes());
    assert_eq!(records[1].value.as_ref(), "2".as_bytes());
    assert_eq!(records[2].value.as_ref(), "4".as_bytes());
    assert_eq!(records[3].value.as_ref(), "6".as_bytes());
    assert_eq!(records[4].value.as_ref(), "8".as_bytes());

    match &response.partition.error_code {
        ErrorCode::SmartStreamError(SmartStreamError::Runtime(error)) => {
            assert_eq!(error.offset, 10);
            assert!(error.record_key.is_none());
            assert_eq!(error.record_value.as_ref(), "ten".as_bytes());
            assert_eq!(error.kind, SmartStreamType::Filter);
            let rendered = format!("{}", error);
            assert_eq!(rendered, "Oops something went wrong\n\nCaused by:\n   0: Failed to parse int\n   1: invalid digit found in string\n\nSmartStream Info: \n    Type: Filter\n    Offset: 10\n    Key: NULL\n    Value: ten");
        }
        _ => panic!("should have gotten error code"),
    }

    drop(response);

    server_end_event.notify();
    debug!("terminated controller");
}

#[fluvio_future::test(ignore)]
async fn test_stream_filter_max_legacy() {
    legacy_test(
        "test_stream_filter_max_legacy",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_filter_max,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_filter_max_adhoc() {
    adhoc_test(
        "test_stream_filter_max_adhoc",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_filter_max,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_filter_max_predefined() {
    predefined_test(
        "test_stream_filter_max_predefined",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_filter_max,
    )
    .await;
}

/// test filter with max bytes
async fn test_stream_filter_max(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions

    let topic = "testfilter";

    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    // write 2 batches each with 10 records
    //debug!("records: {:#?}", records);
    replica
        .write_record_set(&mut create_filter_records(10), ctx.follower_notifier())
        .await
        .expect("write"); // 1000 bytes
    replica
        .write_record_set(&mut create_filter_records(10), ctx.follower_notifier())
        .await
        .expect("write"); // 2000 bytes totals
    replica
        .write_record_set(&mut create_filter_records(10), ctx.follower_notifier())
        .await
        .expect("write"); // 3000 bytes total
                          // now total of 300 filter records bytes (min), but last filter record is greater than max

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 250,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let response = stream.next().await.expect("first").expect("response");
    debug!("respose: {:#?}", response);

    // received partial because we exceed max bytes
    let stream_id = response.stream_id;
    {
        debug!("received first message");
        assert_eq!(response.topic, topic);

        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 30);
        assert_eq!(partition.next_offset_for_fetch(), Some(20)); // shoule be same as HW

        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 0);
        assert_eq!(batch.records().len(), 2);
        assert_eq!(
            batch.records()[0].value().as_ref(),
            "a".repeat(100).as_bytes()
        );
    }

    drop(response);

    // consumer can send back to same offset to read back again
    debug!("send back offset ack to SPU");
    client_socket
        .send_and_receive(RequestMessage::new_request(UpdateOffsetsRequest {
            offsets: vec![OffsetUpdate {
                offset: 20,
                session_id: stream_id,
            }],
        }))
        .await
        .expect("send offset");

    let response = stream.next().await.expect("2nd").expect("response");
    {
        debug!("received 2nd message");
        assert_eq!(response.topic, topic);
        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 30);
        assert_eq!(partition.next_offset_for_fetch(), Some(30)); // shoule be same as HW

        // we got whole batch rather than individual batches
        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 20);
        assert_eq!(batch.records().len(), 1);
        assert_eq!(
            batch.records()[0].value().as_ref(),
            "a".repeat(100).as_bytes()
        );
    }

    drop(response);

    server_end_event.notify();
}

const FLUVIO_WASM_MAP_DOUBLE: &str = "fluvio_wasm_map_double";

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_map_error_legacy() {
    legacy_test(
        "test_stream_fetch_map_error_legacy",
        FLUVIO_WASM_MAP_DOUBLE,
        SmartStreamKind::Map,
        test_stream_fetch_map_error,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_map_error_adhoc() {
    adhoc_test(
        "test_stream_fetch_map_error_legacy",
        FLUVIO_WASM_MAP_DOUBLE,
        SmartStreamKind::Map,
        test_stream_fetch_map_error,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_map_error_predefined() {
    predefined_test(
        "test_stream_fetch_map_error_legacy",
        FLUVIO_WASM_MAP_DOUBLE,
        SmartStreamKind::Map,
        test_stream_fetch_map_error,
    )
    .await;
}

async fn test_stream_fetch_map_error(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions

    let topic = "test_map_error";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let mut records: RecordSet = BatchProducer::builder()
        .records(10u16)
        .record_generator(Arc::new(|i, _| {
            if i < 9 {
                Record::new(i.to_string())
            } else {
                Record::new("nine".to_string())
            }
        }))
        .build()
        .expect("batch")
        .records();

    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    debug!("first map fetch");
    let response = stream.next().await.expect("first").expect("response");

    assert_eq!(response.partition.records.batches.len(), 1);
    let records = response.partition.records.batches[0].records();
    assert_eq!(records.len(), 9);
    assert_eq!(records[0].value.as_ref(), "0".as_bytes());
    assert_eq!(records[1].value.as_ref(), "2".as_bytes());
    assert_eq!(records[2].value.as_ref(), "4".as_bytes());
    assert_eq!(records[3].value.as_ref(), "6".as_bytes());
    assert_eq!(records[4].value.as_ref(), "8".as_bytes());
    assert_eq!(records[5].value.as_ref(), "10".as_bytes());
    assert_eq!(records[6].value.as_ref(), "12".as_bytes());
    assert_eq!(records[7].value.as_ref(), "14".as_bytes());
    assert_eq!(records[8].value.as_ref(), "16".as_bytes());

    match &response.partition.error_code {
        ErrorCode::SmartStreamError(SmartStreamError::Runtime(error)) => {
            assert_eq!(error.offset, 9);
            assert_eq!(error.kind, SmartStreamType::Map);
            assert_eq!(error.record_value.as_ref(), "nine".as_bytes());
        }
        _ => panic!("should get runtime error"),
    }

    drop(response);

    server_end_event.notify();
    debug!("terminated controller");
}

const FLUVIO_WASM_AGGREGATE: &str = "fluvio_wasm_aggregate";

#[fluvio_future::test(ignore)]
async fn test_stream_aggregate_fetch_single_batch_legacy() {
    legacy_test(
        "test_stream_aggregate_fetch_single_batch_legacy",
        FLUVIO_WASM_AGGREGATE,
        SmartStreamKind::Aggregate {
            accumulator: Vec::from("A"),
        },
        test_stream_aggregate_fetch_single_batch,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_aggregate_fetch_single_batch_adhoc() {
    adhoc_test(
        "test_stream_aggregate_fetch_single_batch_adhoc",
        FLUVIO_WASM_AGGREGATE,
        SmartStreamKind::Aggregate {
            accumulator: Vec::from("A"),
        },
        test_stream_aggregate_fetch_single_batch,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_aggregate_fetch_single_batch_predefined() {
    predefined_test(
        "test_stream_aggregate_fetch_single_batch_predefined",
        FLUVIO_WASM_AGGREGATE,
        SmartStreamKind::Aggregate {
            accumulator: Vec::from("A"),
        },
        test_stream_aggregate_fetch_single_batch,
    )
    .await;
}

async fn test_stream_aggregate_fetch_single_batch(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    let topic = "testaggregate";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    // Aggregate 5 records
    // These records look like:
    //
    // 1
    // 2
    // 3
    // 4
    // 5
    let mut records = BatchProducer::builder()
        .records(5u16)
        .record_generator(Arc::new(|i, _| Record::new(i.to_string())))
        .build()
        .expect("batch")
        .records();
    debug!("records: {:#?}", records);

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    debug!("first aggregate fetch");
    let response = stream.next().await.expect("first").expect("response");
    let stream_id = response.stream_id;

    {
        debug!("received first message");
        assert_eq!(response.topic, topic);

        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 5);
        assert_eq!(partition.next_offset_for_fetch(), Some(5)); // shoule be same as HW

        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 0);
        assert_eq!(batch.records().len(), 5);

        let records = batch.records();

        assert_eq!("A0", records[0].value().as_str().expect("string"));
        assert_eq!("A01", records[1].value().as_str().expect("string"));
        assert_eq!("A012", records[2].value().as_str().expect("string"));
        assert_eq!("A0123", records[3].value().as_str().expect("string"));
        assert_eq!("A01234", records[4].value().as_str().expect("string"));
    }

    // consumer can send back to same offset to read back again
    debug!("send back offset ack to SPU");
    client_socket
        .send_and_receive(RequestMessage::new_request(UpdateOffsetsRequest {
            offsets: vec![OffsetUpdate {
                offset: 20,
                session_id: stream_id,
            }],
        }))
        .await
        .expect("send offset");

    server_end_event.notify();
}

#[fluvio_future::test(ignore)]
async fn test_stream_aggregate_fetch_multiple_batch_legacy() {
    legacy_test(
        "test_stream_aggregate_fetch_multiple_batch_legacy",
        FLUVIO_WASM_AGGREGATE,
        SmartStreamKind::Aggregate {
            accumulator: Vec::from("A"),
        },
        test_stream_aggregate_fetch_multiple_batch,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_aggregate_fetch_multiple_batch_adhoc() {
    adhoc_test(
        "test_stream_aggregate_fetch_multiple_batch_adhoc",
        FLUVIO_WASM_AGGREGATE,
        SmartStreamKind::Aggregate {
            accumulator: Vec::from("A"),
        },
        test_stream_aggregate_fetch_multiple_batch,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_aggregate_fetch_multiple_batch_predefined() {
    predefined_test(
        "test_stream_aggregate_fetch_multiple_batch_predefined",
        FLUVIO_WASM_AGGREGATE,
        SmartStreamKind::Aggregate {
            accumulator: Vec::from("A"),
        },
        test_stream_aggregate_fetch_multiple_batch,
    )
    .await;
}

async fn test_stream_aggregate_fetch_multiple_batch(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    let topic = "testaggregatebatch";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    // Aggregate 5 records
    // These records look like:
    //
    // 1
    // 2
    // 3
    // 4
    // 5
    let mut records = BatchProducer::builder()
        .records(1u16)
        .record_generator(Arc::new(|_, _| Record::new("0")))
        .build()
        .expect("batch")
        .records();
    debug!("first batch: {:#?}", records);

    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let mut records2 = BatchProducer::builder()
        .records(1u16)
        .record_generator(Arc::new(|_, _| Record::new("1")))
        .build()
        .expect("batch")
        .records();

    debug!("2nd batch: {:#?}", records2);

    replica
        .write_record_set(&mut records2, ctx.follower_notifier())
        .await
        .expect("write");

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    debug!("first aggregate fetch");
    let response = stream.next().await.expect("first").expect("response");
    let stream_id = response.stream_id;

    {
        debug!("received first message");
        assert_eq!(response.topic, topic);

        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 2);
        assert_eq!(partition.next_offset_for_fetch(), Some(2)); // shoule be same as HW

        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 0);
        assert_eq!(batch.records().len(), 2);

        let records = batch.records();

        assert_eq!("A0", records[0].value().as_str().expect("string"));
        assert_eq!("A1", records[1].value().as_str().expect("string"));
        //   assert_eq!("A2", records[2].value().as_str().expect("string"));
        //   assert_eq!("A3", records[3].value().as_str().expect("string"));
        //   assert_eq!("A4", records[4].value().as_str().expect("string"));
    }

    // consumer can send back to same offset to read back again
    debug!("send back offset ack to SPU");
    client_socket
        .send_and_receive(RequestMessage::new_request(UpdateOffsetsRequest {
            offsets: vec![OffsetUpdate {
                offset: 20,
                session_id: stream_id,
            }],
        }))
        .await
        .expect("send offset");

    server_end_event.notify();
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_and_new_request_adhoc() {
    adhoc_test(
        "test_stream_fetch_and_new_request_adhoc",
        FLUVIO_WASM_FILTER,
        SmartStreamKind::Filter,
        test_stream_fetch_and_new_request,
    )
    .await;
}

async fn test_stream_fetch_and_new_request(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::shared(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    let topic = "test_stream_fetch_and_new_request";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let _stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let fetch_request = DefaultFetchRequest::default();
    let response = client_socket
        .send_and_receive(RequestMessage::new_request(fetch_request))
        .await;

    assert!(response.is_ok());

    server_end_event.notify();
    debug!("terminated controller");
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_invalid_wasm_module_legacy() {
    let test_path = temp_dir().join("test_stream_fetch_invalid_wasm_module_legacy");
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    let wasm = Vec::from("Hello, world, I'm not a valid WASM module!");
    let wasm_payload = SmartStreamPayload {
        wasm: SmartStreamWasm::Raw(wasm),
        kind: SmartStreamKind::Filter,
        ..Default::default()
    };

    test_stream_fetch_invalid_wasm_module(ctx, test_path, Some(wasm_payload), None).await
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_invalid_wasm_module_adhoc() {
    let test_path = temp_dir().join("test_stream_fetch_invalid_wasm_module_adhoc");
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    let wasm = zip(Vec::from("Hello, world, I'm not a valid WASM module!"));
    let smart_module = SmartModuleInvocation {
        wasm: SmartModuleInvocationWasm::AdHoc(wasm),
        kind: SmartStreamKind::Filter,
        ..Default::default()
    };

    test_stream_fetch_invalid_wasm_module(ctx, test_path, None, Some(smart_module)).await
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_invalid_wasm_module_predefined() {
    let test_path = temp_dir().join("test_stream_fetch_invalid_wasm_module_predefined");
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);

    let wasm = zip(Vec::from("Hello, world, I'm not a valid WASM module!"));
    ctx.smart_module_localstore().insert(SmartModule {
        name: "invalid_wasm".to_owned(),
        wasm: SmartModuleWasm {
            format: SmartModuleWasmFormat::Binary,
            payload: wasm,
        },
        ..Default::default()
    });

    let smart_module = SmartModuleInvocation {
        wasm: SmartModuleInvocationWasm::Predefined("invalid_wasm".to_owned()),
        kind: SmartStreamKind::Filter,
        ..Default::default()
    };

    test_stream_fetch_invalid_wasm_module(ctx, test_path, None, Some(smart_module)).await
}

async fn test_stream_fetch_invalid_wasm_module(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::shared(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    let topic = "test_invalid_wasm";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let response = stream
        .next()
        .await
        .expect("should get response")
        .expect("response should be Ok");

    assert_eq!(
        response.partition.error_code,
        ErrorCode::SmartStreamError(SmartStreamError::InvalidSmartStreamModule(
            "Filter".to_string(),
            "failed to parse WebAssembly module".to_string()
        ))
    );

    server_end_event.notify();
    debug!("terminated controller");
}

const FLUVIO_WASM_ARRAY_MAP_ARRAY: &str = "fluvio_wasm_array_map_array";

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_array_map_legacy() {
    legacy_test(
        "test_stream_fetch_array_map_legacy",
        FLUVIO_WASM_ARRAY_MAP_ARRAY,
        SmartStreamKind::ArrayMap,
        test_stream_fetch_array_map,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_array_map_adhoc() {
    adhoc_test(
        "test_stream_fetch_array_map_adhoc",
        FLUVIO_WASM_ARRAY_MAP_ARRAY,
        SmartStreamKind::ArrayMap,
        test_stream_fetch_array_map,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_array_map_predefined() {
    predefined_test(
        "test_stream_fetch_array_map_predefined",
        FLUVIO_WASM_ARRAY_MAP_ARRAY,
        SmartStreamKind::ArrayMap,
        test_stream_fetch_array_map,
    )
    .await;
}

async fn test_stream_fetch_array_map(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::shared(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    let topic = "test_array_map";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    // Input: One JSON record with 10 ints: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
    let mut records = BatchProducer::builder()
        .records(1u16)
        .record_generator(Arc::new(|_, _| {
            let nums = (0..10).collect::<Vec<_>>();
            Record::new(serde_json::to_string(&nums).unwrap())
        }))
        .build()
        .expect("batch")
        .records();

    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let response = stream
        .next()
        .await
        .expect("should get response")
        .expect("response should be Ok");

    assert_eq!(response.partition.records.batches.len(), 1);
    let batch = &response.partition.records.batches[0];

    // Output: 10 records containing integers 0-9
    for (i, record) in batch.records().iter().enumerate() {
        assert_eq!(
            record.value.as_ref(),
            RecordData::from(i.to_string()).as_ref()
        );
    }

    server_end_event.notify();
    debug!("terminated controller");
}

const FLUVIO_WASM_FILTER_MAP: &str = "fluvio_wasm_filter_map";

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_map_legacy() {
    legacy_test(
        "test_stream_fetch_filter_map_legacy",
        FLUVIO_WASM_FILTER_MAP,
        SmartStreamKind::FilterMap,
        test_stream_fetch_filter_map,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_map_adhoc() {
    adhoc_test(
        "test_stream_fetch_filter_map_adhoc",
        FLUVIO_WASM_FILTER_MAP,
        SmartStreamKind::FilterMap,
        test_stream_fetch_filter_map,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_map_predefined() {
    predefined_test(
        "test_stream_fetch_filter_map_predefined",
        FLUVIO_WASM_FILTER_MAP,
        SmartStreamKind::FilterMap,
        test_stream_fetch_filter_map,
    )
    .await;
}

async fn test_stream_fetch_filter_map(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::shared(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    let topic = "test_filter_map";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    // Input: the following records:
    //
    // 11
    // 22
    // 33
    // 44
    // 55
    let mut records = BatchProducer::builder()
        .records(5u16)
        .record_generator(Arc::new(|i, _| Record::new(((i + 1) * 11).to_string())))
        .build()
        .expect("batch")
        .records();

    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let response = stream
        .next()
        .await
        .expect("should get response")
        .expect("response should be Ok");

    assert_eq!(response.partition.records.batches.len(), 1);
    let batch = &response.partition.records.batches[0];
    assert_eq!(batch.records().len(), 2);

    // Output:
    //
    // 11 -> _
    // 22 -> 11
    // 33 -> _
    // 44 -> 22
    // 55 -> _
    let records = batch.records();
    assert_eq!(records[0].value, RecordData::from(11.to_string()));
    assert_eq!(records[1].value, RecordData::from(22.to_string()));

    server_end_event.notify();
    debug!("terminated controller");
}

const FLUVIO_WASM_FILTER_WITH_PARAMETERS: &str = "fluvio_wasm_filter_with_parameters";

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_with_params_legacy() {
    legacy_test(
        "test_stream_fetch_filter_with_params_legacy",
        FLUVIO_WASM_FILTER_WITH_PARAMETERS,
        SmartStreamKind::Filter,
        test_stream_fetch_filter_with_params,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_with_params_adhoc() {
    adhoc_test(
        "test_stream_fetch_filter_with_params_adhoc",
        FLUVIO_WASM_FILTER_WITH_PARAMETERS,
        SmartStreamKind::Filter,
        test_stream_fetch_filter_with_params,
    )
    .await;
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_filter_with_params_predefined() {
    predefined_test(
        "test_stream_fetch_filter_with_params_predefined",
        FLUVIO_WASM_FILTER_WITH_PARAMETERS,
        SmartStreamKind::Filter,
        test_stream_fetch_filter_with_params,
    )
    .await;
}

async fn test_stream_fetch_filter_with_params(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    use std::collections::BTreeMap;
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::new(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    let topic = "testfilter_with_params";

    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let mut params = BTreeMap::new();
    params.insert("key".to_string(), "b".to_string());

    let wasm_payload_with_params = wasm_payload.clone().map(|mut w| {
        w.params = params.clone().into();
        w
    });

    let smart_module_with_params = smart_module.clone().map(|mut w| {
        w.params = params.into();
        w
    });

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload: wasm_payload_with_params,
        smart_module: smart_module_with_params,
        ..Default::default()
    };

    // 1 out of 2 are filtered
    let mut records = create_filter_records(2);
    replica
        .write_record_set(&mut records, ctx.follower_notifier())
        .await
        .expect("write");

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    debug!("first filter fetch");
    let response = stream.next().await.expect("first").expect("response");
    {
        debug!("received first message");
        assert_eq!(response.topic, topic);

        let partition = &response.partition;

        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 0);
        assert_eq!(batch.records().len(), 1);
        assert_eq!(
            batch.records()[0].value().as_ref(),
            "b".repeat(100).as_bytes()
        );
        assert_eq!(batch.records()[0].get_offset_delta(), 0);

        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 2);
        assert_eq!(partition.next_offset_for_fetch(), Some(2));

        assert_eq!(partition.records.batches.len(), 1);
    }

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    debug!("second filter fetch");
    let response = stream.next().await.expect("first").expect("response");
    {
        debug!("received first message");
        assert_eq!(response.topic, topic);

        let partition = &response.partition;
        assert_eq!(partition.error_code, ErrorCode::None);
        assert_eq!(partition.high_watermark, 2);
        assert_eq!(partition.next_offset_for_fetch(), Some(2));

        assert_eq!(partition.records.batches.len(), 1);
        let batch = &partition.records.batches[0];
        assert_eq!(batch.base_offset, 0);
        assert_eq!(batch.records().len(), 1);
        assert_eq!(
            batch.records()[0].value().as_ref(),
            "a".repeat(100).as_bytes()
        );
        assert_eq!(batch.records()[0].get_offset_delta(), 1);
    }

    server_end_event.notify();
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_invalid_smartstream_legacy() {
    let test_path = temp_dir().join("test_stream_fetch_invalid_smartstream_legacy");
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    let wasm = include_bytes!("test_data/filter_missing_attribute.wasm").to_vec();
    let wasm_payload = SmartStreamPayload {
        wasm: SmartStreamWasm::Raw(wasm),
        kind: SmartStreamKind::Filter,
        ..Default::default()
    };

    test_stream_fetch_invalid_smartstream(ctx, test_path, Some(wasm_payload), None).await
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_invalid_smartstream_adhoc() {
    let test_path = temp_dir().join("test_stream_fetch_invalid_smartstream_adhoc");
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);
    let wasm = zip(include_bytes!("test_data/filter_missing_attribute.wasm").to_vec());
    let smart_module = SmartModuleInvocation {
        wasm: SmartModuleInvocationWasm::AdHoc(wasm),
        kind: SmartStreamKind::Filter,
        ..Default::default()
    };

    test_stream_fetch_invalid_smartstream(ctx, test_path, None, Some(smart_module)).await
}

#[fluvio_future::test(ignore)]
async fn test_stream_fetch_invalid_smartstream_predefined() {
    let test_path = temp_dir().join("test_stream_fetch_invalid_smartstream_predefined");
    let mut spu_config = SpuConfig::default();
    spu_config.log.base_dir = test_path.clone();

    let ctx = GlobalContext::new_shared_context(spu_config);

    let wasm = zip(include_bytes!("test_data/filter_missing_attribute.wasm").to_vec());
    ctx.smart_module_localstore().insert(SmartModule {
        name: "invalid_wasm".to_owned(),
        wasm: SmartModuleWasm {
            format: SmartModuleWasmFormat::Binary,
            payload: wasm,
        },
        ..Default::default()
    });

    let smart_module = SmartModuleInvocation {
        wasm: SmartModuleInvocationWasm::Predefined("invalid_wasm".to_owned()),
        kind: SmartStreamKind::Filter,
        ..Default::default()
    };

    test_stream_fetch_invalid_smartstream(ctx, test_path, None, Some(smart_module)).await
}

async fn test_stream_fetch_invalid_smartstream(
    ctx: Arc<GlobalContext<FileReplica>>,
    test_path: PathBuf,
    wasm_payload: Option<SmartStreamPayload>,
    smart_module: Option<SmartModuleInvocation>,
) {
    ensure_clean_dir(&test_path);

    let addr = format!("127.0.0.1:{}", NEXT_PORT.fetch_add(1, Ordering::Relaxed));

    let server_end_event = create_public_server(addr.to_owned(), ctx.clone()).run();

    // wait for stream controller async to start
    sleep(Duration::from_millis(100)).await;

    let client_socket =
        MultiplexerSocket::shared(FluvioSocket::connect(&addr).await.expect("connect"));

    // perform for two versions
    let topic = "test_invalid_smartstream";
    let test = Replica::new((topic.to_owned(), 0), 5001, vec![5001]);
    let test_id = test.id.clone();
    let replica = LeaderReplicaState::create(test, ctx.config(), ctx.status_update_owned())
        .await
        .expect("replica");
    ctx.leaders_state().insert(test_id, replica.clone());

    let stream_request = DefaultStreamFetchRequest {
        topic: topic.to_owned(),
        partition: 0,
        fetch_offset: 0,
        isolation: Isolation::ReadUncommitted,
        max_bytes: 10000,
        wasm_module: Vec::new(),
        wasm_payload,
        smart_module,
        ..Default::default()
    };

    let mut stream = client_socket
        .create_stream(RequestMessage::new_request(stream_request), 11)
        .await
        .expect("create stream");

    let response = stream
        .next()
        .await
        .expect("should get response")
        .expect("response should be Ok");

    match response.partition.error_code {
        ErrorCode::SmartStreamError(SmartStreamError::InvalidSmartStreamModule(name, _reason)) => {
            assert_eq!(name, "Filter");
        }
        _ => panic!("expected an UndefinedSmartStreamModule error"),
    }

    server_end_event.notify();
    debug!("terminated controller");
}
