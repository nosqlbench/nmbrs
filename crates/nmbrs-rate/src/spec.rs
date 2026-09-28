// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Rate limiter configuration.
//!
//! Parses a rate spec string like `"1000"`, `"1000,1.5"`, or
//! `"1000,1.1,restart"` into structured parameters.

/// Time unit for internal tick accounting.
///
/// Scaled based on target rate to keep ticks-per-op within u32 range,
/// matching the nosqlbench time-scaling trick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeUnit {
    Nanos,
    Micros,
    Millis,
    Seconds,
}

impl TimeUnit {
    /// Select the appropriate time unit for a given rate.
    pub fn for_rate(ops_per_sec: f64) -> Self {
        if ops_per_sec > 1.0 {
            TimeUnit::Nanos
        } else if ops_per_sec > 0.001 {
            TimeUnit::Micros
        } else if ops_per_sec > 0.000001 {
            TimeUnit::Millis
        } else {
            TimeUnit::Seconds
        }
    }

    /// Convert nanoseconds to ticks in this unit.
    pub fn nanos_to_ticks(self, nanos: u64) -> u32 {
        let ticks = match self {
            TimeUnit::Nanos => nanos,
            TimeUnit::Micros => nanos / 1_000,
            TimeUnit::Millis => nanos / 1_000_000,
            TimeUnit::Seconds => nanos / 1_000_000_000,
        };
        ticks.min(u32::MAX as u64) as u32
    }

    /// Convert ticks to nanoseconds.
    pub fn ticks_to_nanos(self, ticks: u32) -> u64 {
        match self {
            TimeUnit::Nanos => ticks as u64,
            TimeUnit::Micros => ticks as u64 * 1_000,
            TimeUnit::Millis => ticks as u64 * 1_000_000,
            TimeUnit::Seconds => ticks as u64 * 1_000_000_000,
        }
    }

    /// Ticks per operation at the given rate.
    pub fn ticks_per_op(self, ops_per_sec: f64) -> u32 {
        let ticks = match self {
            TimeUnit::Nanos => 1_000_000_000.0 / ops_per_sec,
            TimeUnit::Micros => 1_000_000.0 / ops_per_sec,
            TimeUnit::Millis => 1_000.0 / ops_per_sec,
            TimeUnit::Seconds => 1.0 / ops_per_sec,
        };
        (ticks as u64).min(u32::MAX as u64) as u32
    }
}

/// Lifecycle verb for rate limiter control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verb {
    /// Start the rate limiter.
    Start,
    /// Configure without affecting running state.
    Configure,
    /// Restart (zero all pools and backlog).
    Restart,
    /// Stop the refill task.
    Stop,
}

/// Parsed rate limiter configuration.
#[derive(Debug, Clone)]
pub struct RateSpec {
    pub ops_per_sec: f64,
    pub burst_ratio: f64,
    pub verb: Verb,
    pub unit: TimeUnit,
}

impl RateSpec {
    /// Create a rate spec with defaults.
    pub fn new(ops_per_sec: f64) -> Self {
        Self::with_burst(ops_per_sec, 1.1)
    }

    /// Create with explicit burst ratio.
    pub fn with_burst(ops_per_sec: f64, burst_ratio: f64) -> Self {
        Self {
            ops_per_sec,
            burst_ratio,
            verb: Verb::Start,
            unit: TimeUnit::for_rate(ops_per_sec),
        }
    }

    /// Parse a rate spec string.
    ///
    /// Formats:
    /// - `"1000"` — 1000 ops/s, 1.1x burst, start
    /// - `"1000,1.5"` — 1000 ops/s, 1.5x burst
    /// - `"1000,1.1,restart"` — with verb
    pub fn parse(spec: &str) -> Result<Self, String> {
        let parts: Vec<&str> = spec.split(',').map(|s| s.trim()).collect();
        if parts.is_empty() {
            return Err("empty rate spec".into());
        }

        let ops_per_sec: f64 = parts[0].parse().map_err(|e| format!("invalid rate: {e}"))?;
        if ops_per_sec <= 0.0 {
            return Err("rate must be positive".into());
        }

        let burst_ratio = if parts.len() > 1 {
            parts[1].parse::<f64>().unwrap_or(1.1)
        } else {
            1.1
        };

        let verb = if parts.len() > 2 {
            match parts[2].to_lowercase().as_str() {
                "start" => Verb::Start,
                "configure" => Verb::Configure,
                "restart" => Verb::Restart,
                "stop" => Verb::Stop,
                other => return Err(format!("unknown verb: {other}")),
            }
        } else {
            Verb::Start
        };

        Ok(Self {
            ops_per_sec,
            burst_ratio,
            verb,
            unit: TimeUnit::for_rate(ops_per_sec),
        })
    }

    /// Ticks per operation in the configured time unit.
    pub fn ticks_per_op(&self) -> u32 {
        self.unit.ticks_per_op(self.ops_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_unit_scaling() {
        assert_eq!(TimeUnit::for_rate(50.0), TimeUnit::Nanos);
        assert_eq!(TimeUnit::for_rate(0.5), TimeUnit::Micros);
        assert_eq!(TimeUnit::for_rate(0.0005), TimeUnit::Millis);
        assert_eq!(TimeUnit::for_rate(0.0000001), TimeUnit::Seconds);
    }

    #[test]
    fn ticks_per_op_fits() {
        // 50 ops/s in nanos: 20_000_000 — fits in u32
        let unit = TimeUnit::Nanos;
        assert_eq!(unit.ticks_per_op(50.0), 20_000_000);

        // 0.5 ops/s in micros: 2_000_000 — fits
        let unit = TimeUnit::Micros;
        assert_eq!(unit.ticks_per_op(0.5), 2_000_000);
    }

    #[test]
    fn nanos_ticks_roundtrip() {
        let unit = TimeUnit::Micros;
        let nanos = 5_000_000u64; // 5ms
        let ticks = unit.nanos_to_ticks(nanos);
        assert_eq!(ticks, 5000);
        assert_eq!(unit.ticks_to_nanos(ticks), nanos);
    }

    #[test]
    fn parse_simple() {
        let spec = RateSpec::parse("1000").unwrap();
        assert_eq!(spec.ops_per_sec, 1000.0);
        assert_eq!(spec.burst_ratio, 1.1);
        assert_eq!(spec.verb, Verb::Start);
    }

    #[test]
    fn parse_with_burst() {
        let spec = RateSpec::parse("500, 1.5").unwrap();
        assert_eq!(spec.ops_per_sec, 500.0);
        assert_eq!(spec.burst_ratio, 1.5);
    }

    #[test]
    fn parse_with_verb() {
        let spec = RateSpec::parse("1000, 1.1, restart").unwrap();
        assert_eq!(spec.verb, Verb::Restart);
    }

    #[test]
    fn parse_errors() {
        assert!(RateSpec::parse("").is_err());
        assert!(RateSpec::parse("-1").is_err());
        assert!(RateSpec::parse("abc").is_err());
    }
}
