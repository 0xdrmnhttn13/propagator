// Fixture: clearing-bridge — consumes risk-alerts (cppkafka), publishes settlements.
#include <string>

static const std::string kTopicRiskAlerts = "risk-alerts";

void run_consumer(cppkafka::Consumer& consumer) {
    consumer.subscribe({kTopicRiskAlerts, "trade-confirms"});
}

void publish_settlement(cppkafka::Producer& producer, const std::string& payload) {
    producer.produce("settlements", payload);
}
