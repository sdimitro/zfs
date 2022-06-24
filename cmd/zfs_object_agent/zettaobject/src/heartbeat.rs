use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Result;
use lazy_static::lazy_static;
use log::debug;
use log::error;
use log::info;
use log::trace;
use log::warn;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::watch;
use tokio::sync::watch::Receiver;
use util::maybe_die_with;
use util::serde::from_json_slice;
use util::tunable;
use uuid::Uuid;

use crate::access_stats::ObjectAccessOpType;
use crate::object_access::OAError;
use crate::object_access::ObjectAccess;
use crate::object_access::PutError;
use crate::pool::CLAIM_DURATION;

tunable! {
    pub static ref LEASE_DURATION: Duration = Duration::from_secs(50);
    pub static ref HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
    pub static ref WRITE_TIMEOUT: Duration = Duration::from_secs(2);
    pub static ref HEARTBEAT_PANIC: bool = true;
    pub static ref INTERVAL_PANIC: bool = false;
    pub static ref HEARTBEAT_TIMEOUT: Duration = *LEASE_DURATION / 5;
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct HeartbeatPhys {
    pub timestamp: SystemTime,
    pub hostname: String,
    pub lease_duration: Duration,
    pub id: Uuid,
}

impl HeartbeatPhys {
    fn key(id: Uuid) -> String {
        format!("zfs/agents/{}", id)
    }

    pub async fn get(object_access: &ObjectAccess, id: Uuid) -> Result<Self> {
        let buf = object_access
            .get_object(Self::key(id), ObjectAccessOpType::MetadataGet)
            .await?;
        let this: Self = from_json_slice(&buf)
            .with_context(|| format!("Failed to decode contents of {}", Self::key(id)))?;
        debug!("got {:#?}", this);
        assert_eq!(this.id, id);
        Ok(this)
    }

    pub async fn put_timeout(
        &self,
        object_access: &ObjectAccess,
        timeout: Option<Duration>,
    ) -> Result<(), OAError<PutError>> {
        maybe_die_with(|| format!("before putting {:#?}", self));
        trace!("putting {:#?}", self);
        let buf = serde_json::to_vec(&self).unwrap();
        object_access
            .put_object_timed(
                Self::key(self.id),
                buf.into(),
                // XXX should this be its own stat type so that it has its own queue in the
                // ObjectAccess layer?
                ObjectAccessOpType::MetadataPut,
                timeout,
            )
            .await
    }

    pub async fn delete(object_access: &ObjectAccess, id: Uuid) {
        object_access.delete_object(Self::key(id)).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HeartbeatImpl {
    endpoint: Option<String>,
    region: Option<String>,
    bucket: String,
}
pub struct HeartbeatGuard {
    _key: Arc<()>,
    /*
     * When we're resuming the agent after a crash, this field will be set if we go more than
     * LEASE_TIMEOUT without sending a heartbeat. When the agent's heartbeat stops for more than
     * that time, other systems may start the claim process on pools owned by this agent. When
     * the heartbeat restarts after the agent starts again, new claim attempts that come in
     * will fail, but in progress ones may succeed. We don't consider the news about the
     * agent's revival fully propogated until the valid_time.
     */
    pub valid_after: Option<Instant>,
}
type ConcurrentMap<T, S> = Arc<std::sync::Mutex<HashMap<T, S>>>;
lazy_static! {
    static ref HEARTBEATS: ConcurrentMap<HeartbeatImpl, (Weak<()>, Option<Instant>)> =
        Default::default();
    static ref HEARTBEAT_INIT: ConcurrentMap<HeartbeatImpl, Receiver<Option<Instant>>> =
        Default::default();
}

pub async fn start_heartbeat(object_access: Arc<ObjectAccess>, id: Uuid) -> HeartbeatGuard {
    let key = HeartbeatImpl {
        endpoint: object_access.endpoint(),
        region: object_access.region(),
        bucket: object_access.bucket(),
    };
    let hiccup = HeartbeatPhys::get(&object_access, id)
        .await
        .map_or(false, |heartbeat_phys| {
            SystemTime::now()
                .duration_since(heartbeat_phys.timestamp)
                .map_or(true, |d| d > *LEASE_DURATION)
        });
    let (mut guard, tx_opt, rx_opt, found) = {
        let mut heartbeats = HEARTBEATS.lock().unwrap();
        match heartbeats.get(&key) {
            None => {
                let value = Arc::new(());
                // We will update this hiccup time once we've managed to write our first heartbeat.
                heartbeats.insert(key.clone(), (Arc::downgrade(&value), None));
                let (tx, rx) = watch::channel(None);
                HEARTBEAT_INIT
                    .lock()
                    .unwrap()
                    .insert(key.clone(), rx.clone());
                (
                    // We will update this hiccup time once we've managed to write our first
                    // heartbeat.
                    HeartbeatGuard {
                        _key: value,
                        valid_after: None,
                    },
                    Some(tx),
                    Some(rx),
                    false,
                )
            }
            Some((val_ref, hiccup_time)) => {
                debug!("existing entry found");
                match val_ref.upgrade() {
                    None => {
                        /*
                         * In this case, there is already a heartbeat thread that would terminate
                         * on its next iteration. Replace the existing weak ref with a new one,
                         * and let it keep running.
                         */
                        let value = Arc::new(());
                        let time = *hiccup_time;
                        heartbeats.insert(key.clone(), (Arc::downgrade(&value), time));
                        return HeartbeatGuard {
                            _key: value,
                            valid_after: time,
                        };
                    }
                    /*
                     * We have to process this outside of the block so that the compiler
                     * realizes the mutex is dropped across the await.
                     * We also will reset the hiccup time if there's an rx in HEARTBEAT_INIT,
                     * because the value in the HEARTBEATS table isn't accurate until the first
                     * heartbeat has been synced out.
                     */
                    Some(val) => (
                        HeartbeatGuard {
                            _key: val,
                            valid_after: *hiccup_time,
                        },
                        None,
                        HEARTBEAT_INIT
                            .lock()
                            .unwrap()
                            .get(&key)
                            .map(Receiver::clone),
                        true,
                    ),
                }
            }
        }
    };
    if found {
        /*
         * There is an existing heartbeat with references. If there is an init in
         * progress, we wait for the init to finish before returning.
         */
        debug!("upgrade succeeded, using existing heartbeat");
        if let Some(mut rx) = rx_opt {
            rx.changed().await.unwrap();
            guard.valid_after = *rx.borrow();
        }
        return guard;
    }
    let mut rx = rx_opt.unwrap();
    let tx = tx_opt.unwrap();
    tokio::spawn(async move {
        let mut last_heartbeat: Option<Instant> = None;
        info!("Starting heartbeat with id {}", id);
        let mut interval = tokio::time::interval(*HEARTBEAT_INTERVAL);
        loop {
            let interval_start = Instant::now();
            interval.tick().await;
            let interval_end = Instant::now();
            trace!(
                "Interval tick started at {:?}, ended at {:?}, duration {:?}",
                interval_start,
                interval_end,
                interval_end.duration_since(interval_start)
            );
            if interval_end.duration_since(interval_start) > *HEARTBEAT_INTERVAL * 10 {
                if *INTERVAL_PANIC {
                    panic!("Long interval detected at {:?}", SystemTime::now());
                } else {
                    error!(
                        "Long interval ({:?}) detected at {:?}",
                        interval_end.duration_since(interval_start),
                        SystemTime::now()
                    );
                }
            }
            if let Some(time) = last_heartbeat {
                let since = Instant::now().duration_since(time);
                if since > *LEASE_DURATION {
                    warn!("Heartbeat interval significantly over: {:?}", since);
                } else if since > *HEARTBEAT_INTERVAL {
                    trace!("Heartbeat interval slightly over: {:?}", since);
                }
            }
            {
                let fut_opt = {
                    let mut heartbeats = HEARTBEATS.lock().unwrap();
                    // We can almost use or_else here, but that doesn't let us change types.
                    match heartbeats.get(&key).unwrap().0.upgrade() {
                        None => {
                            heartbeats.remove(&key);
                            info!("Stopping heartbeat with id {}", id);
                            Some(HeartbeatPhys::delete(&object_access, id))
                        }
                        Some(_) => None,
                    }
                };
                if let Some(fut) = fut_opt {
                    fut.await;
                    return;
                }
            }
            if let Some(time) = last_heartbeat {
                let since = Instant::now().duration_since(time);
                if since > *LEASE_DURATION {
                    warn!("Heartbeat locking significantly over: {:?}", since);
                } else if since > *HEARTBEAT_INTERVAL {
                    trace!("Heartbeat locking slightly over: {:?}", since);
                }
            }
            let heartbeat = HeartbeatPhys {
                timestamp: SystemTime::now(),
                hostname: hostname::get().unwrap().into_string().unwrap(),
                lease_duration: *LEASE_DURATION,
                id,
            };
            let instant = Instant::now();
            let result = heartbeat
                .put_timeout(&object_access, Some(*HEARTBEAT_TIMEOUT))
                .await;
            if lease_timed_out(last_heartbeat) {
                if *HEARTBEAT_PANIC {
                    panic!("Suspending pools due to lease timeout");
                } else {
                    error!("Lease timed out; ignoring due to tunable");
                }
            }
            match result {
                Ok(_) => {
                    if last_heartbeat.is_none() {
                        if hiccup {
                            let mut heartbeats = HEARTBEATS.lock().unwrap();
                            let time = Instant::now() + (*CLAIM_DURATION * 3);
                            if let Entry::Occupied(mut oe) = heartbeats.entry(key.clone()) {
                                oe.get_mut().1 = Some(time);
                            } else {
                                panic!("key {:?} not in heartbeats", key);
                            }
                            assert!(HEARTBEAT_INIT.lock().unwrap().remove(&key).is_some());
                            tx.send(Some(time)).unwrap();
                        } else {
                            assert!(HEARTBEAT_INIT.lock().unwrap().remove(&key).is_some());
                            tx.send(None).unwrap();
                        }
                    }
                    last_heartbeat = Some(instant);
                }
                Err(e) => info!("Heartbeat put error: {:?}", e),
            }
        }
    });
    rx.changed().await.unwrap();
    guard.valid_after = *rx.borrow();
    guard
}

fn lease_timed_out(last_heartbeat: Option<Instant>) -> bool {
    match last_heartbeat {
        Some(time) => {
            let since = Instant::now().duration_since(time);
            if since > *LEASE_DURATION {
                warn!("Extreme heartbeat delay: {:?}", since);
            } else {
                trace!("Short heartbeat delay: {:?}", since);
            }
            since >= *LEASE_DURATION
        }
        None => false,
    }
}
