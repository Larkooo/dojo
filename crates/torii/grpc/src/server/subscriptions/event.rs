use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use futures_util::StreamExt;
use rand::Rng;
use starknet::core::types::Felt;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::RwLock;
use tokio::time::interval;
use torii_core::error::{Error, ParseError};
use torii_core::simple_broker::SimpleBroker;
use torii_core::sql::FELT_DELIMITER;
use torii_core::types::Event;
use tracing::{error, trace};

use crate::proto;
use crate::proto::world::SubscribeEventsResponse;
use crate::types::{KeysClause, PatternMatching};

pub(crate) const LOG_TARGET: &str = "torii::grpc::server::subscriptions::event";

#[derive(Debug)]
pub struct EventSubscriber {
    /// Event keys that the subscriber is interested in
    keys: KeysClause,
    /// The channel to send the response back to the subscriber.
    sender: Sender<Result<proto::world::SubscribeEventsResponse, tonic::Status>>,
}

#[derive(Debug, Default)]
pub struct EventManager {
    subscribers: RwLock<HashMap<usize, EventSubscriber>>,
}

impl EventManager {
    pub async fn add_subscriber(
        &self,
        keys: KeysClause,
    ) -> Result<Receiver<Result<proto::world::SubscribeEventsResponse, tonic::Status>>, Error> {
        let id = rand::thread_rng().gen::<usize>();
        let (sender, receiver) = channel(1);

        // NOTE: unlock issue with firefox/safari
        // initially send empty stream message to return from
        // initial subscribe call
        let _ = sender.send(Ok(SubscribeEventsResponse { event: None })).await;

        self.subscribers.write().await.insert(id, EventSubscriber { keys, sender });

        Ok(receiver)
    }

    pub(super) async fn remove_subscriber(&self, id: usize) {
        self.subscribers.write().await.remove(&id);
    }
}

#[must_use = "Service does nothing unless polled"]
#[allow(missing_debug_implementations)]
pub struct Service {
    simple_broker: Pin<Box<dyn Stream<Item = Event> + Send>>,
    update_sender: Sender<Event>,
}

impl Service {
    pub fn new(subs_manager: Arc<EventManager>) -> Self {
        let (update_sender, update_receiver) = channel(100);
        let service =
            Self { simple_broker: Box::pin(SimpleBroker::<Event>::subscribe()), update_sender };

        // Spawn a task to process event updates
        tokio::spawn(Self::process_updates(Arc::clone(&subs_manager), update_receiver));

        service
    }

    async fn process_updates(subs: Arc<EventManager>, mut update_receiver: Receiver<Event>) {
        let mut interval = interval(Duration::from_millis(100));
        let mut pending_updates = Vec::new();

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if !pending_updates.is_empty() {
                        for event in pending_updates.drain(..) {
                            if let Err(e) = Self::publish_updates(Arc::clone(&subs), &event).await {
                                error!(target = LOG_TARGET, error = %e, "Publishing events update.");
                            }
                        }
                    }
                }
                Some(event) = update_receiver.recv() => {
                    pending_updates.push(event);
                }
            }
        }
    }

    async fn publish_updates(subs: Arc<EventManager>, event: &Event) -> Result<(), Error> {
        let mut closed_stream = Vec::new();
        let keys = event
            .keys
            .trim_end_matches(FELT_DELIMITER)
            .split(FELT_DELIMITER)
            .map(Felt::from_str)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ParseError::from)?;
        let data = event
            .data
            .trim_end_matches(FELT_DELIMITER)
            .split(FELT_DELIMITER)
            .map(Felt::from_str)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ParseError::from)?;

        for (idx, sub) in subs.subscribers.read().await.iter() {
            // if the key pattern doesnt match our subscribers key pattern, skip
            // ["", "0x0"] would match with keys ["0x...", "0x0", ...]
            if sub.keys.pattern_matching == PatternMatching::FixedLen
                && keys.len() != sub.keys.keys.len()
            {
                continue;
            }

            if !keys.iter().enumerate().all(|(idx, key)| {
                // this is going to be None if our key pattern overflows the subscriber key pattern
                // in this case we might want to list all events with the same
                // key selector so we can match them all
                let sub_key = sub.keys.keys.get(idx);

                // if we have a key in the subscriber, it must match the key in the event
                // unless its empty, which is a wildcard
                match sub_key {
                    // the key in the subscriber must match the key of the entity
                    // athis index
                    Some(Some(sub_key)) => key == sub_key,
                    // otherwise, if we have no key we should automatically match.
                    // or.. we overflowed the subscriber key pattern
                    // but we're in VariableLen pattern matching
                    // so we should match all next keys
                    _ => true,
                }
            }) {
                continue;
            }

            let resp = proto::world::SubscribeEventsResponse {
                event: Some(proto::types::Event {
                    keys: keys.iter().map(|k| k.to_bytes_be().to_vec()).collect(),
                    data: data.iter().map(|d| d.to_bytes_be().to_vec()).collect(),
                    transaction_hash: Felt::from_str(&event.transaction_hash)
                        .map_err(ParseError::from)?
                        .to_bytes_be()
                        .to_vec(),
                }),
            };

            if sub.sender.send(Ok(resp)).await.is_err() {
                closed_stream.push(*idx);
            }
        }

        for id in closed_stream {
            trace!(target = LOG_TARGET, id = %id, "Closing events stream.");
            subs.remove_subscriber(id).await
        }

        Ok(())
    }
}

impl Future for Service {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> std::task::Poll<Self::Output> {
        let pin = self.get_mut();

        while let Poll::Ready(Some(event)) = pin.simple_broker.poll_next_unpin(cx) {
            let sender = pin.update_sender.clone();
            tokio::spawn(async move {
                if let Err(e) = sender.send(event).await {
                    error!(target = LOG_TARGET, error = %e, "Sending event update to channel.");
                }
            });
        }

        Poll::Pending
    }
}
