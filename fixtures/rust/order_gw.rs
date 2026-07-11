//! Fixture: order-gateway — publishes order-events (rdkafka), consumes fills.

use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};

const TOPIC_ORDER_EVENTS: &str = "order-events";

pub async fn publish_order(producer: &FutureProducer, payload: &[u8]) {
    let record = FutureRecord::to(TOPIC_ORDER_EVENTS)
        .payload(payload)
        .key("order");
    let _ = producer
        .send(record, std::time::Duration::from_secs(0))
        .await;
}

pub fn start_fill_consumer(consumer: &StreamConsumer) {
    consumer.subscribe(&["fills"]).expect("subscribe failed");
}
