package risk

import (
	"context"
	"fmt"

	"database/sql"

	"github.com/twmb/franz-go/pkg/kgo"
)

const TopicOrderEvents = "order-events"
const TopicRiskAlerts = "risk-alerts"
const USP_NEW_ORDER_V16 = "USP_NEW_ORDER_V16"

type Engine struct {
	cl *kgo.Client
	db *sql.DB
}

func (e *Engine) Run(ctx context.Context, region string) {
	// consume orders (const indirection + direct literal)
	e.cl.AddConsumeTopics(TopicOrderEvents, "position-updates")

	// dynamic topic — must land in unresolved_topics, not the graph
	dyn := fmt.Sprintf("orders-%s", region)
	e.cl.AddConsumeTopics(dyn)

	// on each order, invoke the stored proc (bare const via PrepareContext)
	stmt, _ := e.db.PrepareContext(ctx, USP_NEW_ORDER_V16)
	_ = stmt
	// legacy quoted-literal invoke
	e.db.Exec("SPI_CHECKBUYLIMIT")
}

func (e *Engine) Publish(ctx context.Context) {
	r := &kgo.Record{Topic: TopicRiskAlerts, Value: nil}
	e.cl.Produce(ctx, r, nil)
}
