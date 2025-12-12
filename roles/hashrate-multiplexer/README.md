# Hashrate Multiplexer Daemon

Stratum V2 daemon for distributing hashrate across multiple upstream pools using PID control.

## Features

- **PID-based hashrate distribution** - Maintains target percentage allocation across pools
- **Dynamic rebalancing** - Reassigns miners using `SetExtranoncePrefix` without disconnection
- **Configurable tuning** - Multiple PID presets plus runtime adjustability
- **Priority-based failover** - Automatic redistribution when upstreams fail
- **Metrics & monitoring** - Track actual vs. target distribution in real-time

## Status

**Phase 1 Complete**: Core library (`multiplexer-core`) with PID controller, assignment manager, and metrics

**Phase 2 In Progress**: Daemon implementation
- ✅ Configuration system
- ✅ Rebalancer with PID control loop
- ✅ Main daemon orchestrator
- ⏳ SV2 downstream listener (TODO)
- ⏳ SV2 upstream connector (TODO)
- ⏳ SetExtranoncePrefix message handler (TODO)
- ⏳ REST API (TODO)

## Building

```bash
cargo build --release
```

## Configuration

See `config-examples/config.toml` for a full example.

Key sections:
- `[downstream]` - Where miners connect
- `[authority]` - Noise protocol keys
- `[controller]` - PID tuning and rebalancing settings
- `[[upstreams]]` - Pool configurations with percentage allocations

## Running

```bash
./target/release/hashrate-multiplexer --config config.toml
```

## Architecture

```
Miners (SV2)
    ↓
Downstream Listener
    ↓
Assignment Manager (priority-based initial assignment)
    ↓
PID Rebalancer (evaluation loop every 30s)
    ↓ (SetExtranoncePrefix commands)
Multiple Upstream Pools (60% / 30% / 10%)
```

## Development Roadmap

### Phase 2: Core Daemon (In Progress)
- [x] Configuration loading
- [x] Rebalancer with PID loop
- [x] Main orchestrator
- [ ] SV2 downstream handler
- [ ] SV2 upstream connector
- [ ] Message routing

### Phase 3: API & Metrics
- [ ] REST API for runtime tuning
- [ ] Prometheus metrics exporter
- [ ] Health check endpoints

### Phase 4: Integration
- [ ] Translator integration
- [ ] End-to-end testing with real pools
- [ ] Performance benchmarking

## Contributing

This daemon is part of the Stratum V2 reference implementation.

## License

MIT OR Apache-2.0
