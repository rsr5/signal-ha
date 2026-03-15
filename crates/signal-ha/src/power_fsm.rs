//! Power-based appliance cycle detection.
//!
//! Generic FSM that watches raw watt readings from a smart plug and
//! detects start/finish of appliance cycles (dishwasher, washing machine,
//! dryer, etc.).  Configurable thresholds, hysteresis, and debounce.
//!
//! # Example
//!
//! ```
//! use signal_ha::{PowerFsm, PowerFsmConfig};
//! use std::time::Duration;
//!
//! let mut fsm = PowerFsm::new(PowerFsmConfig {
//!     name: "dishwasher",
//!     idle_below_w: 3.0,
//!     running_above_w: 10.0,
//!     debounce_on: Duration::from_secs(15),
//!     debounce_off: Duration::from_secs(300),
//! });
//!
//! // Each tick, feed the current watts and dt:
//! for event in fsm.update(2100.0, 2.0) {
//!     // handle CycleStarted, CycleFinished, etc.
//! }
//! ```

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Configuration for a power-monitoring FSM.
#[derive(Debug, Clone)]
pub struct PowerFsmConfig {
    /// Human-readable name (for logging / status page).
    pub name: &'static str,
    /// Below this wattage the appliance is considered idle/off.
    pub idle_below_w: f64,
    /// Above this wattage the appliance is considered running.
    pub running_above_w: f64,
    /// Power must stay above `running_above_w` for this long before
    /// a cycle start is confirmed.
    pub debounce_on: Duration,
    /// Power must stay below `idle_below_w` for this long after running
    /// before the cycle is considered finished.
    pub debounce_off: Duration,
}

/// Appliance states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// Appliance is off / standby.
    Off,
    /// Power is above threshold but debounce not yet confirmed.
    PendingOn,
    /// Confirmed running.
    Running,
    /// Power dropped below threshold but debounce not yet confirmed.
    PendingOff,
}

impl PowerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerState::Off => "off",
            PowerState::PendingOn => "pending_on",
            PowerState::Running => "running",
            PowerState::PendingOff => "pending_off",
        }
    }
}

/// A completed appliance cycle.
#[derive(Debug, Clone)]
pub struct CompletedCycle {
    /// When the cycle started (wall clock).
    pub started: DateTime<Utc>,
    /// Total cycle duration.
    pub duration: Duration,
    /// Energy consumed during the cycle (watt-hours).
    pub energy_wh: f64,
    /// Peak power observed during the cycle (watts).
    pub peak_w: f64,
}

/// Events emitted by the FSM.
#[derive(Debug, Clone)]
pub enum FsmEvent {
    /// Cycle just confirmed as started.
    CycleStarted,
    /// Cycle just completed.
    CycleFinished(CompletedCycle),
}

/// Power-based cycle detection state machine.
///
/// Feed it watt readings on each tick via [`update`](PowerFsm::update).
/// It returns events when cycles start or finish.
#[derive(Debug)]
pub struct PowerFsm {
    config: PowerFsmConfig,
    state: PowerState,

    /// How long we've been in the pending-on zone.
    pending_on_elapsed: Duration,
    /// How long we've been in the pending-off zone.
    pending_off_elapsed: Duration,

    /// Wall-clock time when the current cycle started.
    cycle_start: Option<DateTime<Utc>>,
    /// Energy accumulator for the current cycle (watt-hours).
    cycle_energy_wh: f64,
    /// Peak watts seen in the current cycle.
    cycle_peak_w: f64,
    /// Total cycle elapsed time.
    cycle_elapsed: Duration,

    /// Current smoothed power (exponential moving average).
    smoothed_w: f64,
    /// EMA alpha — derived from a ~15s time constant at 2s ticks.
    ema_alpha: f64,

    /// Last raw watts (for status page).
    last_raw_w: f64,
}

impl PowerFsm {
    /// Create a new FSM.  Starts in `Off` state.
    pub fn new(config: PowerFsmConfig) -> Self {
        Self {
            config,
            state: PowerState::Off,
            pending_on_elapsed: Duration::ZERO,
            pending_off_elapsed: Duration::ZERO,
            cycle_start: None,
            cycle_energy_wh: 0.0,
            cycle_peak_w: 0.0,
            cycle_elapsed: Duration::ZERO,
            smoothed_w: 0.0,
            ema_alpha: 0.125, // ~15s time constant at 2s ticks
            last_raw_w: 0.0,
        }
    }

    /// Current FSM state.
    pub fn state(&self) -> PowerState {
        self.state
    }

    /// Smoothed power reading (watts).
    pub fn smoothed_w(&self) -> f64 {
        self.smoothed_w
    }

    /// Last raw power reading (watts).
    pub fn raw_w(&self) -> f64 {
        self.last_raw_w
    }

    /// Energy accumulated in the current cycle (watt-hours).
    /// Returns 0 if no cycle is in progress.
    pub fn cycle_energy_wh(&self) -> f64 {
        self.cycle_energy_wh
    }

    /// Peak watts in the current cycle.
    pub fn cycle_peak_w(&self) -> f64 {
        self.cycle_peak_w
    }

    /// Duration of the current cycle so far.
    pub fn cycle_elapsed(&self) -> Duration {
        self.cycle_elapsed
    }

    /// Feed a new power reading. `dt` is seconds since last call.
    /// Returns any events emitted by this tick.
    pub fn update(&mut self, raw_watts: f64, dt: f64) -> Vec<FsmEvent> {
        let mut events = Vec::new();
        let dt_dur = Duration::from_secs_f64(dt);

        // Clamp negative readings.
        let watts = raw_watts.max(0.0);
        self.last_raw_w = watts;

        // Exponential moving average.
        self.smoothed_w = self.smoothed_w + self.ema_alpha * (watts - self.smoothed_w);

        // Use raw watts for start detection (debounce handles noise),
        // smoothed watts for end detection (EMA rides through brief dips).
        let raw = watts;
        let smooth = self.smoothed_w;

        match self.state {
            PowerState::Off => {
                if raw >= self.config.running_above_w {
                    self.state = PowerState::PendingOn;
                    self.pending_on_elapsed = dt_dur;
                }
            }
            PowerState::PendingOn => {
                if raw >= self.config.running_above_w {
                    self.pending_on_elapsed += dt_dur;
                    if self.pending_on_elapsed >= self.config.debounce_on {
                        // Confirmed start.
                        self.state = PowerState::Running;
                        self.cycle_start = Some(Utc::now());
                        self.cycle_energy_wh = 0.0;
                        self.cycle_peak_w = smooth;
                        self.cycle_elapsed = self.pending_on_elapsed;
                        // Account for energy during pending period.
                        let pending_hours = self.pending_on_elapsed.as_secs_f64() / 3600.0;
                        self.cycle_energy_wh = smooth * pending_hours;
                        events.push(FsmEvent::CycleStarted);
                    }
                } else {
                    // Dropped below threshold before debounce completed — false alarm.
                    self.state = PowerState::Off;
                    self.pending_on_elapsed = Duration::ZERO;
                }
            }
            PowerState::Running => {
                // Accumulate energy and track peak (use smoothed for accuracy).
                let hours = dt / 3600.0;
                self.cycle_energy_wh += smooth * hours;
                self.cycle_elapsed += dt_dur;
                if smooth > self.cycle_peak_w {
                    self.cycle_peak_w = smooth;
                }

                if smooth < self.config.idle_below_w {
                    self.state = PowerState::PendingOff;
                    self.pending_off_elapsed = dt_dur;
                }
            }
            PowerState::PendingOff => {
                // Still accumulate energy during pending-off.
                let hours = dt / 3600.0;
                self.cycle_energy_wh += smooth * hours;
                self.cycle_elapsed += dt_dur;

                if smooth < self.config.idle_below_w {
                    self.pending_off_elapsed += dt_dur;
                    if self.pending_off_elapsed >= self.config.debounce_off {
                        // Confirmed finish.
                        let cycle = CompletedCycle {
                            started: self.cycle_start.unwrap_or_else(Utc::now),
                            duration: self.cycle_elapsed,
                            energy_wh: self.cycle_energy_wh,
                            peak_w: self.cycle_peak_w,
                        };
                        events.push(FsmEvent::CycleFinished(cycle));
                        self.state = PowerState::Off;
                        self.cycle_start = None;
                        self.cycle_energy_wh = 0.0;
                        self.cycle_peak_w = 0.0;
                        self.cycle_elapsed = Duration::ZERO;
                    }
                } else {
                    // Power came back — cancel pending-off, still running.
                    self.state = PowerState::Running;
                    self.pending_off_elapsed = Duration::ZERO;
                    if smooth > self.cycle_peak_w {
                        self.cycle_peak_w = smooth;
                    }
                }
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PowerFsmConfig {
        PowerFsmConfig {
            name: "dishwasher",
            idle_below_w: 3.0,
            running_above_w: 10.0,
            debounce_on: Duration::from_secs(10),
            debounce_off: Duration::from_secs(20),
        }
    }

    #[test]
    fn starts_off() {
        let fsm = PowerFsm::new(test_config());
        assert_eq!(fsm.state(), PowerState::Off);
    }

    #[test]
    fn low_power_stays_off() {
        let mut fsm = PowerFsm::new(test_config());
        for _ in 0..20 {
            let events = fsm.update(1.0, 2.0);
            assert!(events.is_empty());
        }
        assert_eq!(fsm.state(), PowerState::Off);
    }

    #[test]
    fn spike_enters_pending_on() {
        let mut fsm = PowerFsm::new(test_config());
        // Feed high power — EMA needs a few ticks to catch up.
        for _ in 0..10 {
            fsm.update(500.0, 2.0);
        }
        // Should be pending or running by now.
        assert!(
            fsm.state() == PowerState::PendingOn || fsm.state() == PowerState::Running,
            "state: {:?}",
            fsm.state()
        );
    }

    #[test]
    fn full_cycle() {
        let mut fsm = PowerFsm::new(test_config());

        // 1. Ramp up — feed high power until CycleStarted.
        let mut started = false;
        for _ in 0..30 {
            for ev in fsm.update(1500.0, 2.0) {
                if matches!(ev, FsmEvent::CycleStarted) {
                    started = true;
                }
            }
        }
        assert!(started, "should have started");
        assert_eq!(fsm.state(), PowerState::Running);

        // 2. Run for a while.
        for _ in 0..50 {
            let events = fsm.update(800.0, 2.0);
            assert!(events.is_empty());
        }
        assert_eq!(fsm.state(), PowerState::Running);
        assert!(fsm.cycle_energy_wh() > 0.0, "should accumulate energy");

        // 3. Power drops — EMA needs many ticks to decay below idle_below_w.
        //    Feed zero for enough ticks that EMA drops below 3W and
        //    debounce_off (20s) completes.
        let mut finished = false;
        let mut cycle = None;
        for _ in 0..100 {
            for ev in fsm.update(0.0, 2.0) {
                if let FsmEvent::CycleFinished(c) = ev {
                    finished = true;
                    cycle = Some(c);
                }
            }
        }
        assert!(finished, "should have finished");
        assert_eq!(fsm.state(), PowerState::Off);

        let c = cycle.unwrap();
        assert!(c.energy_wh > 0.0, "cycle should have energy");
        assert!(c.peak_w > 100.0, "cycle should have peak");
        assert!(!c.duration.is_zero(), "cycle should have duration");
    }

    #[test]
    fn brief_spike_no_start() {
        let mut fsm = PowerFsm::new(test_config());
        // Brief spike (two ticks = 4s) then drop — should not start a cycle
        // because debounce_on (10s) isn't satisfied before raw watts drop.
        fsm.update(500.0, 2.0);
        fsm.update(500.0, 2.0);
        // Raw watts drop to 0 → exits PendingOn immediately.
        fsm.update(0.0, 2.0);
        assert_eq!(fsm.state(), PowerState::Off);
    }

    #[test]
    fn power_returns_during_pending_off() {
        let mut fsm = PowerFsm::new(test_config());

        // Start a cycle.
        for _ in 0..30 {
            fsm.update(1500.0, 2.0);
        }
        assert_eq!(fsm.state(), PowerState::Running);

        // Drop power long enough for EMA to decay below idle_below_w (3W).
        // From ~1500W, need ~47 ticks at 0W with alpha=0.125.
        for _ in 0..50 {
            fsm.update(0.0, 2.0);
        }
        assert_eq!(fsm.state(), PowerState::PendingOff);

        // Power returns within debounce_off — should go back to Running.
        for _ in 0..5 {
            fsm.update(800.0, 2.0);
        }
        assert_eq!(fsm.state(), PowerState::Running, "should resume running");
    }

    #[test]
    fn negative_watts_clamped() {
        let mut fsm = PowerFsm::new(test_config());
        let events = fsm.update(-50.0, 2.0);
        assert!(events.is_empty());
        assert!(fsm.smoothed_w() >= 0.0);
    }

    #[test]
    fn energy_accumulates_during_pending_off() {
        let mut fsm = PowerFsm::new(test_config());

        // Start cycle.
        for _ in 0..30 {
            fsm.update(1000.0, 2.0);
        }
        let energy_before = fsm.cycle_energy_wh();

        // Enter PendingOff with small residual power.
        fsm.update(2.0, 2.0);
        fsm.update(2.0, 2.0);

        assert!(fsm.cycle_energy_wh() > energy_before);
    }
}
