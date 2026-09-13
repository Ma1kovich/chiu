use super::*;
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

fn unavailable_log() -> LocalLog {
    LocalLog::unavailable(crate::local_log::LoggingFailureStage::Open)
}

#[test]
fn first_observation_is_a_baseline_and_the_next_increase_is_activity() {
    let mut normalizer = ActivityNormalizer::new();

    assert_eq!(
        normalizer.observe(
            vec![InterfaceCounter::external(7, 10_000)],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Baseline
    );
    let NormalizedObservation::Sample(sample) = normalizer.observe(
        vec![InterfaceCounter::external(7, 12_500)],
        Duration::from_millis(1_250),
    ) else {
        panic!("continuous increase should produce a sample");
    };
    assert_eq!(sample.received_bytes(), 2_500);
    assert_eq!(sample.observed_over(), Duration::from_millis(1_250));
}

#[test]
fn normalized_samples_require_a_positive_trusted_interval() {
    assert!(ReceiveActivitySample::try_new(1, Duration::ZERO).is_none());
    assert!(ReceiveActivitySample::try_new(1, Duration::from_secs(6)).is_none());
    assert!(ReceiveActivitySample::try_new(1, MAX_TRUSTED_INTERVAL).is_some());
}

#[test]
fn loopback_counters_do_not_contribute_to_activity() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![
            InterfaceCounter::external(7, 1_000),
            InterfaceCounter::loopback(8, 2_000),
        ],
        Duration::from_secs(1),
    );

    assert_eq!(
        normalizer.observe(
            vec![
                InterfaceCounter::external(7, 1_100),
                InterfaceCounter::loopback(8, 9_000),
            ],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Sample(ReceiveActivitySample {
            received_bytes: 100,
            observed_over: Duration::from_secs(1),
        })
    );
}

#[test]
fn partial_topology_changes_preserve_continuous_interface_activity() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![
            InterfaceCounter::external(1, 1_000),
            InterfaceCounter::external(2, 2_000),
            InterfaceCounter::external(3, 3_000),
        ],
        Duration::from_secs(1),
    );

    assert_eq!(
        normalizer.observe(
            vec![
                InterfaceCounter::external(1, 1_100),
                InterfaceCounter::external(2, 500),
                InterfaceCounter::external(4, 40_000),
            ],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Sample(ReceiveActivitySample {
            received_bytes: 100,
            observed_over: Duration::from_secs(1),
        })
    );
    assert_eq!(
        normalizer.observe(
            vec![
                InterfaceCounter::external(1, 1_250),
                InterfaceCounter::external(2, 700),
                InterfaceCounter::external(4, 40_050),
            ],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Sample(ReceiveActivitySample {
            received_bytes: 400,
            observed_over: Duration::from_secs(1),
        })
    );
}

#[test]
fn diagnostics_distinguish_continuous_new_reset_and_removed_interfaces() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![
            InterfaceCounter::external(1, 100),
            InterfaceCounter::external(2, 200),
            InterfaceCounter::external(3, 300),
        ],
        Duration::from_secs(1),
    );
    let baseline = normalizer.diagnostics();
    assert_eq!(baseline.len(), 3);
    assert!(
        baseline
            .iter()
            .all(|interface| interface.continuity() == NetworkInterfaceContinuity::New)
    );

    normalizer.observe(
        vec![
            InterfaceCounter::external(1, 150),
            InterfaceCounter::external(2, 10),
            InterfaceCounter::external(4, 400),
        ],
        Duration::from_secs(1),
    );
    let diagnostics = normalizer.diagnostics();

    let continuous = diagnostics
        .iter()
        .find(|interface| interface.last_observed_receive_bytes() == 150)
        .expect("continuous interface should be projected");
    assert_eq!(
        continuous.continuity(),
        NetworkInterfaceContinuity::Continuous
    );
    assert_eq!(continuous.recent_delta(), Some(50));
    assert!(continuous.id() == baseline[0].id());

    let reset = diagnostics
        .iter()
        .find(|interface| interface.last_observed_receive_bytes() == 10)
        .expect("counter reset should be projected");
    assert_eq!(reset.continuity(), NetworkInterfaceContinuity::CounterReset);
    assert_eq!(reset.recent_delta(), None);
    assert!(reset.id() == baseline[1].id());

    let removed = diagnostics
        .iter()
        .find(|interface| interface.last_observed_receive_bytes() == 300)
        .expect("removed interface should be projected once");
    assert_eq!(removed.continuity(), NetworkInterfaceContinuity::Removed);
    assert_eq!(removed.recent_delta(), None);
    assert!(removed.id() == baseline[2].id());

    let added = diagnostics
        .iter()
        .find(|interface| interface.last_observed_receive_bytes() == 400)
        .expect("new interface should be projected");
    assert_eq!(added.continuity(), NetworkInterfaceContinuity::New);
    assert_eq!(added.recent_delta(), None);
    assert!(
        baseline
            .iter()
            .all(|interface| interface.id() != added.id())
    );

    normalizer.observe(
        vec![
            InterfaceCounter::external(1, 175),
            InterfaceCounter::external(2, 20),
            InterfaceCounter::external(4, 450),
        ],
        Duration::from_secs(1),
    );
    assert!(
        normalizer
            .diagnostics()
            .iter()
            .all(|interface| interface.continuity() != NetworkInterfaceContinuity::Removed),
        "removed context must expire after one later successful observation"
    );
}

#[test]
fn loopback_is_diagnostic_context_but_never_aggregate_activity() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![
            InterfaceCounter::external(1, 100),
            InterfaceCounter::loopback(2, 200),
        ],
        Duration::from_secs(1),
    );

    assert_eq!(
        normalizer.observe(
            vec![
                InterfaceCounter::external(1, 150),
                InterfaceCounter::loopback(2, 1_200),
            ],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Sample(ReceiveActivitySample {
            received_bytes: 50,
            observed_over: Duration::from_secs(1),
        })
    );
    let diagnostics = normalizer.diagnostics();
    let loopback = diagnostics
        .iter()
        .find(|interface| interface.last_observed_receive_bytes() == 1_200)
        .expect("loopback context should be projected");
    assert_eq!(
        loopback.continuity(),
        NetworkInterfaceContinuity::Continuous
    );
    assert_eq!(loopback.recent_delta(), Some(1_000));
    assert!(!loopback.included_in_activity());
}

#[test]
fn reused_numeric_identity_with_a_new_incarnation_is_rebaselined() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![
            InterfaceCounter::external_incarnation(7, b"en0", 1_000),
            InterfaceCounter::external(8, 2_000),
        ],
        Duration::from_secs(1),
    );

    assert_eq!(
        normalizer.observe(
            vec![
                InterfaceCounter::external_incarnation(7, b"bridge0", 80_000),
                InterfaceCounter::external(8, 2_250),
            ],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Sample(ReceiveActivitySample {
            received_bytes: 250,
            observed_over: Duration::from_secs(1),
        })
    );
}

#[test]
fn explicit_reset_makes_the_next_observation_baseline_only() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![InterfaceCounter::external(7, 1_000)],
        Duration::from_secs(1),
    );
    normalizer.reset();

    assert_eq!(
        normalizer.observe(
            vec![InterfaceCounter::external(7, 50_000)],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Baseline
    );
}

#[test]
fn an_untrusted_interval_rebaselines_without_emitting_activity() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![InterfaceCounter::external(7, 1_000)],
        Duration::from_secs(1),
    );

    assert_eq!(
        normalizer.observe(
            vec![InterfaceCounter::external(7, 100_000)],
            Duration::from_secs(6),
        ),
        NormalizedObservation::ContinuityLost(ContinuityLossReason::UntrustedInterval)
    );
    assert_eq!(
        normalizer.observe(
            vec![InterfaceCounter::external(7, 100_250)],
            Duration::from_secs(1),
        ),
        NormalizedObservation::Sample(ReceiveActivitySample {
            received_bytes: 250,
            observed_over: Duration::from_secs(1),
        })
    );
}

#[test]
fn aggregate_overflow_is_a_discontinuity_instead_of_wrapping() {
    let mut normalizer = ActivityNormalizer::new();
    normalizer.observe(
        vec![
            InterfaceCounter::external(1, 0),
            InterfaceCounter::external(2, 0),
        ],
        Duration::from_secs(1),
    );

    assert_eq!(
        normalizer.observe(
            vec![
                InterfaceCounter::external(1, u64::MAX),
                InterfaceCounter::external(2, 1),
            ],
            Duration::from_secs(1),
        ),
        NormalizedObservation::ContinuityLost(ContinuityLossReason::ArithmeticOverflow)
    );
}

#[test]
fn provider_failure_invalidates_continuity_and_recovery_rebaselines() {
    let mut sampling = SamplingCore::new();

    assert_eq!(
        sampling.process(
            Ok(vec![InterfaceCounter::external(7, 1_000)]),
            Duration::from_secs(1),
        ),
        vec![NetworkActivityEvent::AvailabilityChanged(
            NetworkActivityAvailability::Available,
        )]
    );
    assert_eq!(
        sampling.process(
            Ok(vec![InterfaceCounter::external(7, 1_100)]),
            Duration::from_secs(2),
        ),
        vec![NetworkActivityEvent::Sample(ReceiveActivitySample {
            received_bytes: 100,
            observed_over: Duration::from_secs(1),
        })]
    );
    assert_eq!(
        sampling.process(
            Err(Box::new(std::io::Error::other("unavailable"))),
            Duration::from_secs(3),
        ),
        vec![
            NetworkActivityEvent::ContinuityLost(ContinuityLossReason::ProviderUnavailable,),
            NetworkActivityEvent::AvailabilityChanged(NetworkActivityAvailability::Unavailable,),
        ]
    );
    assert!(
        sampling
            .process(
                Err(Box::new(std::io::Error::other("still unavailable"))),
                Duration::from_secs(4),
            )
            .is_empty()
    );
    assert_eq!(
        sampling.process(
            Ok(vec![InterfaceCounter::external(7, 50_000)]),
            Duration::from_secs(5),
        ),
        vec![NetworkActivityEvent::AvailabilityChanged(
            NetworkActivityAvailability::Available,
        )]
    );
    assert_eq!(
        sampling.process(
            Ok(vec![InterfaceCounter::external(7, 50_250)]),
            Duration::from_secs(6),
        ),
        vec![NetworkActivityEvent::Sample(ReceiveActivitySample {
            received_bytes: 250,
            observed_over: Duration::from_secs(1),
        })]
    );
}

struct BlockingProvider {
    started: mpsc::Sender<()>,
    proceed: mpsc::Receiver<()>,
}

struct StaticProvider;

struct DiagnosticProvider;

struct RecoveringProvider {
    read_count: usize,
}

impl CounterProvider for StaticProvider {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
        Ok(vec![InterfaceCounter::external(7, 1_000)])
    }
}

impl CounterProvider for DiagnosticProvider {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
        Ok(vec![
            InterfaceCounter::external(7, 1_000),
            InterfaceCounter::loopback(8, 2_000),
        ])
    }
}

impl CounterProvider for RecoveringProvider {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
        let result = match self.read_count {
            0 => Ok(vec![InterfaceCounter::external(7, 1_000)]),
            1 => Err(Box::new(std::io::Error::other("unavailable")) as BoxError),
            _ => Ok(vec![InterfaceCounter::external(7, 50_000)]),
        };
        self.read_count += 1;
        result
    }
}

impl CounterProvider for BlockingProvider {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
        self.started.send(()).unwrap();
        self.proceed.recv().unwrap();
        Ok(vec![InterfaceCounter::external(7, 1_000)])
    }
}

#[test]
fn accepted_reset_is_the_last_effective_event_from_an_in_flight_read() {
    let source = NetworkActivitySource::new(unavailable_log());
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded_events = events.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (proceed_tx, proceed_rx) = mpsc::channel();
    let mut runtime = source
        .start_with(
            BlockingProvider {
                started: started_tx,
                proceed: proceed_rx,
            },
            move |event| recorded_events.lock().unwrap().push(event),
        )
        .expect("source should start");
    started_rx.recv().expect("provider read should begin");

    let reset_source = source.clone();
    let reset = thread::spawn(move || reset_source.reset());
    proceed_tx.send(()).unwrap();
    reset
        .join()
        .expect("reset caller should not panic")
        .expect("reset should be accepted");

    assert_eq!(
        events.lock().unwrap().last(),
        Some(&NetworkActivityEvent::ContinuityLost(
            ContinuityLossReason::ExplicitReset,
        ))
    );
    assert!(
        source.snapshot().interfaces().is_empty(),
        "accepted reset must clear pre-reset diagnostic context"
    );
    runtime.shutdown().expect("source should stop");
}

#[test]
fn snapshot_exposes_provider_independent_context_and_retains_it_after_stop() {
    let source = NetworkActivitySource::new(unavailable_log());
    let (events, received) = mpsc::channel();
    let mut runtime = source
        .start_with(DiagnosticProvider, move |event| events.send(event).unwrap())
        .expect("source should start");
    assert_eq!(
        received.recv().unwrap(),
        NetworkActivityEvent::AvailabilityChanged(NetworkActivityAvailability::Available)
    );

    let snapshot = source.snapshot();
    let diagnostics = snapshot.interfaces();
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .all(|interface| interface.continuity() == NetworkInterfaceContinuity::New)
    );
    assert!(
        diagnostics
            .iter()
            .any(NetworkInterfaceDiagnostic::included_in_activity)
    );
    assert!(
        diagnostics
            .iter()
            .any(|interface| !interface.included_in_activity())
    );

    runtime.shutdown().expect("source should stop");
    assert_eq!(source.snapshot().interfaces().len(), 2);
}

#[test]
fn provider_failure_retains_context_and_recovery_restarts_interface_continuity() {
    let source = NetworkActivitySource::new(unavailable_log());
    let mut provider = RecoveringProvider { read_count: 0 };
    let mut consumer = |_| {};
    let mut sampling = SamplingCore::new();
    let origin = Instant::now();

    sample_once(
        &source.shared,
        &mut provider,
        &mut consumer,
        &mut sampling,
        origin,
    );
    let before_failure = source.snapshot();
    let original_id = before_failure.interfaces()[0].id();

    sample_once(
        &source.shared,
        &mut provider,
        &mut consumer,
        &mut sampling,
        origin,
    );
    let unavailable = source.snapshot();
    assert_eq!(
        unavailable.status(),
        NetworkActivitySourceStatus::Unavailable
    );
    assert_eq!(
        unavailable.latest_failure(),
        Some(NetworkActivityFailure::ProviderRead)
    );
    assert_eq!(unavailable.interfaces().len(), 1);
    assert!(unavailable.interfaces()[0].id() == original_id);

    sample_once(
        &source.shared,
        &mut provider,
        &mut consumer,
        &mut sampling,
        origin,
    );
    let recovered = source.snapshot();
    assert_eq!(recovered.status(), NetworkActivitySourceStatus::Available);
    assert_eq!(recovered.latest_failure(), None);
    assert_eq!(recovered.interfaces().len(), 1);
    assert_eq!(
        recovered.interfaces()[0].continuity(),
        NetworkInterfaceContinuity::New
    );
    assert_eq!(recovered.interfaces()[0].recent_delta(), None);
    assert!(recovered.interfaces()[0].id() != original_id);
}

#[test]
fn changing_receive_rates_do_not_persist_one_log_record_per_sample() {
    struct IncreasingProvider(u64);

    impl CounterProvider for IncreasingProvider {
        fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError> {
            self.0 += 1_000;
            Ok(vec![InterfaceCounter::external(7, self.0)])
        }
    }

    let (log, lines) = crate::local_log::LocalLog::recording();
    let source = NetworkActivitySource::new(log);
    let mut provider = IncreasingProvider(0);
    let mut consumer = |_| {};
    let mut sampling = SamplingCore::new();
    let origin = Instant::now();

    for _ in 0..10 {
        sample_once(
            &source.shared,
            &mut provider,
            &mut consumer,
            &mut sampling,
            origin,
        );
    }

    let persisted = lines.lock().unwrap().join("");
    assert_eq!(persisted.matches("network.availability").count(), 1);
    assert_eq!(persisted.matches("network.topology-changed").count(), 2);
    assert!(!persisted.contains("receive-rate"));
}

#[test]
fn source_owns_one_worker_and_shutdown_is_idempotent() {
    let source = NetworkActivitySource::new(unavailable_log());
    let (events, received) = mpsc::channel();
    let mut runtime = source
        .start_with(StaticProvider, move |event| events.send(event).unwrap())
        .expect("source should start");
    assert_eq!(
        received.recv().unwrap(),
        NetworkActivityEvent::AvailabilityChanged(NetworkActivityAvailability::Available)
    );
    assert_eq!(
        source.snapshot().status(),
        NetworkActivitySourceStatus::Available
    );
    assert_eq!(source.snapshot().latest_failure(), None);

    let duplicate = source.start_with(StaticProvider, |_| {});
    assert!(matches!(
        duplicate,
        Err(NetworkActivityRuntimeError::AlreadyStarted)
    ));

    runtime.shutdown().expect("first shutdown should succeed");
    runtime
        .shutdown()
        .expect("second shutdown should be a no-op");
    assert_eq!(
        source.snapshot().status(),
        NetworkActivitySourceStatus::Stopped
    );
    assert_eq!(
        source.reset(),
        Err(NetworkActivityControlError::Unavailable)
    );
}
