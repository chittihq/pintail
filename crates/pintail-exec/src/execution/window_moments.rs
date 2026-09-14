use std::cell::Cell;
use std::ops::Range;

use pintail_types::Value;

use super::ExecError;

thread_local! {
    static HIGH_PRECISION: Cell<bool> = const { Cell::new(true) };
}

/// Install the statement's preference for high-precision window aggregation.
pub fn set_session_window_high_precision(value: Option<bool>) {
    HIGH_PRECISION.set(value.unwrap_or(true));
}

/// The precision preference captured by windows compiled on this thread.
#[must_use]
pub fn session_window_high_precision() -> bool {
    HIGH_PRECISION.get()
}

/// Moving second moments measured relative to the first non-NULL value.
/// Subtraction removes expired rows without visiting the remaining frame.
#[derive(Default)]
pub(super) struct MovingMoments {
    origin: Option<f64>,
    count: u64,
    sum: f64,
    squares: f64,
    start: usize,
    end: usize,
}

impl MovingMoments {
    pub(super) fn advance(
        &mut self,
        keys: &[Vec<Value>],
        partition: &[usize],
        argument: usize,
        frame: Range<usize>,
    ) -> Result<(), ExecError> {
        while self.start < frame.start && self.start < self.end {
            self.update(&keys[partition[self.start]][argument], false)?;
            self.start += 1;
        }
        self.start = frame.start;
        self.end = self.end.max(frame.start);
        while self.end < frame.end {
            self.update(&keys[partition[self.end]][argument], true)?;
            self.end += 1;
        }
        Ok(())
    }

    fn update(&mut self, value: &Value, adding: bool) -> Result<(), ExecError> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        let value = crate::expression::mysql_f64(value)?;
        let origin = *self.origin.get_or_insert(value);
        let delta = value - origin;
        if adding {
            self.count += 1;
            self.sum += delta;
            self.squares += delta * delta;
        } else {
            self.count -= 1;
            self.sum -= delta;
            self.squares -= delta * delta;
            if self.count == 0 {
                self.origin = None;
                self.sum = 0.0;
                self.squares = 0.0;
            }
        }
        Ok(())
    }

    pub(super) fn finish(&self, sample: bool, stddev: bool) -> Value {
        if self.count == 0 || (sample && self.count == 1) {
            return Value::Null;
        }
        if self.count == 1 {
            return Value::float64(0.0);
        }
        #[allow(clippy::cast_precision_loss)]
        let count = self.count as f64;
        let variance = ((self.squares - self.sum * self.sum / count)
            / (count - f64::from(u8::from(sample))))
        .max(0.0);
        Value::float64(if stddev { variance.sqrt() } else { variance })
    }
}
