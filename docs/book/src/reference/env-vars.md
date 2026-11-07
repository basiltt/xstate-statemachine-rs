# Environment variables

The library crates are configured through types and feature flags, not the environment; the
environment variables below drive the test suite, the CLI's colour handling, and the backend
service discovery. Production configuration for brokers and stores is passed as constructor
options (URLs, prefixes, credentials), not read from these.

## Backend service discovery (tests and development)

Each backend test reads one variable, falling back to a localhost default; set
`XSM_REQUIRE_SERVICES=1` to make an unreachable service a panic instead of a skipped test (CI does
this).

| Variable | Default | Server |
|---|---|---|
| `XSM_REDIS_URL` | `redis://127.0.0.1:6379/` | Redis Streams broker and `RedisStore` tests |
| `XSM_POSTGRES_URL` | `postgres://xsm:xsm@127.0.0.1:5432/xsm` | `PostgresStore` tests |
| `XSM_NATS_URL` | `nats://127.0.0.1:4222` | NATS (JetStream) broker tests |
| `XSM_AMQP_URL` | `amqp://guest:guest@127.0.0.1:5672/%2f` | RabbitMQ broker tests |
| `XSM_SQS_ENDPOINT` | `http://127.0.0.1:9324` | SQS-compatible endpoint (e.g. ElasticMQ) |
| `XSM_PYTHON` | (sibling checkout) | Python interpreter with the library, for cross-language tests |
| `XSM_REQUIRE_SERVICES` | unset | `1` forbids backend tests from skipping |

## CLI output

`xsm` writes plain text when piped; `--no-color`, `NO_COLOR` and `XSM_NO_COLOR` all disable
colour, and `TERM=dumb` suppresses box-drawing characters.

See the [feature flags](../guide/features.md) page for the Cargo features that gate each backend.
