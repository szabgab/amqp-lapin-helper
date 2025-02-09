#[forbid(unsafe_code)]
#[macro_use]
extern crate tracing;

use std::path::Path;
pub use lapin::{
    message::Delivery, options::*, types::*, BasicProperties, Channel, Connection,
    ConnectionProperties, ExchangeKind, Queue,
};

pub mod message {
    pub use lapin::message::Delivery;
}

pub mod options {
    pub use lapin::options::*;
}

pub mod types {
    pub use lapin::types::*;
}

use async_trait::async_trait;
use bincode::ErrorKind;
use futures_lite::StreamExt;
use lapin::publisher_confirm::{Confirmation, PublisherConfirm};
use serde::Serialize;
use std::sync::Arc;
use once_cell::sync::Lazy;
use prometheus::{Histogram, HistogramVec, IntGaugeVec, opts, register_histogram, register_histogram_vec, register_int_gauge_vec};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, SemaphorePermit};
use tokio::task;
use tokio::task::JoinHandle;
use tokio_amqp::*;

pub type Requeue = bool;

pub type Result<E> = std::result::Result<E, Error>;
pub type ConsumeResult<E> = std::result::Result<E, Requeue>;

static STAT_CONCURRENT_TASK: Lazy<IntGaugeVec> = Lazy::new(|| {
    register_int_gauge_vec!(
        opts!(
            "amqp_consumer_concurrent_tasks",
            "Current/Max concurrent check",
        ),
        &["exchange_name", "kind"],
    ).unwrap()
});

const EXPONENTIAL_SECONDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

static STAT_CONSUMER_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "amqp_consumer_duration",
        "The duration of the consumer",
        &["exchange_name"],
        EXPONENTIAL_SECONDS.to_vec(),
    ).unwrap()
});

static STAT_PUBLISHER_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "amqp_publisher_duration",
        "The duration of the publisher",
        &["exchange_name", "routing_key"],
        EXPONENTIAL_SECONDS.to_vec(),
    ).unwrap()
});

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("acquire-semaphore: {0}")]
    AcquireSemaphore(#[from] AcquireError),

    #[error("AMQP: {0}")]
    Amqp(#[from] lapin::Error),

    #[error("Missing server ID")]
    MissingServerId,

    #[error("String UTF-8 error: {0}")]
    StringUtf8Error(#[from] std::string::FromUtf8Error),

    #[error("Bincode: {0}")]
    Bincode(#[from] bincode::Error),

    #[error("Consumer: {0}")]
    ConsumerError(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Tag an object as Publishable
#[async_trait]
pub trait BrokerPublish {
    fn exchange_name(&self) -> &'static str;
}

/// Plug listeners to the broker.
#[async_trait]
pub trait BrokerListener: Send + Sync {
    /// Bind the queue & struct to this exchange name
    fn exchange_name(&self) -> &'static str;

    /// How to process the Messages queue
    ///  - X: by spawning a task for each of them, up to some concurrent limit X (use semaphore internally)
    fn max_concurrent_tasks(&self) -> usize {
        1
    }

    /// The method that will be called in the struct impl on every messages received
    /// Err(false): reject.requeue = false
    /// Err(true): reject.requeue = true
    async fn consume(&self, delivery: &Delivery) -> std::result::Result<(), bool>;
}

/// AMQP Client
pub struct Broker {
    conn: Option<Connection>,
    publisher: Publisher,
    consumer: Consumer,
}

impl Broker {
    pub fn new() -> Self {
        Self {
            conn: None,
            publisher: Publisher::new(),
            consumer: Consumer::new(),
        }
    }

    /// Connect `Broker` to the AMQP endpoint, then declare Proxy's queue.
    pub async fn init(&mut self, uri: &str) -> Result<()> {
        let conn = Connection::connect(uri, ConnectionProperties::default().with_tokio()).await?;

        info!("Broker connected.");

        self.conn = Some(conn);

        Ok(())
    }

    /// Setup publisher
    pub async fn setup_publisher(&mut self) -> Result<&Publisher> {
        let channel = self.conn.as_ref().unwrap().create_channel().await?;
        self.publisher.channel = Some(channel);

        Ok(&self.publisher)
    }

    /// Init the consumer then return a mut instance in case we need to make more bindings
    pub async fn setup_consumer(&mut self) -> Result<&mut Consumer> {
        let channel = self.conn.as_ref().unwrap().create_channel().await?;
        self.consumer.channel = Some(channel);

        Ok(&mut self.consumer)
    }

    pub async fn publish<P>(&self, entity: &P, routing_key: &str) -> Result<PublisherConfirm>
    where
        P: BrokerPublish + Serialize,
    {
        self.publisher.publish(entity, routing_key).await
    }

    pub async fn publish_raw(
        &self,
        exchange: &str,
        routing_key: &str,
        msg: &[u8],
    ) -> Result<PublisherConfirm> {
        self.publisher.publish_raw(exchange, routing_key, msg).await
    }
}

pub struct Publisher {
    channel: Option<Channel>,
}

impl Publisher {
    pub fn new() -> Self {
        Self { channel: None }
    }

    pub fn channel(&self) -> &Channel {
        self.channel.as_ref().expect("Publisher's channel is None")
    }

    /// Push item into amqp
    pub async fn publish<P>(&self, entity: &P, routing_key: &str) -> Result<PublisherConfirm>
    where
        P: BrokerPublish + Serialize,
    {
        let serialized = bincode::serialize(entity)?;

        // start prometheus duration timer
        let histogram_timer = STAT_PUBLISHER_DURATION.with_label_values(&[entity.exchange_name(), routing_key]).start_timer();

        let res = self
            .channel()
            .basic_publish(
                entity.exchange_name(),
                routing_key,
                BasicPublishOptions::default(),
                serialized.as_slice(),
                BasicProperties::default(),
            )
            .await;

        // finish and compute the duration to prometheus
        histogram_timer.observe_duration();

        res.map_err(|e| Error::Amqp(e))
    }

    /// Push without serializing
    pub async fn publish_raw(
        &self,
        exchange: &str,
        routing_key: &str,
        msg: &[u8],
    ) -> Result<PublisherConfirm> {
        // start prometheus duration timer
        let histogram_timer = STAT_PUBLISHER_DURATION.with_label_values(&[exchange, routing_key]).start_timer();

        let res = self
            .channel()
            .basic_publish(
                exchange,
                routing_key,
                BasicPublishOptions::default(),
                msg,
                BasicProperties::default(),
            )
            .await;

        // finish and compute the duration to prometheus
        histogram_timer.observe_duration();

        // let res = res.await?;
        res.map_err(|e| Error::Amqp(e))
    }
}

impl Clone for Publisher {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
        }
    }
}

pub struct Listener {
    inner: Arc<dyn BrokerListener>,  // Replace Box with Arc, because a Box can not be cloned.
    semaphore: Arc<Semaphore>,
}

impl Clone for Listener {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            semaphore: self.semaphore.clone(),
        }
    }
}

impl Listener {
    pub fn new(listener: Arc<dyn BrokerListener>) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(listener.max_concurrent_tasks())),
            inner: listener,
        }
    }

    fn listener(&self) -> &Arc<dyn BrokerListener> {
        &self.inner
    }

    fn max_concurrent_tasks(&self) -> usize {
        self.inner.max_concurrent_tasks()
    }
}

pub struct Consumer {
    channel: Option<Channel>,
    consumer: Option<lapin::Consumer>,
    listeners: Option<Vec<Listener>>,
}

impl Consumer {
    pub fn new() -> Self {
        Self {
            channel: None,
            consumer: None,
            listeners: Some(vec![]),
        }
    }

    pub fn channel(&self) -> &Channel {
        self.channel.as_ref().expect("Consumer's channel is None")
    }

    pub fn set_consumer(&mut self, consumer: lapin::Consumer) {
        self.consumer = Some(consumer);
    }

    /// Add and store listeners
    /// When a listener is added, it will bind the queue to the specified exchange name.
    pub fn add_listener(&mut self, listener: Arc<dyn BrokerListener>) {
        self.listeners.as_mut().expect("No listeners found").push(Listener::new(listener));
    }

    /// Will spawn the Consumer automatically
    pub fn spawn(&mut self) -> JoinHandle<Result<()>> {
        let consumer = self
            .consumer
            .as_ref()
            .expect("A consumer hasn't been set.")
            .clone();
        let listeners = self.listeners.take().expect("No listeners found");

        let handle = task::spawn(Consumer::consume(consumer, listeners));

        info!("Consumer has been launched in background.");

        handle
    }

    /// In order to spawn it manually.
    pub fn get_consumer(&mut self) -> (lapin::Consumer, Vec<Listener>) {
        let consumer = self
            .consumer
            .as_ref()
            .expect("A consumer hasn't been set.")
            .clone();
        let listeners = self.listeners.take().expect("No listeners found");

        (consumer, listeners)
    }

    /// Consume messages by finding the appropriated listener.
    pub async fn consume(
        mut consumer: lapin::Consumer,
        listeners: Vec<Listener>,
    ) -> Result<()> {
        debug!("Broker consuming...");
        while let Some(message) = consumer.next().await {
            match message {
                Ok(delivery) => {
                    // info!("received message: {:?}", delivery);
                    let listener = listeners
                        .iter()
                        .find(|listener| listener.listener().exchange_name() == delivery.exchange.as_str());

                    if let Some(listener) = listener {
                        // Listener found, try to consume the delivery
                        let listener = listener.clone();
                        let permits_available = listener.semaphore.available_permits() as i64; // i64 for prometheus
                        debug!("waiting for a permit ({}/{} available)", permits_available, permits_max = listener.max_concurrent_tasks());
                        STAT_CONCURRENT_TASK
                            .with_label_values(&[delivery.exchange.as_str(), "max"])
                            .set(listener.max_concurrent_tasks() as i64);

                        let permit = listener.semaphore.clone();
                        let permit = permit.acquire_owned().await?;
                        debug!("Got a permit, we can start to check");

                        STAT_CONCURRENT_TASK
                            .with_label_values(&[delivery.exchange.as_str(), "permits_used"])
                            .inc();

                        // consume the delivery asynchronously
                        task::spawn(consume_async(delivery, listener, permit));
                    } else {
                        // No listener found for that exchange
                        if let Err(err) = delivery.nack(BasicNackOptions::default())
                            .await
                        {
                            panic!("Can't find any registered listeners for `{}` exchange: {:?} + Failed to send nack: {}", &delivery.exchange, &delivery, err);
                        } else {
                            panic!(
                                "Can't find any registered listeners for `{}` exchange: {:?}",
                                &delivery.exchange, &delivery
                            );
                        }
                    }
                }
                Err(err) => {
                    error!(%err, "Error when receiving a delivery");
                    Err(err)? // force the binary to shutdown on any AMQP error received
                }
            }
        }
        Ok(())
    }
}

impl Clone for Consumer {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            consumer: self.consumer.clone(),
            listeners: self.listeners.clone(),
        }
    }
}

// async fn consume_async<L: BrokerListener + ?Sized>(
//     delivery: Delivery,
//     listener: Arc<L>,
//     channel: Channel,
// ) {
/// Consume the delivery async
async fn consume_async(
    delivery: Delivery,
    listener: Listener,
    permit: OwnedSemaphorePermit,
) {
    // start prometheus duration timer
    let histogram_timer = STAT_CONSUMER_DURATION.with_label_values(&[listener.inner.exchange_name()]).start_timer();

    // launch the consumer
    let res = listener.listener().consume(&delivery).await;
    drop(permit); // release the permit immediately

    STAT_CONCURRENT_TASK
        .with_label_values(&[delivery.exchange.as_str(), "permits_used"])
        .dec();

    // finish and compute the duration to prometheus
    histogram_timer.observe_duration();

    if let Err(requeue) = res {
        let mut options = BasicRejectOptions::default();
        options.requeue = requeue;

        if let Err(err_reject) = delivery.reject(options).await {
            error!(requeue, %err_reject, "Broker failed to send REJECT");
        } else {
            let exchange_name = listener.inner.exchange_name();
            let routing_key = delivery.routing_key;
            let redelivered = delivery.redelivered;

            warn!(requeue, %exchange_name, %routing_key, %redelivered, "Error during consumption of a delivery, `REJECT` sent");
        }
    } else {
        // Consumption went fine, we send ACK
        if let Err(err) = delivery.ack( BasicAckOptions::default()).await {
            error!(
                %err, "Delivery consumed, but failed to send ACK back to the broker",
            );
        }
    }
}
