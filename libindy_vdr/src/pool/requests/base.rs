use std::pin::Pin;

use futures::channel::mpsc::UnboundedReceiver;
use futures::stream::{FusedStream, Stream};
use futures::task::{Context, Poll};

use pin_utils::unsafe_pinned;

use crate::common::error::prelude::*;
use crate::config::PoolConfig;

use super::networker::{NetworkerEvent, RefNetworker};
use super::types::NodeKeys;
use super::{RequestEvent, RequestExtEvent, RequestState, RequestTiming, TimingResult};

new_handle_type!(RequestHandle, RQ_COUNTER);

#[must_use = "requests do nothing unless polled"]
pub trait PoolRequest: std::fmt::Debug + Stream<Item = RequestEvent> + FusedStream + Unpin {
    fn clean_timeout(&self, node_alias: String) -> LedgerResult<()>;
    fn extend_timeout(&self, node_alias: String, timeout: i64) -> LedgerResult<()>;
    fn get_timing(&self) -> Option<TimingResult>;
    fn is_active(&self) -> bool;
    fn node_count(&self) -> usize;
    fn node_keys(&self) -> NodeKeys;
    fn node_order(&self) -> Vec<String>;
    fn pool_config(&self) -> PoolConfig;
    fn send_to_all(&mut self, timeout: i64) -> LedgerResult<()>;
    fn send_to_any(&mut self, count: usize, timeout: i64) -> LedgerResult<Vec<String>>;
    fn send_to(&mut self, node_aliases: Vec<String>, timeout: i64) -> LedgerResult<Vec<String>>;
}

pub struct PoolRequestImpl<T: RefNetworker> {
    handle: RequestHandle,
    events: Option<UnboundedReceiver<RequestExtEvent>>,
    pool_config: PoolConfig,
    networker: T,
    node_keys: NodeKeys,
    node_order: Vec<String>,
    send_count: usize,
    state: RequestState,
    timing: RequestTiming,
}

impl<T: RefNetworker> PoolRequestImpl<T> {
    unsafe_pinned!(events: Option<UnboundedReceiver<RequestExtEvent>>);

    pub fn new(
        handle: RequestHandle,
        events: UnboundedReceiver<RequestExtEvent>,
        pool_config: PoolConfig,
        networker: T,
        node_keys: NodeKeys,
        node_order: Vec<String>,
    ) -> Self {
        Self {
            handle,
            events: Some(events),
            pool_config,
            networker,
            node_keys,
            node_order,
            send_count: 0,
            state: RequestState::NotStarted,
            timing: RequestTiming::new(),
        }
    }

    fn trigger(&self, event: NetworkerEvent) -> LedgerResult<()> {
        self.networker.as_ref().send(event)
    }
}

impl<T: RefNetworker> Unpin for PoolRequestImpl<T> {}

impl<T: RefNetworker> PoolRequest for PoolRequestImpl<T> {
    fn clean_timeout(&self, node_alias: String) -> LedgerResult<()> {
        self.trigger(NetworkerEvent::CleanTimeout(self.handle, node_alias))
    }

    fn extend_timeout(&self, node_alias: String, timeout: i64) -> LedgerResult<()> {
        self.trigger(NetworkerEvent::ExtendTimeout(
            self.handle,
            node_alias,
            timeout,
        ))
    }

    fn get_timing(&self) -> Option<TimingResult> {
        self.timing.get_result()
    }

    fn is_active(&self) -> bool {
        self.state == RequestState::Active
    }

    fn node_order(&self) -> Vec<String> {
        self.node_order.clone()
    }

    fn node_count(&self) -> usize {
        self.node_order.len()
    }

    fn node_keys(&self) -> NodeKeys {
        // FIXME - remove nodes that aren't present in node_aliases?
        self.node_keys.clone()
    }

    fn pool_config(&self) -> PoolConfig {
        self.pool_config
    }

    fn send_to_all(&mut self, timeout: i64) -> LedgerResult<()> {
        let aliases = self.node_order();
        let count = aliases.len();
        self.trigger(NetworkerEvent::Dispatch(self.handle, aliases, timeout))?;
        self.send_count += count;
        Ok(())
    }

    fn send_to_any(&mut self, count: usize, timeout: i64) -> LedgerResult<Vec<String>> {
        let aliases = self.node_order();
        let max = std::cmp::min(self.send_count + count, aliases.len());
        let min = std::cmp::min(self.send_count, max);
        trace!("send to any {} {} {:?}", min, max, aliases);
        let nodes = (min..max)
            .map(|idx| aliases[idx].clone())
            .collect::<Vec<String>>();
        if nodes.len() > 0 {
            self.trigger(NetworkerEvent::Dispatch(
                self.handle,
                nodes.clone(),
                timeout,
            ))?;
            self.send_count += nodes.len();
        }
        Ok(nodes)
    }

    fn send_to(&mut self, node_aliases: Vec<String>, timeout: i64) -> LedgerResult<Vec<String>> {
        let aliases = self
            .node_order
            .iter()
            .filter(|n| node_aliases.contains(n))
            .cloned()
            .collect::<Vec<String>>();
        if aliases.len() > 0 {
            self.trigger(NetworkerEvent::Dispatch(
                self.handle,
                aliases.clone(),
                timeout,
            ))?;
            self.send_count += aliases.len();
        }
        Ok(aliases)
    }
}

impl<T: RefNetworker> std::fmt::Debug for PoolRequestImpl<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PoolRequest({}, state={})", *self.handle, self.state)
    }
}

impl<T: RefNetworker> Drop for PoolRequestImpl<T> {
    fn drop(&mut self) {
        trace!("Finish dropped request: {}", self.handle);
        self.trigger(NetworkerEvent::FinishRequest(self.handle))
            .unwrap_or(()) // don't mind if the receiver disconnected
    }
}

impl<T: RefNetworker> Stream for PoolRequestImpl<T> {
    type Item = RequestEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            trace!("poll_next");
            match self.state {
                RequestState::NotStarted => {
                    if let Some(events) = self.as_mut().events().as_pin_mut() {
                        match events.poll_next(cx) {
                            Poll::Ready(val) => {
                                if let Some(RequestExtEvent::Init()) = val {
                                    trace!("Request active {}", self.handle);
                                    self.state = RequestState::Active
                                } else {
                                    trace!("Request aborted {}", self.handle);
                                    // events.close(); ?
                                    self.as_mut().events().set(None);
                                    self.state = RequestState::Terminated
                                }
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    } else {
                        self.state = RequestState::Terminated
                    }
                }
                RequestState::Active => {
                    if let Some(events) = self.as_mut().events().as_pin_mut() {
                        match events.poll_next(cx) {
                            Poll::Ready(val) => match val {
                                Some(RequestExtEvent::Sent(alias, when)) => {
                                    trace!("{} was sent to {}", self.handle, alias);
                                    self.timing.sent(&alias, when)
                                }
                                Some(RequestExtEvent::Received(alias, message, meta, when)) => {
                                    trace!("{} response from {}", self.handle, alias);
                                    self.timing.received(&alias, when);
                                    return Poll::Ready(Some(RequestEvent::Received(
                                        alias, message, meta,
                                    )));
                                }
                                Some(RequestExtEvent::Timeout(alias)) => {
                                    trace!("{} timed out {}", self.handle, alias);
                                    return Poll::Ready(Some(RequestEvent::Timeout(alias)));
                                }
                                _ => {
                                    trace!("{} terminated", self.handle);
                                    // events.close(); ?
                                    self.as_mut().events().set(None);
                                    self.state = RequestState::Terminated
                                }
                            },
                            Poll::Pending => return Poll::Pending,
                        }
                    } else {
                        self.state = RequestState::Terminated
                    }
                }
                RequestState::Terminated => return Poll::Ready(None),
            }
        }
    }
}

impl<T: RefNetworker> FusedStream for PoolRequestImpl<T> {
    fn is_terminated(&self) -> bool {
        self.state == RequestState::Terminated
    }
}