//! The four native operations over a NATS `JetStream` context: `send`, `fetch`, `ack` and `term`,
//! plus the subject naming.
//!
//! Python reference: `contrib/brokers/nats.py` (`NatsTransport`).
//!
//! ```text
//! stream     {topic}                      subjects [{topic}.>]
//! subject    {topic}.{subject token}      the envelope subject, sanitised
//! consumer   {durable}                    one durable pull consumer per (topic, durable)
//! headers    Nats-Msg-Id    the envelope id (JetStream's duplicate window dedups re-publishes)
//!            content-type   application/cloudevents+json; charset=UTF-8
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::pin::pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_nats::ServerAddr;
use async_nats::header::{HeaderMap, NATS_MESSAGE_ID};
use async_nats::jetstream::consumer::{AckPolicy, PullConsumer, pull};
use async_nats::jetstream::{self, AckKind, Context, stream};
use futures_util::StreamExt;

use super::ConnectFactory;
use crate::async_core::{CE_CONTENT_TYPE, Raw, Transport, into_publish};
use crate::envelope::Envelope;
use crate::error::BrokerError;

/// What settles one delivery: the `JetStream` message.
pub(super) type Native = jetstream::Message;

/// The shortest server-side wait of a pull request: a smaller remaining poll budget still waits
/// this long (the poll loop re-checks its deadline afterwards).
const MIN_PULL_WAIT: Duration = Duration::from_millis(10);

/// The transport's settings (the broker options after validation).
#[derive(Debug, Clone)]
pub(super) struct Settings {
    pub(super) durable: String,
    pub(super) ack_wait: Duration,
    pub(super) batch: usize,
    pub(super) max_bytes: usize,
    pub(super) io_timeout: Duration,
}

/// Where the `JetStream` context comes from.
pub(super) enum Source {
    /// Connect lazily to these servers (with these connection options, if any).
    Servers { addrs: Vec<ServerAddr>, connect_with: Option<ConnectFactory> },
    /// A context the caller manages (Python `js=`): never closed by the adapter.
    Context,
}

#[derive(Default)]
struct Cache {
    context: Option<Context>,
    /// `true` when the adapter opened the connection (and so closes it).
    owned: bool,
    streams: HashSet<String>,
    consumers: HashMap<String, PullConsumer>,
}

/// `JetStream` operations for one durable consumer name.
pub(super) struct NatsTransport {
    source: Source,
    pub(super) settings: Settings,
    cache: Mutex<Cache>,
    /// Held while connecting, so concurrent first callers share one connection.
    connecting: tokio::sync::Mutex<()>,
}

/// A NATS-safe single token for an envelope subject: `.`, `*`, `>` and whitespace become `_`,
/// and an empty subject is `_` (Python `subject_token`).
///
/// ```
/// assert_eq!(xstate_eda::nats::subject_token(Some("order.1 *")), "order_1__");
/// assert_eq!(xstate_eda::nats::subject_token(None), "_");
/// ```
#[must_use]
pub fn subject_token(subject: Option<&str>) -> String {
    let token: String = subject.unwrap_or_default().chars().map(unsafe_to_underscore).collect();
    if token.is_empty() { "_".to_owned() } else { token }
}

/// Python's `\s` matches `str.isspace()`: Unicode whitespace plus the four ASCII separators
/// `\x1c`-`\x1f`, which `char::is_whitespace` does not cover.
fn unsafe_to_underscore(c: char) -> char {
    if matches!(c, '.' | '*' | '>') || c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c) {
        '_'
    } else {
        c
    }
}

fn failed(error: impl Display) -> BrokerError {
    BrokerError::Other(format!("nats: {error}"))
}

fn publish_failed(error: impl Display) -> BrokerError {
    BrokerError::Publish(format!("nats: {error}"))
}

impl NatsTransport {
    pub(super) fn new(source: Source, settings: Settings, context: Option<Context>) -> Self {
        let cache = Cache { context, ..Cache::default() };
        Self { source, settings, cache: Mutex::new(cache), connecting: tokio::sync::Mutex::new(()) }
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `JetStream` context, connecting on first use. Connecting is serialised: of several
    /// concurrent first callers one connects and the others reuse its connection.
    async fn context(&self) -> Result<Context, BrokerError> {
        if let Some(context) = self.cache().context.clone() {
            return Ok(context);
        }
        let Source::Servers { addrs, connect_with } = &self.source else {
            return Err(failed("the JetStream context was closed"));
        };
        let _connecting = self.connecting.lock().await;
        // another caller may have connected while this one waited
        if let Some(context) = self.cache().context.clone() {
            return Ok(context);
        }
        let options = connect_with.as_ref().map_or_else(async_nats::ConnectOptions::new, |f| f());
        let client = options.connect(addrs.clone()).await.map_err(failed)?;
        let context = jetstream::new(client);
        let mut cache = self.cache();
        cache.context = Some(context.clone());
        cache.owned = true;
        Ok(context)
    }

    /// The `JetStream` stream `topic` (capturing `topic.>`), created on first use.
    async fn stream(&self, topic: &str) -> Result<(Context, stream::Stream), BrokerError> {
        let context = self.context().await?;
        let stream = context
            .get_or_create_stream(stream::Config {
                name: topic.to_owned(),
                subjects: vec![format!("{topic}.>")],
                ..stream::Config::default()
            })
            .await
            .map_err(failed)?;
        self.cache().streams.insert(topic.to_owned());
        Ok((context, stream))
    }

    /// The durable pull consumer of `topic`, created on first use.
    async fn consumer(&self, topic: &str) -> Result<PullConsumer, BrokerError> {
        if let Some(consumer) = self.cache().consumers.get(topic).cloned() {
            return Ok(consumer);
        }
        let (_, stream) = self.stream(topic).await?;
        let durable = self.settings.durable.clone();
        let consumer = stream
            .get_or_create_consumer(
                &durable,
                pull::Config {
                    durable_name: Some(durable.clone()),
                    ack_wait: self.settings.ack_wait,
                    ack_policy: AckPolicy::Explicit,
                    filter_subject: format!("{topic}.>"),
                    ..pull::Config::default()
                },
            )
            .await
            .map_err(failed)?;
        self.cache().consumers.insert(topic.to_owned(), consumer.clone());
        Ok(consumer)
    }

    /// Send an ack-kind to the server and flush it, so a settled delivery is not left in the
    /// client's write buffer.
    async fn settle(native: &Native, kind: AckKind) -> Result<(), BrokerError> {
        native.ack_with(kind).await.map_err(failed)?;
        native.context.client().flush().await.map_err(failed)
    }
}

/// The messages of a pull request, with their redelivery counts.
async fn collect<S>(stream: S) -> Result<Vec<Raw<Native>>, BrokerError>
where
    S: futures_util::Stream<Item = Result<Native, async_nats::Error>>,
{
    let mut stream = pin!(stream);
    let mut out = Vec::new();
    while let Some(message) = stream.next().await {
        let message = message.map_err(failed)?;
        out.push(Raw {
            body: message.payload.to_vec(),
            attempts: attempts_of(&message),
            native: message,
        });
    }
    Ok(out)
}

/// Deliveries the server made before this one: `num_delivered - 1` (0 for a message without
/// `JetStream` metadata).
fn attempts_of(message: &Native) -> u64 {
    message
        .info()
        .ok()
        .and_then(|info| u64::try_from(info.delivered.saturating_sub(1)).ok())
        .unwrap_or(0)
}

impl Transport for NatsTransport {
    type Native = Native;

    const NAME: &'static str = "NATS";

    fn redelivery_window(&self) -> Duration {
        self.settings.ack_wait
    }

    fn max_bytes(&self) -> usize {
        self.settings.max_bytes
    }

    fn io_timeout(&self) -> Duration {
        self.settings.io_timeout
    }

    /// Publish on `{topic}.{subject token}` and wait for the stream's acknowledgement.
    async fn send(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        let body = envelope.to_json(self.settings.max_bytes)?;
        let (context, _) = self.stream(topic).await.map_err(into_publish)?;
        let mut headers = HeaderMap::new();
        headers.insert(NATS_MESSAGE_ID, envelope.id.as_str());
        headers.insert("content-type", CE_CONTENT_TYPE);
        let subject = format!("{topic}.{}", subject_token(envelope.subject.as_deref()));
        let ack = context
            .publish_with_headers(subject, headers, body.into_bytes().into())
            .await
            .map_err(publish_failed)?;
        ack.await.map(|_| ()).map_err(publish_failed)
    }

    /// First what is ready (no waiting); when nothing is, wait up to `wait` for one message.
    async fn fetch(&self, topic: &str, wait: Duration) -> Result<Vec<Raw<Native>>, BrokerError> {
        let consumer = self.consumer(topic).await?;
        let ready =
            consumer.fetch().max_messages(self.settings.batch).messages().await.map_err(failed)?;
        let out = collect(ready).await?;
        if !out.is_empty() || wait.is_zero() {
            return Ok(out);
        }
        let waiting = consumer
            .batch()
            .max_messages(1)
            .expires(wait.max(MIN_PULL_WAIT))
            .messages()
            .await
            .map_err(failed)?;
        collect(waiting).await
    }

    async fn ack(&self, native: &Native) -> Result<(), BrokerError> {
        Self::settle(native, AckKind::Ack).await
    }

    /// `+TERM`: never redelivered.
    async fn drop_message(&self, native: &Native) -> Result<(), BrokerError> {
        Self::settle(native, AckKind::Term).await
    }

    /// Drain a connection the adapter opened (a context the caller manages is left alone).
    async fn close(&self) -> Result<(), BrokerError> {
        let context = {
            let mut cache = self.cache();
            cache.streams.clear();
            cache.consumers.clear();
            if std::mem::take(&mut cache.owned) { cache.context.take() } else { None }
        };
        match context {
            Some(context) => context.client().drain().await.map_err(failed),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_replace_separators_and_wildcards_like_python() {
        assert_eq!(subject_token(Some("order:1")), "order:1");
        assert_eq!(subject_token(Some("a.b*c>d e\tf")), "a_b_c_d_e_f");
        assert_eq!(subject_token(Some("a\u{1c}b\u{a0}c")), "a_b_c", "Python's \\s");
        assert_eq!(subject_token(Some("")), "_");
        assert_eq!(subject_token(None), "_");
        assert_eq!(subject_token(Some("日本/4")), "日本/4");
    }
}
