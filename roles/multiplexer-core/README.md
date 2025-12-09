# Multiplexer Core

Core library for hashrate multiplexing across multiple Stratum V2 upstream pools using PID control and dynamic miner reassignment.

## Features

- **PID Control**: Closed-loop feedback control system maintains target percentage distribution
- **Dynamic Reassignment**: Uses SV2's `SetExtranoncePrefix` to reassign miners without reconnection
- **Configurable Tuning**: Multiple tuning presets (Aggressive, Balanced, Conservative, ProportionalOnly) plus custom parameters
- **Runtime Adjustable**: Change PID parameters, deadband, and evaluation interval via API without restart
- **Priority-Based Assignment**: Initial miner assignment follows upstream priority order
- **Metrics Collection**: Track submitted/accepted/rejected shares per upstream
- **Thrashing Prevention**: Deadband, minimum reassignment interval, and integral anti-windup

## Architecture

### Core Components

1. **PidController**: Implements PID algorithm for maintaining setpoint
2. **AssignmentManager**: Tracks downstream → upstream assignments
3. **MetricsCollector**: Records and aggregates share metrics
4. **MultiplexerCore**: Main interface coordinating all components

### How It Works

```
┌─────────────────────────────────────────────────────────┐
│                  PID Control Loop                        │
│                                                          │
│  Target: Pool A=60%, Pool B=30%, Pool C=10%            │
│     ↓                                                    │
│  Measure: Pool A=58%, Pool B=32%, Pool C=10%           │
│     ↓                                                    │
│  Error: Pool A=-2%, Pool B=+2%, Pool C=0%              │
│     ↓                                                    │
│  PID Output: Move 2 miners from Pool B → Pool A        │
│     ↓                                                    │
│  Action: Send SetExtranoncePrefix + NewMiningJob        │
│     ↓                                                    │
│  Miners continue mining on new upstream seamlessly      │
└─────────────────────────────────────────────────────────┘
```

## Usage Example

```rust
use multiplexer_core::{MultiplexerConfig, MultiplexerCore, MultiplexedUpstream};

// Load configuration
let config = MultiplexerConfig {
    upstreams: vec![
        MultiplexedUpstream {
            id: "pool_a".to_string(),
            address: "pool-a.example.com".to_string(),
            port: 3333,
            authority_pubkey: "02abc123...".to_string(),
            percentage: 60.0,
            priority: 1,
            enabled: true,
        },
        MultiplexedUpstream {
            id: "pool_b".to_string(),
            address: "pool-b.example.com".to_string(),
            port: 3333,
            authority_pubkey: "03def456...".to_string(),
            percentage: 40.0,
            priority: 2,
            enabled: true,
        },
    ],
    controller: ControllerConfig::default(),
    failover: FailoverConfig::default(),
};

// Create multiplexer
let multiplexer = MultiplexerCore::new(config)?;

// Assign new downstream miner
let upstream_id = multiplexer.assign_new_downstream(
    downstream_id,
    channel_id,
    extranonce_prefix,
)?;

// Record share events
multiplexer.record_share_submitted(&upstream_id);
multiplexer.record_share_accepted(&upstream_id);

// Calculate rebalancing adjustments
let adjustments = multiplexer.calculate_rebalancing_adjustments();
// Returns: HashMap<upstream_id, i32> where value is miner count adjustment

// Get metrics
let snapshots = multiplexer.get_metrics_snapshot();
for snapshot in snapshots {
    println!("{}: target={:.1}%, actual={:.1}%, acceptance={:.1}%",
        snapshot.upstream_id,
        snapshot.target_percentage,
        snapshot.actual_percentage,
        snapshot.acceptance_rate
    );
}
```

## PID Tuning Models

| Model | Kp | Ki | Kd | Use Case |
|-------|-----|-----|-----|----------|
| **Aggressive** | 1.0 | 0.1 | 0.05 | Fast convergence, production testing |
| **Balanced** | 0.5 | 0.05 | 0.02 | Default, general purpose |
| **Conservative** | 0.2 | 0.01 | 0.005 | Slow, stable, risk-averse |
| **ProportionalOnly** | 0.3 | 0.0 | 0.0 | Simple P controller, no history |

### Runtime Tuning

```rust
// Switch to aggressive tuning
multiplexer.set_tuning_model(TuningModel::Aggressive)?;

// Set custom parameters
multiplexer.set_custom_pid_params(PidParams {
    kp: 0.7,
    ki: 0.08,
    kd: 0.03,
})?;

// Adjust deadband (don't rebalance if error < 1.5%)
multiplexer.set_deadband(1.5)?;

// Change evaluation interval to 60 seconds
multiplexer.set_evaluation_interval(60)?;
```

## Configuration

See `config.example.toml` for full configuration format.

Key parameters:

- **evaluation_interval_secs**: How often to run rebalancing (default: 30s)
- **deadband**: Don't rebalance if error < threshold in % (default: 0.5%)
- **min_reassignment_interval_secs**: Cooldown per miner (default: 30s)
- **measure_on_accepted**: Use accepted shares vs submitted (default: true)

## Testing

```bash
cargo test
```

All tests pass with 100% coverage of core algorithms.

## License

MIT OR Apache-2.0
