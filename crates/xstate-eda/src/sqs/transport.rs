//! The native operations over an SQS client: `send`, `fetch`, `ack` / delete and `extend`, plus
//! the SDK error mapping.
//!
//! Python reference: `contrib/brokers/sqs.py` (`SqsTransport`).
//!
//! ```text
//! queue      {topic}                       resolved once with GetQueueUrl (never created)
//! body       Envelope::to_json             structured-mode CloudEvents JSON (the size cap applies)
//! attribute  ce_type (String) = envelope type
//! FIFO       {topic} ends with ".fifo":    MessageGroupId = subject or "_",
//!                                          MessageDeduplicationId = envelope id
//! receive    MaxNumberOfMessages = batch (1-10), WaitTimeSeconds <= 20,
//!            AttributeNames = ApproximateReceiveCount, optional VisibilityTimeout
//! attempts   ApproximateReceiveCount - 1
//! settle     ack and nack(requeue = false) = DeleteMessage
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use aws_config::{BehaviorVersion, Region};
use aws_sdk_sqs::Client;
use aws_sdk_sqs::config::Credentials;
use aws_sdk_sqs::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_sqs::operation::receive_message::builders::ReceiveMessageFluentBuilder;
use aws_sdk_sqs::types::{MessageAttributeValue, MessageSystemAttributeName};

use crate::async_core::{Raw, Transport, into_publish};
use crate::envelope::Envelope;
use crate::error::BrokerError;

/// SQS refuses a `ReceiveMessage` of more than 10 messages.
pub(super) const MAX_BATCH: usize = 10;
/// The longest SQS long poll, in seconds.
const MAX_WAIT_S: u64 = 20;
/// The visibility timeout SQS applies when none is given (30 s).
pub(super) const DEFAULT_VISIBILITY: Duration = Duration::from_secs(30);
/// The attribute that carries the envelope type, for consumers that filter without parsing.
const TYPE_ATTRIBUTE: &str = "ce_type";
/// The longest message in an error text taken from the service.
const MAX_SERVICE_MESSAGE: usize = 200;

/// Static credentials, never echoed.
#[derive(Clone)]
pub(super) struct StaticCredentials {
    pub(super) access_key_id: String,
    pub(super) secret_access_key: String,
    pub(super) session_token: Option<String>,
}

/// Where the client comes from.
#[derive(Clone)]
pub(super) struct ClientConfig {
    pub(super) endpoint_url: Option<String>,
    pub(super) region: Option<String>,
    pub(super) credentials: Option<StaticCredentials>,
}

impl ClientConfig {
    /// Build the client from the SDK's environment-based configuration plus the overrides.
    async fn connect(&self) -> Client {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = &self.region {
            loader = loader.region(Region::new(region.clone()));
        }
        if let Some(c) = &self.credentials {
            loader = loader.credentials_provider(Credentials::new(
                c.access_key_id.clone(),
                c.secret_access_key.clone(),
                c.session_token.clone(),
                None,
                "xstate-eda",
            ));
        }
        if let Some(endpoint) = &self.endpoint_url {
            loader = loader.endpoint_url(endpoint.clone());
        }
        Client::new(&loader.load().await)
    }
}

/// Where the transport gets its client.
pub(super) enum Source {
    /// Build one lazily on first use.
    Config(ClientConfig),
    /// A client the caller manages (Python `client=`).
    Client,
}

/// The transport's settings (the broker options after validation).
#[derive(Debug, Clone)]
pub(super) struct Settings {
    /// Per-receive visibility timeout in whole seconds; `None` = the queue's own.
    pub(super) visibility_secs: Option<i32>,
    pub(super) batch: usize,
    pub(super) max_bytes: usize,
    pub(super) io_timeout: Duration,
}

/// What settles one delivery: the queue and the receipt handle, plus the client to call with.
#[derive(Clone)]
pub(super) struct Native {
    client: Client,
    url: Arc<str>,
    receipt: String,
}

impl Native {
    /// Keep the message invisible for `secs` more (SQS `ChangeMessageVisibility`).
    pub(super) async fn extend(&self, secs: i32) -> Result<(), BrokerError> {
        self.client
            .change_message_visibility()
            .queue_url(&*self.url)
            .receipt_handle(&self.receipt)
            .visibility_timeout(secs)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| failed(&e))
    }

    async fn delete(&self) -> Result<(), BrokerError> {
        self.client
            .delete_message()
            .queue_url(&*self.url)
            .receipt_handle(&self.receipt)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| failed(&e))
    }
}

#[derive(Default)]
struct Cache {
    client: Option<Client>,
    urls: HashMap<String, Arc<str>>,
}

/// SQS operations over one client.
pub(super) struct SqsTransport {
    source: Source,
    settings: Settings,
    cache: Mutex<Cache>,
    /// Held while the client is built, so concurrent first callers share one client.
    building: tokio::sync::Mutex<()>,
}

/// A failure of the SDK, described without the endpoint, the credentials or the request.
fn describe<E: ProvideErrorMetadata, R>(error: &SdkError<E, R>) -> String {
    match error {
        SdkError::ServiceError(context) => {
            let meta = context.err().meta();
            let message: String =
                meta.message().unwrap_or_default().chars().take(MAX_SERVICE_MESSAGE).collect();
            format!("{}: {message}", meta.code().unwrap_or("service error"))
        }
        SdkError::TimeoutError(_) => "the request timed out".to_owned(),
        SdkError::DispatchFailure(failure) if failure.is_timeout() => {
            "the request timed out".to_owned()
        }
        SdkError::DispatchFailure(failure) if failure.is_io() => "connection failed".to_owned(),
        SdkError::DispatchFailure(_) => "the request could not be sent".to_owned(),
        SdkError::ResponseError(_) => "the response could not be read".to_owned(),
        SdkError::ConstructionFailure(_) => {
            "the request could not be built (is a region configured?)".to_owned()
        }
        _ => "the request failed".to_owned(),
    }
}

fn failed<E: ProvideErrorMetadata, R>(error: &SdkError<E, R>) -> BrokerError {
    BrokerError::Other(format!("sqs: {}", describe(error)))
}

fn publish_failed<E: ProvideErrorMetadata, R>(error: &SdkError<E, R>) -> BrokerError {
    BrokerError::Publish(format!("sqs: {}", describe(error)))
}

/// The attempt a receive count stands for: `ApproximateReceiveCount - 1` (a missing or
/// malformed count is a first delivery).
fn attempts_of(count: Option<&str>) -> u64 {
    count.and_then(|c| c.parse::<u64>().ok()).unwrap_or(1).saturating_sub(1)
}

impl SqsTransport {
    pub(super) fn new(source: Source, settings: Settings, client: Option<Client>) -> Self {
        Self {
            source,
            settings,
            cache: Mutex::new(Cache { client, urls: HashMap::new() }),
            building: tokio::sync::Mutex::new(()),
        }
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The SDK client, built on first use (serialised: of several concurrent first callers one
    /// builds it and the others reuse it).
    async fn client(&self) -> Result<Client, BrokerError> {
        if let Some(client) = self.cache().client.clone() {
            return Ok(client);
        }
        let Source::Config(config) = &self.source else {
            return Err(BrokerError::Other("sqs: the client was closed".to_owned()));
        };
        let _building = self.building.lock().await;
        // another caller may have built it while this one waited
        if let Some(client) = self.cache().client.clone() {
            return Ok(client);
        }
        let client = config.connect().await;
        self.cache().client = Some(client.clone());
        Ok(client)
    }

    /// The client and the URL of the queue named `topic` (cached).
    async fn queue(&self, topic: &str) -> Result<(Client, Arc<str>), BrokerError> {
        let client = self.client().await?;
        if let Some(url) = self.cache().urls.get(topic).cloned() {
            return Ok((client, url));
        }
        let output =
            client.get_queue_url().queue_name(topic).send().await.map_err(|e| failed(&e))?;
        let url: Arc<str> = output
            .queue_url()
            .ok_or_else(|| BrokerError::Other("sqs: the queue has no URL".to_owned()))?
            .into();
        self.cache().urls.insert(topic.to_owned(), Arc::clone(&url));
        Ok((client, url))
    }

    /// One `ReceiveMessage` (waiting `wait_s` seconds on the server) as raw messages.
    async fn receive(
        &self,
        client: &Client,
        url: &Arc<str>,
        wait_s: u64,
    ) -> Result<Vec<Raw<Native>>, BrokerError> {
        let mut request: ReceiveMessageFluentBuilder = client
            .receive_message()
            .queue_url(&**url)
            .max_number_of_messages(clamp_batch(self.settings.batch))
            .wait_time_seconds(i32::try_from(wait_s.min(MAX_WAIT_S)).unwrap_or(0))
            .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount);
        if let Some(secs) = self.settings.visibility_secs {
            request = request.visibility_timeout(secs);
        }
        let output = request.send().await.map_err(|e| failed(&e))?;
        let mut out = Vec::new();
        for message in output.messages() {
            let Some(receipt) = message.receipt_handle() else {
                // nothing to settle it with: SQS redelivers it after the visibility timeout
                tracing::warn!("an SQS message without a receipt handle was skipped");
                continue;
            };
            let count = message
                .attributes()
                .and_then(|a| a.get(&MessageSystemAttributeName::ApproximateReceiveCount));
            out.push(Raw {
                body: message.body().unwrap_or_default().as_bytes().to_vec(),
                attempts: attempts_of(count.map(String::as_str)),
                native: Native {
                    client: client.clone(),
                    url: Arc::clone(url),
                    receipt: receipt.to_owned(),
                },
            });
        }
        Ok(out)
    }
}

fn clamp_batch(batch: usize) -> i32 {
    i32::try_from(batch.clamp(1, MAX_BATCH)).unwrap_or(1)
}

impl Transport for SqsTransport {
    type Native = Native;

    const NAME: &'static str = "SQS";

    /// The visibility timeout: the clock after which SQS hands an un-acked message to someone else.
    fn redelivery_window(&self) -> Duration {
        self.settings
            .visibility_secs
            .map_or(DEFAULT_VISIBILITY, |s| Duration::from_secs(u64::try_from(s).unwrap_or(0)))
    }

    fn max_bytes(&self) -> usize {
        self.settings.max_bytes
    }

    fn io_timeout(&self) -> Duration {
        self.settings.io_timeout
    }

    /// `SendMessage` with the `ce_type` attribute; on a `.fifo` queue also the group (the subject)
    /// and the deduplication id (the envelope id).
    async fn send(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        let body = envelope.to_json(self.settings.max_bytes)?;
        let (client, url) = self.queue(topic).await.map_err(into_publish)?;
        let attribute = MessageAttributeValue::builder()
            .data_type("String")
            .string_value(&envelope.ty)
            .build()
            .map_err(|e| BrokerError::Publish(format!("sqs: {e}")))?;
        let mut request = client
            .send_message()
            .queue_url(&*url)
            .message_body(body)
            .message_attributes(TYPE_ATTRIBUTE, attribute);
        if topic.strip_suffix(".fifo").is_some() {
            let group = envelope.subject.as_deref().filter(|s| !s.is_empty()).unwrap_or("_");
            request = request.message_group_id(group).message_deduplication_id(&envelope.id);
        }
        request.send().await.map(|_| ()).map_err(|e| publish_failed(&e))
    }

    /// What is ready; `wait` is a long poll of whole seconds (at most 20). A sub-second `wait`
    /// is a short poll, a pause of `wait` and a second short poll, so a caller with a small
    /// budget neither spins on the service nor overshoots its deadline.
    async fn fetch(&self, topic: &str, wait: Duration) -> Result<Vec<Raw<Native>>, BrokerError> {
        let (client, url) = self.queue(topic).await?;
        let secs = wait.as_secs();
        let out = self.receive(&client, &url, secs).await?;
        if !out.is_empty() || secs > 0 || wait.is_zero() {
            return Ok(out);
        }
        tokio::time::sleep(wait).await;
        self.receive(&client, &url, 0).await
    }

    async fn ack(&self, native: &Native) -> Result<(), BrokerError> {
        native.delete().await
    }

    /// `DeleteMessage` as well: dead-lettering is the dispatcher's job.
    async fn drop_message(&self, native: &Native) -> Result<(), BrokerError> {
        native.delete().await
    }

    /// Drop the cached client and queue URLs (a client the caller manages is left alone).
    fn close(&self) -> impl Future<Output = Result<(), BrokerError>> + Send {
        {
            let mut cache = self.cache();
            cache.urls.clear();
            if matches!(self.source, Source::Config(_)) {
                cache.client = None;
            }
        }
        std::future::ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attempt_is_the_receive_count_minus_one() {
        assert_eq!(attempts_of(Some("1")), 0);
        assert_eq!(attempts_of(Some("4")), 3);
        assert_eq!(attempts_of(None), 0);
        assert_eq!(attempts_of(Some("0")), 0);
        assert_eq!(attempts_of(Some("junk")), 0);
    }

    #[test]
    fn the_batch_is_clamped_to_what_sqs_accepts() {
        assert_eq!((clamp_batch(0), clamp_batch(5), clamp_batch(50)), (1, 5, 10));
    }
}
