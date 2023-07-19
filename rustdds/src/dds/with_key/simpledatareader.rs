use std::{
  cmp::max,
  collections::BTreeMap,
  io,
  pin::Pin,
  sync::{Arc, Mutex, MutexGuard},
  task::{Context, Poll, Waker},
};

use futures::stream::{FusedStream, Stream};
use serde::de::DeserializeSeed;
use mio_extras::channel as mio_channel;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use mio_06::{self, Evented};
use mio_08;

use crate::{
  dds::{
    adapters::with_key::*,
    ddsdata::*,
    key::*,
    pubsub::Subscriber,
    qos::*,
    result::*,
    statusevents::*,
    topic::{Topic, TopicDescription},
    with_key::datasample::{DeserializedCacheChange, Sample},
  },
  discovery::discovery::DiscoveryCommand,
  log_and_err_internal, log_and_err_precondition_not_met,
  mio_source::PollEventSource,
  structure::{
    cache_change::CacheChange,
    dds_cache::TopicCache,
    entity::RTPSEntity,
    guid::{EntityId, GUID},
    sequence_number::SequenceNumber,
    time::Timestamp,
  },
  CDRDeserializerAdapter, RepresentationIdentifier,
};

#[derive(Clone, Debug)]
pub(crate) enum ReaderCommand {
  #[allow(dead_code)] // TODO: Implement this (resetting) feature
  ResetRequestedDeadlineStatus,
}

// This is helper struct.
// All mutable state needed for reading should go here.
pub(crate) struct ReadState<K: Key> {
  latest_instant: Timestamp, /* This is used as a read pointer from dds_cache for BEST_EFFORT
                              * reading */
  last_read_sn: BTreeMap<GUID, SequenceNumber>, // collection of read pointers for RELIABLE reading
  /// hash_to_key_map is used for decoding received key hashes back to original
  /// key values. This is needed when we receive a dispose message via hash
  /// only.
  hash_to_key_map: BTreeMap<KeyHash, K>, // TODO: garbage collect this somehow
}

impl<K: Key> ReadState<K> {
  fn new() -> Self {
    ReadState {
      latest_instant: Timestamp::ZERO,
      last_read_sn: BTreeMap::new(),
      hash_to_key_map: BTreeMap::<KeyHash, K>::new(),
    }
  }

  // This is a helper function so that borrow checker understands
  // that we are splitting one mutable borrow into two _disjoint_ mutable
  // borrows.
  fn get_sn_map_and_hash_map(
    &mut self,
  ) -> (
    &mut BTreeMap<GUID, SequenceNumber>,
    &mut BTreeMap<KeyHash, K>,
  ) {
    let ReadState {
      last_read_sn,
      hash_to_key_map,
      ..
    } = self;
    (last_read_sn, hash_to_key_map)
  }
}

/// SimpleDataReaders can only do "take" semantics and does not have
/// any deduplication or other DataSampleCache functionality.
pub struct SimpleDataReader<K>
where
  K: Key,
{
  #[allow(dead_code)] // TODO: This is currently unused, because we do not implement
  // any subscriber-wide QoS policies, such as ordered or coherent access.
  // Remove this attribute when/if such things are implemented.
  my_subscriber: Subscriber,

  my_topic: Topic,
  qos_policy: QosPolicies,
  my_guid: GUID,
  pub(crate) notification_receiver: mio_channel::Receiver<()>,

  // SimpleDataReader stores a pointer to a mutex on the topic cache
  topic_cache: Arc<Mutex<TopicCache>>,

  read_state: Mutex<ReadState<K>>,

  discovery_command: mio_channel::SyncSender<DiscoveryCommand>,
  status_receiver: StatusReceiver<DataReaderStatus>,

  #[allow(dead_code)] // TODO: This is currently unused, because we do not implement
  // resetting deadline missed status. Remove attribute when it is supported.
  reader_command: mio_channel::SyncSender<ReaderCommand>,
  data_reader_waker: Arc<Mutex<Option<Waker>>>,

  event_source: PollEventSource,
}

impl<K> Drop for SimpleDataReader<K>
where
  K: Key,
{
  fn drop(&mut self) {
    // Tell dp_event_loop
    self.my_subscriber.remove_reader(self.my_guid);

    // Tell discovery
    match self
      .discovery_command
      .send(DiscoveryCommand::RemoveLocalReader { guid: self.my_guid })
    {
      Ok(_) => {}
      Err(mio_channel::SendError::Disconnected(_)) => {
        debug!("Failed to send DiscoveryCommand::RemoveLocalReader . Maybe shutting down?");
      }
      Err(e) => error!(
        "Failed to send DiscoveryCommand::RemoveLocalReader. {:?}",
        e
      ),
    }
  }
}

impl<K> SimpleDataReader<K>
where
  K: Key,
{
  #[allow(clippy::too_many_arguments)]
  pub(crate) fn new(
    subscriber: Subscriber,
    my_id: EntityId,
    topic: Topic,
    qos_policy: QosPolicies,
    // Each notification sent to this channel must be try_recv'd
    notification_receiver: mio_channel::Receiver<()>,
    topic_cache: Arc<Mutex<TopicCache>>,
    discovery_command: mio_channel::SyncSender<DiscoveryCommand>,
    status_channel_rec: StatusChannelReceiver<DataReaderStatus>,
    reader_command: mio_channel::SyncSender<ReaderCommand>,
    data_reader_waker: Arc<Mutex<Option<Waker>>>,
    event_source: PollEventSource,
  ) -> Result<Self> {
    let dp = match subscriber.participant() {
      Some(dp) => dp,
      None => {
        return log_and_err_precondition_not_met!(
          "Cannot create new DataReader, DomainParticipant doesn't exist."
        )
      }
    };

    let my_guid = GUID::new_with_prefix_and_id(dp.guid_prefix(), my_id);

    // Verify that the topic cache corresponds to the topic of the Reader
    let topic_cache_name = topic_cache.lock().unwrap().topic_name();
    if topic.name() != topic_cache_name {
      return log_and_err_internal!(
        "Topic name = {} and topic cache name = {} not equal when creating a SimpleDataReader",
        topic.name(),
        topic_cache_name
      );
    }

    Ok(Self {
      my_subscriber: subscriber,
      qos_policy,
      my_guid,
      notification_receiver,
      topic_cache,
      read_state: Mutex::new(ReadState::new()),
      my_topic: topic,
      discovery_command,
      status_receiver: StatusReceiver::new(status_channel_rec),
      reader_command,
      data_reader_waker,
      event_source,
    })
  }
  pub fn set_waker(&self, w: Option<Waker>) {
    *self.data_reader_waker.lock().unwrap() = w;
  }

  pub(crate) fn drain_read_notifications(&self) {
    while self.notification_receiver.try_recv().is_ok() {}
    self.event_source.drain();
  }

  fn try_take_undecoded<'a>(
    is_reliable: bool,
    topic_cache: &'a TopicCache,
    latest_instant: Timestamp,
    last_read_sn: &'a BTreeMap<GUID, SequenceNumber>,
  ) -> Box<dyn Iterator<Item = (Timestamp, &'a CacheChange)> + 'a> {
    if is_reliable {
      topic_cache.get_changes_in_range_reliable(last_read_sn)
    } else {
      topic_cache.get_changes_in_range_best_effort(latest_instant, Timestamp::now())
    }
  }

  fn update_hash_to_key_map<D>(
    hash_to_key_map: &mut BTreeMap<KeyHash, K>,
    deserialized: &Sample<D, K>,
  ) where
    D: Keyed<K = K>,
  {
    let instance_key = match deserialized {
      Sample::Value(d) => d.key(),
      Sample::Dispose(k) => k.clone(),
    };
    hash_to_key_map.insert(instance_key.hash_key(), instance_key);
  }

  fn deserialize<DA, D>(
    timestamp: Timestamp,
    cc: &CacheChange,
    hash_to_key_map: &mut BTreeMap<KeyHash, K>,
  ) -> std::result::Result<DeserializedCacheChange<D>, String>
  where
    DA: DeserializerAdapter<D>,
    D: Keyed<K = K>,
  {
    Self::deserialize_inner::<DA, D>(
      cc,
      hash_to_key_map,
      timestamp,
      DA::supported_encodings(),
      DA::from_bytes,
    )
  }

  fn deserialize_seed<DA, D, S>(
    timestamp: Timestamp,
    cc: &CacheChange,
    hash_to_key_map: &mut BTreeMap<KeyHash, K>,
    deserialize: S,
  ) -> std::result::Result<DeserializedCacheChange<D>, String>
  where
    S: for<'de> DeserializeSeed<'de, Value = D>,
    D: Keyed<K = K> + 'static,
    DA: SeedDeserializerAdapter<D>,
  {
    Self::deserialize_inner::<DA, D>(
      cc,
      hash_to_key_map,
      timestamp,
      DA::supported_encodings(),
      |value, encoding| DA::from_bytes(deserialize, value, encoding),
    )
  }

  /// Note: Always remember to call .drain_read_notifications() just before
  /// calling this one. Otherwise, new notifications may not appear.
  pub fn try_take_one<DA, D>(&self) -> Result<Option<DeserializedCacheChange<D>>>
  where
    DA: DeserializerAdapter<D>,
    D: Keyed<K = K>,
  {
    let is_reliable = matches!(
      self.qos_policy.reliability(),
      Some(policy::Reliability::Reliable { .. })
    );

    let topic_cache = self.acquire_the_topic_cache_guard();

    let mut read_state_ref = self.read_state.lock().unwrap();
    let latest_instant = read_state_ref.latest_instant;
    let (last_read_sn, hash_to_key_map) = read_state_ref.get_sn_map_and_hash_map();
    let (timestamp, cc) = match Self::try_take_undecoded(
      is_reliable,
      &topic_cache,
      latest_instant,
      last_read_sn,
    )
    .next()
    {
      None => return Ok(None),
      Some((ts, cc)) => (ts, cc),
    };

    match Self::deserialize::<DA, D>(timestamp, cc, hash_to_key_map) {
      Ok(dcc) => {
        read_state_ref.latest_instant = max(read_state_ref.latest_instant, timestamp);
        read_state_ref
          .last_read_sn
          .insert(dcc.writer_guid, dcc.sequence_number);
        Ok(Some(dcc))
      }
      Err(string) => Error::serialization_error(format!(
        "{} Topic = {}, Type = {:?}",
        string,
        self.my_topic.name(),
        self.my_topic.get_type()
      )),
    }
  }

  /// Note: Always remember to call .drain_read_notifications() just before
  /// calling this one. Otherwise, new notifications may not appear.
  pub fn try_take_one_seed<DA, D, S>(
    &self,
    deserialize: S,
  ) -> Result<Option<DeserializedCacheChange<D>>>
  where
    S: for<'de> DeserializeSeed<'de, Value = D>,
    D: Keyed<K = K> + 'static,
    DA: SeedDeserializerAdapter<D>,
  {
    let is_reliable = matches!(
      self.qos_policy.reliability(),
      Some(policy::Reliability::Reliable { .. })
    );

    let topic_cache = self.acquire_the_topic_cache_guard();

    let mut read_state_ref = self.read_state.lock().unwrap();
    let latest_instant = read_state_ref.latest_instant;
    let (last_read_sn, hash_to_key_map) = read_state_ref.get_sn_map_and_hash_map();
    let (timestamp, cc) = match Self::try_take_undecoded(
      is_reliable,
      &topic_cache,
      latest_instant,
      last_read_sn,
    )
    .next()
    {
      None => return Ok(None),
      Some((ts, cc)) => (ts, cc),
    };

    match Self::deserialize_seed::<DA, D, S>(timestamp, cc, hash_to_key_map, deserialize) {
      Ok(dcc) => {
        read_state_ref.latest_instant = max(read_state_ref.latest_instant, timestamp);
        read_state_ref
          .last_read_sn
          .insert(dcc.writer_guid, dcc.sequence_number);
        Ok(Some(dcc))
      }
      Err(string) => Error::serialization_error(format!(
        "{} Topic = {}, Type = {:?}",
        string,
        self.my_topic.name(),
        self.my_topic.get_type()
      )),
    }
  }

  pub fn qos(&self) -> &QosPolicies {
    &self.qos_policy
  }

  pub fn guid(&self) -> GUID {
    self.my_guid
  }

  pub fn topic(&self) -> &Topic {
    &self.my_topic
  }

  pub fn as_async_stream<DA, D>(&self) -> SimpleDataReaderStream<D, DA>
  where
    DA: DeserializerAdapter<D>,
    D: Keyed<K = K>,
  {
    SimpleDataReaderStream {
      simple_datareader: self,
      phantom: std::marker::PhantomData,
      phantom_d: std::marker::PhantomData,
    }
  }

  pub fn as_simple_data_reader_event_stream(&self) -> SimpleDataReaderEventStream<K> {
    SimpleDataReaderEventStream {
      simple_datareader: self,
    }
  }

  fn acquire_the_topic_cache_guard(&self) -> MutexGuard<TopicCache> {
    self.topic_cache.lock().unwrap_or_else(|e| {
      panic!(
        "The topic cache of topic {} is poisoned. Error: {}",
        &self.my_topic.name(),
        e
      )
    })
  }

  fn deserialize_inner<DA, D>(
    cc: &CacheChange,
    hash_to_key_map: &mut BTreeMap<KeyHash, K>,
    timestamp: Timestamp,
    supported_encodings: &[RepresentationIdentifier],
    deserialize: impl FnOnce(
      &[u8],
      crate::RepresentationIdentifier,
    ) -> std::result::Result<D, crate::serialization::Error>,
  ) -> std::result::Result<DeserializedCacheChange<D>, String>
  where
    D: Keyed<K = K>,
    DA: KeyFromBytes<D>,
  {
    match cc.data_value {
      DDSData::Data {
        ref serialized_payload,
      } => {
        // what is our data serialization format (representation identifier) ?
        if let Some(recognized_rep_id) = supported_encodings
          .iter()
          .find(|r| **r == serialized_payload.representation_identifier)
        {
          match deserialize(&serialized_payload.value, *recognized_rep_id) {
            // Data update, decoded ok
            Ok(payload) => {
              let p = Sample::Value(payload);
              Self::update_hash_to_key_map(hash_to_key_map, &p);
              Ok(DeserializedCacheChange::new(timestamp, cc, p))
            }
            Err(e) => Err(format!("Failed to deserialize sample bytes: {e}, ")),
          }
        } else {
          Err(format!(
            "Unknown representation id {:?}.",
            serialized_payload.representation_identifier
          ))
        }
      }

      DDSData::DisposeByKey {
        key: ref serialized_key,
        ..
      } => {
        match DA::key_from_bytes(
          &serialized_key.value,
          serialized_key.representation_identifier,
        ) {
          Ok(key) => {
            let k = Sample::Dispose(key);
            Self::update_hash_to_key_map(hash_to_key_map, &k);
            Ok(DeserializedCacheChange::new(timestamp, cc, k))
          }
          Err(e) => Err(format!("Failed to deserialize key {}", e)),
        }
      }

      DDSData::DisposeByKeyHash { key_hash, .. } => {
        // The cache should know hash -> key mapping even if the sample
        // has been disposed or .take()n
        if let Some(key) = hash_to_key_map.get(&key_hash) {
          Ok(DeserializedCacheChange::new(
            timestamp,
            cc,
            Sample::Dispose(key.clone()),
          ))
        } else {
          Err(format!(
            "Tried to dispose with unknown key hash: {:x?}",
            key_hash
          ))
        }
      }
    }
    // match
  }
}

// This is  not part of DDS spec. We implement mio Eventd so that the
// application can asynchronously poll DataReader(s).
impl<K> Evented for SimpleDataReader<K>
where
  K: Key,
{
  // We just delegate all the operations to notification_receiver, since it
  // already implements Evented
  fn register(
    &self,
    poll: &mio_06::Poll,
    token: mio_06::Token,
    interest: mio_06::Ready,
    opts: mio_06::PollOpt,
  ) -> io::Result<()> {
    self
      .notification_receiver
      .register(poll, token, interest, opts)
  }

  fn reregister(
    &self,
    poll: &mio_06::Poll,
    token: mio_06::Token,
    interest: mio_06::Ready,
    opts: mio_06::PollOpt,
  ) -> io::Result<()> {
    self
      .notification_receiver
      .reregister(poll, token, interest, opts)
  }

  fn deregister(&self, poll: &mio_06::Poll) -> io::Result<()> {
    self.notification_receiver.deregister(poll)
  }
}

impl<K> mio_08::event::Source for SimpleDataReader<K>
where
  K: Key,
{
  fn register(
    &mut self,
    registry: &mio_08::Registry,
    token: mio_08::Token,
    interests: mio_08::Interest,
  ) -> io::Result<()> {
    self.event_source.register(registry, token, interests)
  }

  fn reregister(
    &mut self,
    registry: &mio_08::Registry,
    token: mio_08::Token,
    interests: mio_08::Interest,
  ) -> io::Result<()> {
    self.event_source.reregister(registry, token, interests)
  }

  fn deregister(&mut self, registry: &mio_08::Registry) -> io::Result<()> {
    self.event_source.deregister(registry)
  }
}

impl<K> StatusEvented<DataReaderStatus> for SimpleDataReader<K>
where
  K: Key,
{
  fn as_status_evented(&mut self) -> &dyn Evented {
    self.status_receiver.as_status_evented()
  }

  fn as_status_source(&mut self) -> &mut dyn mio_08::event::Source {
    self.status_receiver.as_status_source()
  }

  fn try_recv_status(&self) -> Option<DataReaderStatus> {
    self.status_receiver.try_recv_status()
  }
}

impl<K> RTPSEntity for SimpleDataReader<K>
where
  K: Key,
{
  fn guid(&self) -> GUID {
    self.my_guid
  }
}

// ----------------------------------------------
// ----------------------------------------------

// Async interface to the SimpleDataReader

pub struct SimpleDataReaderStream<
  'a,
  D: Keyed + 'static,
  DA: DeserializerAdapter<D> + 'static = CDRDeserializerAdapter<D>,
> {
  simple_datareader: &'a SimpleDataReader<D::K>,
  phantom: std::marker::PhantomData<DA>,
  phantom_d: std::marker::PhantomData<D>,
}

// ----------------------------------------------
// ----------------------------------------------

// https://users.rust-lang.org/t/take-in-impl-future-cannot-borrow-data-in-a-dereference-of-pin/52042
impl<'a, D, DA> Unpin for SimpleDataReaderStream<'a, D, DA>
where
  D: Keyed + 'static,
  DA: DeserializerAdapter<D>,
{
}

impl<'a, D, DA> Stream for SimpleDataReaderStream<'a, D, DA>
where
  D: Keyed + 'static,
  DA: DeserializerAdapter<D>,
{
  type Item = Result<DeserializedCacheChange<D>>;

  // The full return type is now
  // Poll<Option<Result<DeserializedCacheChange<D>>>
  // Poll -> Ready or Pending
  // Option -> Some = stream produces a value, None = stream has ended (does not
  // occur) Result -> Ok = No DDS error, Err = DDS processing error
  // (inner Option -> Some = there is new value/key, None = no new data yet)

  fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    debug!("poll_next");
    match self.simple_datareader.try_take_one::<DA, D>() {
      Err(e) =>
      // DDS fails
      {
        Poll::Ready(Some(Err(e)))
      }

      // ok, got something
      Ok(Some(d)) => Poll::Ready(Some(Ok(d))),

      // No new data (yet)
      Ok(None) => {
        // Did not get any data.
        // --> Store waker.
        // 1. synchronously store waker to background thread (must rendezvous)
        // 2. try take_bare again, in case something arrived just now
        // 3. if nothing still, return pending.
        self.simple_datareader.set_waker(Some(cx.waker().clone()));
        match self.simple_datareader.try_take_one::<DA, D>() {
          Err(e) => Poll::Ready(Some(Err(e))),
          Ok(Some(d)) => Poll::Ready(Some(Ok(d))),
          Ok(None) => Poll::Pending,
        }
      }
    } // match
  } // fn
} // impl

impl<'a, D, DA> FusedStream for SimpleDataReaderStream<'a, D, DA>
where
  D: Keyed + 'static,
  DA: DeserializerAdapter<D>,
{
  fn is_terminated(&self) -> bool {
    false // Never terminate. This means it is always valid to call poll_next().
  }
}

// ----------------------------------------------
// ----------------------------------------------

pub struct SimpleDataReaderEventStream<'a, K>
where
  K: Key,
{
  simple_datareader: &'a SimpleDataReader<K>,
}

impl<'a, K> Stream for SimpleDataReaderEventStream<'a, K>
where
  K: Key,
{
  type Item = std::result::Result<DataReaderStatus, std::sync::mpsc::RecvError>;

  fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    Pin::new(&mut self.simple_datareader.status_receiver.as_async_stream()).poll_next(cx)
  } // fn
} // impl
