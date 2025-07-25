/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: LicenseRef-SEL
 *
 * This file is subject to the Stalwart Enterprise License Agreement (SEL) and
 * is NOT open source software.
 *
 */

use std::{future::Future, time::Duration};

use ahash::{AHashMap, AHashSet};
use store::{
    Deserialize, IterateParams, Store, U64_LEN, ValueKey,
    write::{BatchBuilder, TelemetryClass, ValueClass, key::DeserializeBigEndian},
};
use trc::{
    AddContext, AuthEvent, Event, EventDetails, EventType, Key, MessageIngestEvent,
    OutgoingReportEvent, QueueEvent, Value,
    ipc::subscriber::SubscriberBuilder,
    serializers::binary::{deserialize_events, serialize_events},
};
use utils::snowflake::SnowflakeIdGenerator;

use crate::config::telemetry::StoreTracer;

const MAX_EVENTS: usize = 2048;

pub(crate) fn spawn_store_tracer(builder: SubscriberBuilder, settings: StoreTracer) {
    let (_, mut rx) = builder.register();
    tokio::spawn(async move {
        let mut active_spans = AHashMap::new();
        let store = settings.store;
        let mut batch = BatchBuilder::new();

        while let Some(events) = rx.recv().await {
            for event in events {
                if let Some(span) = &event.inner.span {
                    let span_id = span.span_id().unwrap();
                    if !event.inner.typ.is_span_end() {
                        let events = active_spans.entry(span_id).or_insert_with(Vec::new);
                        if events.len() < MAX_EVENTS {
                            events.push(event);
                        }
                    } else if let Some(events) = active_spans.remove(&span_id) {
                        let mut queue_ids = AHashSet::new();
                        let mut values = AHashSet::new();

                        for event in events.iter().chain([span, &event]) {
                            for (key, value) in &event.keys {
                                match (key, value) {
                                    (Key::QueueId, Value::UInt(queue_id)) => {
                                        queue_ids.insert(*queue_id);
                                    }
                                    (
                                        Key::From | Key::To | Key::Domain | Key::Hostname,
                                        Value::String(address),
                                    ) => {
                                        values.insert(address.clone());
                                    }
                                    (Key::To, Value::Array(value)) => {
                                        for value in value {
                                            if let Value::String(address) = value {
                                                values.insert(address.clone());
                                            }
                                        }
                                    }
                                    (Key::RemoteIp, Value::Ipv4(ip)) => {
                                        values.insert(ip.to_string().into());
                                    }
                                    (Key::RemoteIp, Value::Ipv6(ip)) => {
                                        values.insert(ip.to_string().into());
                                    }

                                    _ => {}
                                }
                            }
                        }

                        if !queue_ids.is_empty() {
                            // Serialize events
                            batch.set(
                                ValueClass::Telemetry(TelemetryClass::Span { span_id }),
                                serialize_events(
                                    [span.as_ref()]
                                        .into_iter()
                                        .chain(events.iter().map(|event| event.as_ref()))
                                        .chain([event.as_ref()].into_iter()),
                                    events.len() + 2,
                                ),
                            );

                            // Build index
                            batch.set(
                                ValueClass::Telemetry(TelemetryClass::Index {
                                    span_id,
                                    value: (span.inner.typ.code() as u16).to_be_bytes().to_vec(),
                                }),
                                vec![],
                            );
                            for queue_id in queue_ids {
                                batch.set(
                                    ValueClass::Telemetry(TelemetryClass::Index {
                                        span_id,
                                        value: queue_id.to_be_bytes().to_vec(),
                                    }),
                                    vec![],
                                );
                            }
                            for value in values {
                                batch.set(
                                    ValueClass::Telemetry(TelemetryClass::Index {
                                        span_id,
                                        value: value.as_bytes().to_vec(),
                                    }),
                                    vec![],
                                );
                            }
                        }
                    }
                }
            }

            if !batch.is_empty() {
                if let Err(err) = store.write(batch.build_all()).await {
                    trc::error!(err.caused_by(trc::location!()));
                }
                batch = BatchBuilder::new();
            }
        }
    });
}

pub enum TracingQuery {
    EventType(EventType),
    QueueId(u64),
    Keywords(String),
}

pub trait TracingStore: Sync + Send {
    fn get_span(
        &self,
        span_id: u64,
    ) -> impl Future<Output = trc::Result<Vec<Event<EventDetails>>>> + Send;
    fn get_raw_span(
        &self,
        span_id: u64,
    ) -> impl Future<Output = trc::Result<Option<Vec<u8>>>> + Send;
    fn query_spans(
        &self,
        params: &[TracingQuery],
        from_span_id: u64,
        to_span_id: u64,
    ) -> impl Future<Output = trc::Result<Vec<u64>>> + Send;
    fn purge_spans(&self, period: Duration) -> impl Future<Output = trc::Result<()>> + Send;
}

impl TracingStore for Store {
    async fn get_span(&self, span_id: u64) -> trc::Result<Vec<Event<EventDetails>>> {
        self.get_value::<Span>(ValueKey::from(ValueClass::Telemetry(
            TelemetryClass::Span { span_id },
        )))
        .await
        .caused_by(trc::location!())
        .map(|span| span.map(|span| span.0).unwrap_or_default())
    }

    async fn get_raw_span(&self, span_id: u64) -> trc::Result<Option<Vec<u8>>> {
        self.get_value::<RawSpan>(ValueKey::from(ValueClass::Telemetry(
            TelemetryClass::Span { span_id },
        )))
        .await
        .caused_by(trc::location!())
        .map(|span| span.map(|span| span.0))
    }

    async fn query_spans(
        &self,
        params: &[TracingQuery],
        from_span_id: u64,
        to_span_id: u64,
    ) -> trc::Result<Vec<u64>> {
        let mut spans = SpanCollector::Empty;
        let num_params = params.len();

        for (param_num, param) in params.iter().enumerate() {
            let (value, exact_len) = match param {
                TracingQuery::EventType(event) => (
                    (event.code() as u16).to_be_bytes().to_vec(),
                    std::mem::size_of::<u16>() + U64_LEN,
                ),
                TracingQuery::QueueId(id) => (
                    id.to_be_bytes().to_vec(),
                    std::mem::size_of::<u64>() + U64_LEN,
                ),
                TracingQuery::Keywords(value) => {
                    if let Some(value) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                        (value.as_bytes().to_vec(), value.len() + U64_LEN)
                    } else {
                        (value.as_bytes().to_vec(), 0)
                    }
                }
            };

            let mut param_spans = SpanCollector::new(num_params);
            self.iterate(
                IterateParams::new(
                    ValueKey::from(ValueClass::Telemetry(TelemetryClass::Index {
                        span_id: 0,
                        value: value.clone(),
                    })),
                    ValueKey::from(ValueClass::Telemetry(TelemetryClass::Index {
                        span_id: u64::MAX,
                        value,
                    })),
                )
                .no_values(),
                |key, _| {
                    if exact_len == 0 || key.len() == exact_len {
                        let span_id = key
                            .deserialize_be_u64(key.len() - U64_LEN)
                            .caused_by(trc::location!())?;

                        if (from_span_id == 0 || span_id >= from_span_id)
                            && (to_span_id == 0 || span_id <= to_span_id)
                        {
                            param_spans.insert(span_id);
                        }
                    }

                    Ok(true)
                },
            )
            .await
            .caused_by(trc::location!())?;

            if param_num == 0 {
                spans = param_spans;
            } else if spans.intersect(param_spans) {
                return Ok(Vec::new());
            }
        }

        Ok(spans.into_vec())
    }

    async fn purge_spans(&self, period: Duration) -> trc::Result<()> {
        let until_span_id = SnowflakeIdGenerator::from_duration(period).ok_or_else(|| {
            trc::StoreEvent::UnexpectedError
                .caused_by(trc::location!())
                .ctx(trc::Key::Reason, "Failed to generate reference span id.")
        })?;

        self.delete_range(
            ValueKey::from(ValueClass::Telemetry(TelemetryClass::Span { span_id: 0 })),
            ValueKey::from(ValueClass::Telemetry(TelemetryClass::Span {
                span_id: until_span_id,
            })),
        )
        .await
        .caused_by(trc::location!())?;

        let mut delete_keys: Vec<ValueClass> = Vec::new();
        self.iterate(
            IterateParams::new(
                ValueKey::from(ValueClass::Telemetry(TelemetryClass::Index {
                    span_id: 0,
                    value: vec![],
                })),
                ValueKey::from(ValueClass::Telemetry(TelemetryClass::Index {
                    span_id: u64::MAX,
                    value: vec![u8::MAX; 16],
                })),
            )
            .no_values(),
            |key, _| {
                let span_id = key
                    .deserialize_be_u64(key.len() - U64_LEN)
                    .caused_by(trc::location!())?;
                if span_id < until_span_id {
                    delete_keys.push(ValueClass::Telemetry(TelemetryClass::Index {
                        span_id,
                        value: key[0..key.len() - U64_LEN].to_vec(),
                    }));
                }

                Ok(true)
            },
        )
        .await
        .caused_by(trc::location!())?;

        if !delete_keys.is_empty() {
            // Commit index
            let mut batch = BatchBuilder::new();

            for key in delete_keys {
                if batch.is_large_batch() {
                    self.write(batch.build_all()).await?;
                    batch = BatchBuilder::new();
                }
                batch.clear(key);
            }

            if !batch.is_empty() {
                self.write(batch.build_all()).await?;
            }
        }

        Ok(())
    }
}

enum SpanCollector {
    Vec(Vec<u64>),
    HashSet(AHashSet<u64>),
    Empty,
}

impl SpanCollector {
    fn new(num_params: usize) -> Self {
        if num_params == 1 {
            Self::Vec(Vec::new())
        } else {
            Self::HashSet(AHashSet::new())
        }
    }

    fn insert(&mut self, span_id: u64) {
        match self {
            Self::Vec(vec) => vec.push(span_id),
            Self::HashSet(set) => {
                set.insert(span_id);
            }
            _ => unreachable!(),
        }
    }

    fn into_vec(self) -> Vec<u64> {
        match self {
            Self::Vec(mut vec) => {
                vec.sort_unstable_by(|a, b| b.cmp(a));
                vec
            }
            Self::HashSet(set) => {
                let mut vec: Vec<u64> = set.into_iter().collect();
                vec.sort_unstable_by(|a, b| b.cmp(a));
                vec
            }
            Self::Empty => Vec::new(),
        }
    }

    fn intersect(&mut self, other_span: Self) -> bool {
        match (self, other_span) {
            (Self::HashSet(set), Self::HashSet(other_set)) => {
                set.retain(|span_id| other_set.contains(span_id));
                set.is_empty()
            }
            _ => unreachable!(),
        }
    }
}

impl StoreTracer {
    pub fn default_events() -> impl IntoIterator<Item = EventType> {
        EventType::variants().into_iter().filter(|event| {
            !event.is_raw_io()
                && matches!(
                    event,
                    EventType::MessageIngest(
                        MessageIngestEvent::Ham
                            | MessageIngestEvent::Spam
                            | MessageIngestEvent::Duplicate
                            | MessageIngestEvent::Error
                    ) | EventType::Smtp(_)
                        | EventType::Delivery(_)
                        | EventType::MtaSts(_)
                        | EventType::TlsRpt(_)
                        | EventType::Dane(_)
                        | EventType::Iprev(_)
                        | EventType::Spf(_)
                        | EventType::Dmarc(_)
                        | EventType::Dkim(_)
                        | EventType::MailAuth(_)
                        | EventType::Queue(
                            QueueEvent::QueueMessage
                                | QueueEvent::QueueMessageAuthenticated
                                | QueueEvent::QueueReport
                                | QueueEvent::QueueDsn
                                | QueueEvent::QueueAutogenerated
                                | QueueEvent::Rescheduled
                                | QueueEvent::RateLimitExceeded
                                | QueueEvent::ConcurrencyLimitExceeded
                                | QueueEvent::QuotaExceeded
                        )
                        | EventType::Limit(_)
                        | EventType::Tls(_)
                        | EventType::IncomingReport(_)
                        | EventType::OutgoingReport(
                            OutgoingReportEvent::SpfReport
                                | OutgoingReportEvent::SpfRateLimited
                                | OutgoingReportEvent::DkimReport
                                | OutgoingReportEvent::DkimRateLimited
                                | OutgoingReportEvent::DmarcReport
                                | OutgoingReportEvent::DmarcRateLimited
                                | OutgoingReportEvent::DmarcAggregateReport
                                | OutgoingReportEvent::TlsAggregate
                                | OutgoingReportEvent::HttpSubmission
                                | OutgoingReportEvent::UnauthorizedReportingAddress
                                | OutgoingReportEvent::ReportingAddressValidationError
                                | OutgoingReportEvent::NotFound
                                | OutgoingReportEvent::SubmissionError
                                | OutgoingReportEvent::NoRecipientsFound
                        )
                        | EventType::Auth(
                            AuthEvent::Success
                                | AuthEvent::Failed
                                | AuthEvent::TooManyAttempts
                                | AuthEvent::Error
                        )
                        | EventType::Sieve(_)
                        | EventType::Milter(_)
                        | EventType::MtaHook(_)
                        | EventType::Security(_)
                )
        })
    }
}

struct RawSpan(Vec<u8>);
struct Span(Vec<Event<EventDetails>>);

impl Deserialize for Span {
    fn deserialize(bytes: &[u8]) -> trc::Result<Self> {
        deserialize_events(bytes).map(Self)
    }
}

impl Deserialize for RawSpan {
    fn deserialize(bytes: &[u8]) -> trc::Result<Self> {
        Ok(Self(bytes.to_vec()))
    }
}
