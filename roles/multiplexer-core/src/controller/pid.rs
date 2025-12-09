use crate::config::PidParams;
use std::time::Instant;

/// PID controller for maintaining target setpoint
#[derive(Debug)]
pub struct PidController {
    /// PID parameters
    params: PidParams,

    /// Integral accumulator (with anti-windup)
    integral: f64,

    /// Last error value (for derivative calculation)
    last_error: f64,

    /// Last update timestamp
    last_update: Instant,

    /// Integral windup limits
    integral_limit: f64,
}

impl PidController {
    /// Create a new PID controller with given parameters
    pub fn new(params: PidParams) -> Self {
        Self {
            params,
            integral: 0.0,
            last_error: 0.0,
            last_update: Instant::now(),
            integral_limit: 100.0, // Prevent integral windup
        }
    }

    /// Update PID controller with new measurement
    ///
    /// # Arguments
    /// * `setpoint` - Target value (e.g., 60.0 for 60%)
    /// * `measurement` - Actual measured value (e.g., 58.2 for 58.2%)
    ///
    /// # Returns
    /// Control output (adjustment needed in percentage points)
    pub fn update(&mut self, setpoint: f64, measurement: f64) -> f64 {
        let error = setpoint - measurement;
        let dt = self.last_update.elapsed().as_secs_f64();

        // Proportional term
        let p = self.params.kp * error;

        // Integral term (with anti-windup clamping)
        if dt > 0.0 {
            self.integral += error * dt;
            self.integral = self.integral.clamp(-self.integral_limit, self.integral_limit);
        }
        let i = self.params.ki * self.integral;

        // Derivative term (rate of change of error)
        let d = if dt > 0.0 {
            self.params.kd * (error - self.last_error) / dt
        } else {
            0.0
        };

        // Update state
        self.last_error = error;
        self.last_update = Instant::now();

        // Return total control output
        p + i + d
    }

    /// Reset controller state (useful when setpoint changes dramatically)
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.last_error = 0.0;
        self.last_update = Instant::now();
    }

    /// Update PID parameters
    pub fn set_params(&mut self, params: PidParams) {
        self.params = params;
    }

    /// Get current parameters
    pub fn params(&self) -> &PidParams {
        &self.params
    }

    /// Get current integral value (for debugging)
    pub fn integral(&self) -> f64 {
        self.integral
    }

    /// Get last error value (for debugging)
    pub fn last_error(&self) -> f64 {
        self.last_error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_proportional_only() {
        let params = PidParams {
            kp: 0.5,
            ki: 0.0,
            kd: 0.0,
        };

        let mut controller = PidController::new(params);

        // Target: 60%, Actual: 50%, Error: +10%
        let output = controller.update(60.0, 50.0);

        // Expected: 0.5 * 10.0 = 5.0
        assert_relative_eq!(output, 5.0, epsilon = 0.001);
    }

    #[test]
    fn test_integral_accumulation() {
        let params = PidParams {
            kp: 0.0,
            ki: 0.1,
            kd: 0.0,
        };

        let mut controller = PidController::new(params);

        // Simulate constant error over time
        controller.update(60.0, 50.0); // Error: +10%
        thread::sleep(Duration::from_millis(100));

        controller.update(60.0, 50.0); // Error: +10%
        thread::sleep(Duration::from_millis(100));

        let output = controller.update(60.0, 50.0); // Error: +10%

        // Integral should accumulate over time
        assert!(output > 0.0);
        assert!(controller.integral() > 0.0);
    }

    #[test]
    fn test_derivative_dampening() {
        let params = PidParams {
            kp: 0.0,
            ki: 0.0,
            kd: 1.0,
        };

        let mut controller = PidController::new(params);

        // Error decreasing rapidly (good sign - system responding)
        controller.update(60.0, 50.0); // Error: +10%
        thread::sleep(Duration::from_millis(100));

        let output = controller.update(60.0, 55.0); // Error: +5% (improved)

        // Derivative should produce negative output (dampening)
        // because error is decreasing
        assert!(output < 0.0);
    }

    #[test]
    fn test_full_pid() {
        let params = PidParams {
            kp: 0.5,
            ki: 0.05,
            kd: 0.02,
        };

        let mut controller = PidController::new(params);

        // Simulate approaching setpoint
        let measurements = vec![50.0, 52.0, 55.0, 57.0, 59.0, 60.0];
        let mut outputs = Vec::new();

        for measurement in measurements {
            thread::sleep(Duration::from_millis(50));
            let output = controller.update(60.0, measurement);
            outputs.push(output);
        }

        // Output should decrease as we approach setpoint
        assert!(outputs[0] > outputs[outputs.len() - 1]);

        // At setpoint, output should be near zero (just integral term)
        assert!(outputs[outputs.len() - 1].abs() < 1.0);
    }

    #[test]
    fn test_integral_windup_protection() {
        let params = PidParams {
            kp: 0.0,
            ki: 10.0, // Very high integral gain
            kd: 0.0,
        };

        let mut controller = PidController::new(params);

        // Simulate large sustained error
        for _ in 0..100 {
            thread::sleep(Duration::from_millis(10));
            controller.update(100.0, 0.0); // Huge error: 100%
        }

        // Integral should be clamped
        assert!(controller.integral() <= 100.0);
        assert!(controller.integral() >= -100.0);
    }

    #[test]
    fn test_reset() {
        let params = PidParams {
            kp: 0.5,
            ki: 0.1,
            kd: 0.02,
        };

        let mut controller = PidController::new(params);

        // Build up some state
        controller.update(60.0, 50.0);
        thread::sleep(Duration::from_millis(100));
        controller.update(60.0, 50.0);

        assert!(controller.integral() > 0.0);
        assert!(controller.last_error() > 0.0);

        // Reset
        controller.reset();

        assert_eq!(controller.integral(), 0.0);
        assert_eq!(controller.last_error(), 0.0);
    }

    #[test]
    fn test_zero_error() {
        let params = PidParams {
            kp: 0.5,
            ki: 0.05,
            kd: 0.02,
        };

        let mut controller = PidController::new(params);

        // At setpoint
        let output = controller.update(60.0, 60.0);

        // Should be zero (or very close due to floating point)
        assert_relative_eq!(output, 0.0, epsilon = 0.001);
    }

    #[test]
    fn test_negative_error() {
        let params = PidParams {
            kp: 0.5,
            ki: 0.0,
            kd: 0.0,
        };

        let mut controller = PidController::new(params);

        // Over setpoint: Target 60%, Actual 70%, Error: -10%
        let output = controller.update(60.0, 70.0);

        // Expected: 0.5 * -10.0 = -5.0
        assert_relative_eq!(output, -5.0, epsilon = 0.001);
    }
}
