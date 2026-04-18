# Notification Connectors

Arca delivers S3 bucket event notifications through a modular connector layer. Each destination in a bucket notification configuration is bound to a **connector type** that dictates how the JSON event is delivered — as an HTTP POST, a Kafka record, an AMQP message, an e-mail, and so on. This guide lists every connector shipped with Arca, the URL form it accepts, and the properties it honors.

All connectors share a common anatomy:

- The **destination URL** goes into the `Topic`, `Queue`, or `CloudFunction` element of the `NotificationConfiguration` XML.
- A `<ConnectorType>` element selects the connector (`webhook`, `kafka`, `grpc`, …).
- Zero or more `<Property><Name>…</Name><Value>…</Value></Property>` entries carry connector-specific options.
- An operational timeout for each connector type is tuned via the `[notifications]` section of the server config (`webhook_timeout_seconds`, `kafka_timeout_seconds`, …).

The Arca Console exposes the same knobs through a per-bucket form. The examples below show the raw XML — any tool that speaks the S3 `PutBucketNotificationConfiguration` API can install them.

## Webhook (HTTP POST)

The original Phase 25 connector. Delivers the S3 event JSON as an `application/json` HTTP POST.

- **URL format**: `http://host[:port]/path` or `https://host[:port]/path`
- **Properties**:
  - `auth_token` — optional Bearer token sent in the `Authorization` header.

```xml
<TopicConfiguration>
  <Id>alerts-webhook</Id>
  <Topic>https://ops.example.com/s3-events</Topic>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>webhook</ConnectorType>
  <Property><Name>auth_token</Name><Value>abc123</Value></Property>
</TopicConfiguration>
```

## Kafka

Produces events to a Kafka topic via `librdkafka`.

- **URL format**: `hostname:9092` (bootstrap server; comma-separated list supported).
- **Properties**:
  - `topic` — Kafka topic name (default `arca-notifications`).
  - `security_protocol` — `PLAINTEXT`, `SSL`, `SASL_PLAINTEXT`, `SASL_SSL`.
  - `sasl_username`, `sasl_password` — SASL/PLAIN credentials.

```xml
<QueueConfiguration>
  <Id>kafka-events</Id>
  <Queue>kafka-broker:9092</Queue>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>kafka</ConnectorType>
  <Property><Name>topic</Name><Value>s3-events</Value></Property>
</QueueConfiguration>
```

## AMQP (RabbitMQ)

Publishes events via `lapin` (pure-Rust AMQP 0-9-1).

- **URL format**: `amqp://user:pass@host:5672` or `amqps://…`
- **Properties**:
  - `exchange` — AMQP exchange (default empty = default exchange).
  - `routing_key` — routing key (default `arca.notifications`).
  - `durable` — `true`/`false` to declare a durable queue.

```xml
<QueueConfiguration>
  <Id>rabbitmq-events</Id>
  <Queue>amqp://guest:guest@rabbitmq:5672/%2f</Queue>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>amqp</ConnectorType>
  <Property><Name>routing_key</Name><Value>arca.object.created</Value></Property>
</QueueConfiguration>
```

## Redis Pub/Sub

Publishes the event JSON to a Redis channel via `redis-rs`.

- **URL format**: `redis://[user:pass@]host:6379[/db]`
- **Properties**:
  - `channel` — channel name (default `arca:notifications`).
  - `password` — Redis AUTH password (overrides the URL).

```xml
<QueueConfiguration>
  <Id>redis-events</Id>
  <Queue>redis://redis-server:6379</Queue>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>redis</ConnectorType>
  <Property><Name>channel</Name><Value>s3-events</Value></Property>
</QueueConfiguration>
```

## NATS

Publishes to a NATS subject via `async-nats`.

- **URL format**: `nats://host:4222`
- **Properties**:
  - `subject` — NATS subject (default `arca.notifications`).
  - `token` — NATS auth token.
  - `user`, `password` — user/password authentication.

```xml
<QueueConfiguration>
  <Id>nats-events</Id>
  <Queue>nats://nats:4222</Queue>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>nats</ConnectorType>
  <Property><Name>subject</Name><Value>s3.events</Value></Property>
</QueueConfiguration>
```

## MQTT

Publishes to an MQTT topic via `rumqttc`.

- **URL format**: `mqtt://host:1883`
- **Properties**:
  - `topic` — MQTT topic (default `arca/notifications`).
  - `user`, `password` — optional authentication.

```xml
<QueueConfiguration>
  <Id>mqtt-events</Id>
  <Queue>mqtt://mqtt-broker:1883</Queue>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>mqtt</ConnectorType>
  <Property><Name>topic</Name><Value>arca/events</Value></Property>
</QueueConfiguration>
```

## PostgreSQL

Inserts a row per event into a configurable table.

- **URL format**: `postgresql://user:pass@host:5432/dbname`
- **Properties**:
  - `table` — table name (default `arca_notifications`).
  - `schema` — PostgreSQL schema (default `public`).

```xml
<TopicConfiguration>
  <Id>pg-events</Id>
  <Topic>postgresql://arca:arca@db:5432/arca</Topic>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>postgresql</ConnectorType>
  <Property><Name>table</Name><Value>s3_audit_log</Value></Property>
</TopicConfiguration>
```

## MySQL / MariaDB

Inserts a row per event via `sqlx-mysql`.

- **URL format**: `mysql://user:pass@host:3306/dbname`
- **Properties**:
  - `table` — table name (default `arca_notifications`).

```xml
<TopicConfiguration>
  <Id>mysql-events</Id>
  <Topic>mysql://arca:arca@db:3306/arca</Topic>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>mysql</ConnectorType>
  <Property><Name>table</Name><Value>s3_audit_log</Value></Property>
</TopicConfiguration>
```

## MongoDB

Inserts a document per event via the official `mongodb` driver.

- **URL format**: `mongodb://host:27017`
- **Properties**:
  - `database` — database name (default `arca`).
  - `collection` — collection name (default `arca_notifications`).

```xml
<TopicConfiguration>
  <Id>mongo-events</Id>
  <Topic>mongodb://mongo:27017</Topic>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>mongodb</ConnectorType>
  <Property><Name>collection</Name><Value>s3-events</Value></Property>
</TopicConfiguration>
```

## Elasticsearch

Indexes each event as a document via the Elasticsearch REST API.

- **URL format**: `http://host:9200` or `https://host:9200`
- **Properties**:
  - `index` — index name (default `arca-notifications`).
  - `user`, `password` — optional basic authentication.

```xml
<TopicConfiguration>
  <Id>es-events</Id>
  <Topic>http://elasticsearch:9200</Topic>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>elasticsearch</ConnectorType>
  <Property><Name>index</Name><Value>s3-events</Value></Property>
</TopicConfiguration>
```

## Syslog (RFC 5424)

Sends each event as an RFC 5424 syslog message over UDP or TCP.

- **URL format**: `udp://host:514` or `tcp://host:1514`
- **Properties**:
  - `facility` — one of `kern`, `user`, `daemon`, `auth`, `syslog`, `local0`–`local7` (default `local0`).
  - `severity` — `emergency`, `alert`, `critical`, `error`, `warning`, `notice`, `informational`, `debug` (default `informational`).
  - `app_name` — the APP-NAME field in the syslog header (default `arca`).

```xml
<CloudFunctionConfiguration>
  <Id>syslog-events</Id>
  <CloudFunction>udp://siem.example.com:514</CloudFunction>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>syslog</ConnectorType>
  <Property><Name>facility</Name><Value>local4</Value></Property>
</CloudFunctionConfiguration>
```

## SMTP

Sends each event as an e-mail message via `lettre` over SMTP (cleartext with optional STARTTLS) or SMTPS (implicit TLS on port 465).

- **URL format**: `smtp://host[:port]` or `smtps://host[:port]`. Default ports are 25 (SMTP) and 465 (SMTPS).
- **Properties**:
  - `to` — recipient e-mail address (**required**).
  - `from` — sender address (default `arca@localhost`).
  - `subject` — custom subject line. When absent, the subject is generated as `Arca S3 Notification: <eventName>`.
  - `username`, `password` — optional PLAIN authentication.
  - `starttls` — set to `true` on `smtp://` endpoints to force STARTTLS upgrade.

The message body is the same S3 event JSON that the webhook connector posts.

```xml
<CloudFunctionConfiguration>
  <Id>smtp-alerts</Id>
  <CloudFunction>smtp://mail.example.com:587</CloudFunction>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>smtp</ConnectorType>
  <Property><Name>to</Name><Value>ops@example.com</Value></Property>
  <Property><Name>from</Name><Value>arca@example.com</Value></Property>
  <Property><Name>starttls</Name><Value>true</Value></Property>
  <Property><Name>username</Name><Value>arca</Value></Property>
  <Property><Name>password</Name><Value>s3cret</Value></Property>
  <Property><Name>subject</Name><Value>[Arca] New object uploaded</Value></Property>
</CloudFunctionConfiguration>
```

The SMTP timeout is controlled by `notifications.smtp_timeout_seconds` (default 15 s).

## gRPC

Delivers each event as a unary `arca.notifications.v1.NotificationService/Notify` call. Any server that implements this tiny proto contract receives the same S3 event JSON the webhook connector would post.

- **URL format**: `http://host:port` for HTTP/2 cleartext (h2c) or `https://host:port` for HTTP/2 over TLS.
- **Properties**:
  - `auth_token` — optional Bearer token forwarded as the `authorization` gRPC metadata header.
  - `ca_certificate` — optional PEM-encoded CA certificate used to trust servers presenting self-signed certs.
  - `domain_name` — override the TLS SNI / domain name used for certificate validation.
  - `insecure` — set to `true` to accept invalid TLS certificates (test only).
  - Any other property key/value is forwarded to the server in the proto `metadata` map so custom backends can route by tenant, channel, …

The proto contract (see `crates/arca-server/proto/arca_notifications.proto`):

```proto
syntax = "proto3";
package arca.notifications.v1;

service NotificationService {
  rpc Notify(NotificationRequest) returns (NotificationResponse);
}

message NotificationRequest {
  string event_payload = 1;
  string connector_id  = 2;
  map<string, string> metadata = 3;
}

message NotificationResponse {
  bool   success = 1;
  string message = 2;
}
```

```xml
<TopicConfiguration>
  <Id>grpc-events</Id>
  <Topic>https://grpc.example.com:50051</Topic>
  <Event>s3:ObjectCreated:*</Event>
  <ConnectorType>grpc</ConnectorType>
  <Property><Name>auth_token</Name><Value>secret</Value></Property>
  <Property><Name>tenant</Name><Value>acme</Value></Property>
</TopicConfiguration>
```

The gRPC timeout is controlled by `notifications.grpc_timeout_seconds` (default 10 s).

## Testing a connector

Every connector type can be probed without persisting an event:

```bash
curl -X POST http://localhost:9000/admin/notifications/test-connector \
  -H 'Content-Type: application/json' \
  --data '{"connector_type":"smtp","url":"smtp://mail.example.com:25","properties":{}}'
```

The Arca Console exposes the same probe as a **Test connection** button next to every destination form.

## Tuning timeouts

All connector operations inherit a per-connector timeout from `[notifications]`:

```toml
[notifications]
webhook_timeout_seconds = 10
kafka_timeout_seconds = 10
amqp_timeout_seconds = 5
redis_timeout_seconds = 5
nats_timeout_seconds = 5
mqtt_timeout_seconds = 5
postgresql_timeout_seconds = 5
mysql_timeout_seconds = 5
mongodb_timeout_seconds = 5
elasticsearch_timeout_seconds = 5
syslog_timeout_seconds = 5
smtp_timeout_seconds = 15
grpc_timeout_seconds = 10
```

Raise these on high-latency networks; lower them when you want fast-fail behavior behind a retry strategy.
