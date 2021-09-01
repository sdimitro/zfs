use futures::stream;
use futures::{future, StreamExt};
use log::*;
use metered::common::*;
use metered::hdr_histogram::AtomicHdrHistogram;
use metered::metered;
use metered::time_source::StdInstantMicros;
use std::convert::TryInto;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zettaobject::ObjectAccess;

pub async fn write_test(
    object_access: &ObjectAccess,
    objsize: i32,
    qdepth: i32,
    duration: Duration,
) -> Result<(), Box<dyn Error>> {
    let perf = Perf::default();
    let my_perf = perf.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            info!("metrics: {:#?}", my_perf.metrics);
        }
    });

    let mut key_id: i32 = 0;
    let start = Instant::now();
    let data = vec![0; objsize.try_into().unwrap()];
    stream::repeat_with(|| {
        let my_object_access = object_access.clone();
        let my_data = data.clone();
        let my_perf = perf.clone();
        key_id += 1;
        tokio::spawn(async move {
            my_perf
                .put(
                    &my_object_access,
                    &format!("perftest/key{}", key_id),
                    my_data,
                )
                .await;
        })
    })
    .take_while(|_| future::ready(start.elapsed() < duration))
    .buffer_unordered(qdepth.try_into().unwrap())
    .for_each(|_| future::ready(()))
    .await;

    println!("metrics: {:#?}", perf.metrics);
    Ok(())
}

#[derive(Default, Clone)]
struct Perf {
    metrics: Arc<PerfMetrics>,
}

#[metered(registry=PerfMetrics)]
impl Perf {
    #[measure(type = ResponseTime<AtomicHdrHistogram, StdInstantMicros>)]
    #[measure(InFlight)]
    #[measure(Throughput)]
    #[measure(HitCount)]
    async fn put(&self, object_access: &ObjectAccess, key: &str, data: Vec<u8>) {
        object_access.put_object(&key.to_string(), data).await;
    }
}
