//! Aggregate compilation, accumulator state, spill and the hash,
//! scan-fused and join-fused aggregate builders.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, hash_map::Entry},
    hash::{DefaultHasher, Hash, Hasher},
};

use pintail_sql::{
    AggregateFunction, BoundAggregate, BoundColumn, BoundJoinKind, BoundOrderKey, DatePart,
    ScalarFunction,
};
use pintail_types::{DataType, Value};

use crate::collation::Collation;
use rayon::prelude::*;

use super::join::{
    JoinGroupPlan, JoinHashKey, MAX_DENSE_SPAN, PartitionedBuild, build_hash_join_state,
    normalized_collation_value, normalized_hash_key, normalized_join_key, resolve_join_group_plan,
};
use super::two_pass::{
    TwoPassKeySource, TwoPassLane, build_streaming_two_pass_aggregate, two_pass_lanes,
};
use super::{
    DenseJoinTable, ExecError, HASH_ENTRY_OVERHEAD, JoinKeyMode, MaterializedRows, MemoryTracker,
    OneShotStream, PullOperator, SESSION_GROUP_CONCAT_MAX_LEN, SESSION_GROUP_CONCAT_WARNINGS,
    compare_sort_values, estimated_row_payload_bytes, reserve_hash_map_entries,
    reserve_hash_set_entries, reserve_vec_elements, scalar_string_memory_upper_bound,
};
use crate::{
    ColumnVector, RecordBatch,
    expression::{
        CompiledExpr, compare_mysql, compare_utf8_mysql, mysql_f64, mysql_i64, mysql_u64,
    },
    spill,
};

pub(super) struct CompiledAggregate {
    pub(super) function: AggregateFunction,
    pub(super) expr: Option<CompiledExpr>,
    pub(super) input_type: Option<DataType>,
    pub(super) distinct: bool,
    pub(super) data_type: Option<DataType>,
    /// `GROUP_CONCAT` separator (`MySQL` defaults to a comma).
    pub(super) separator: String,
    /// `GROUP_CONCAT ... ORDER BY` keys as `(expr, ascending, decimal)`.
    pub(super) order_within: Vec<(CompiledExpr, bool, bool)>,
    /// The collation this plan compares text with, resolved once at bind
    /// time. Carried on the compiled aggregate because every operator that
    /// touches a text value already holds one, which keeps it from having to
    /// travel as a parameter beside the data it describes.
    pub(super) collation: Collation,
}

impl CompiledAggregate {
    pub(super) fn compile(
        aggregate: &BoundAggregate,
        columns: &[BoundColumn],
        collation: Collation,
    ) -> Result<Self, ExecError> {
        let input_type = aggregate
            .expr
            .as_ref()
            .and_then(|expression| expression.data_type);
        // COUNT(DISTINCT col) folds col's values under COL's collation: a
        // general_ci column PAD-folds 'red' and 'red ' into one distinct
        // value whatever collation the rest of the plan resolved.
        let collation = aggregate
            .expr
            .as_ref()
            .and_then(|expression| {
                // DISTINCT over JSON folds documents structurally; text
                // arguments fold under their own column collation.
                if expression.data_type == Some(pintail_types::DataType::Json) {
                    return Some(Collation::Json);
                }
                expression
                    .text_collation()
                    .and_then(Collation::from_mysql_name)
            })
            .unwrap_or(collation);
        Ok(Self {
            collation,
            function: aggregate.function,
            expr: aggregate
                .expr
                .as_ref()
                .map(|expression| CompiledExpr::compile(expression, columns, collation))
                .transpose()?,
            input_type,
            distinct: aggregate.distinct,
            data_type: aggregate.data_type,
            separator: aggregate
                .separator
                .clone()
                .unwrap_or_else(|| ",".to_owned()),
            order_within: aggregate
                .order_within
                .iter()
                .map(|(expression, ascending)| {
                    Ok::<_, ExecError>((
                        CompiledExpr::compile(expression, columns, collation)?,
                        *ascending,
                        matches!(expression.data_type, Some(DataType::Decimal { .. })),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

struct AggregateGroup {
    values: Vec<Value>,
    states: Vec<AggregateState>,
}

#[derive(Clone)]
/// DISTINCT key set. Integer-keyed values dedup through a plain i128 set
/// (no Value allocation, no enum-cell hashing — e16 measured 2.6x); the
/// first non-integer key migrates the set to normalized Values.
enum DistinctSeen {
    Ints(HashSet<i128, std::hash::BuildHasherDefault<IntKeyHasher>>),
    Values(HashSet<Value>),
}

/// splitmix-style hasher for raw integer distinct keys: `SipHash` cost is
/// pure overhead here — the keys are column data in a per-query set, not
/// a persistent attacker-fed table.
#[derive(Default)]
struct IntKeyHasher(u64);

impl std::hash::Hasher for IntKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("integer distinct keys hash through write_i128");
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn write_i128(&mut self, value: i128) {
        let low = value as u64;
        let high = (value >> 64) as u64;
        self.0 = crate::batch::mix64(low ^ high.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
}

/// splitmix-style hasher for the two-pass `(group sentinel, seen)` map keys:
/// like [`IntKeyHasher`], the keys are per-query column data, so `SipHash`'s
/// `DoS` resistance buys nothing.
#[derive(Default)]
pub(super) struct GroupKeyHasher(u64);

impl std::hash::Hasher for GroupKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("two-pass group keys hash through write_u64/write_u8");
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = crate::batch::mix64(value ^ self.0);
    }

    fn write_u8(&mut self, value: u8) {
        self.0 ^= u64::from(value).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

pub(super) type GroupKeyMap =
    HashMap<(u64, bool), Vec<AggregateState>, std::hash::BuildHasherDefault<GroupKeyHasher>>;

fn int_distinct_key(value: &Value) -> Option<i128> {
    match value {
        Value::Int64(value) => Some(i128::from(*value)),
        Value::UInt64(value) => Some(i128::from(*value)),
        _ => None,
    }
}

fn int_key_value(key: i128) -> Value {
    i64::try_from(key).map_or_else(
        |_| Value::UInt64(u64::try_from(key).expect("distinct int keys fit u64")),
        Value::Int64,
    )
}

impl DistinctSeen {
    /// Returns whether the key is new. `false` means the caller must skip
    /// the aggregate update (already counted).
    fn insert_value(
        &mut self,
        value: &Value,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<bool, ExecError> {
        if let Some(key) = int_distinct_key(value) {
            return self.insert_int(key, memory, collation);
        }
        if let Self::Ints(_) = self {
            self.migrate_to_values(memory, collation)?;
        }
        let Self::Values(set) = self else {
            unreachable!()
        };
        let key = normalized_hash_key(value.clone(), collation).unwrap_or(Value::Null);
        reserve_hash_set_entries(
            set,
            1,
            size_of::<Value>().saturating_add(HASH_ENTRY_OVERHEAD),
            0,
            memory,
        )?;
        if set.contains(&key) {
            return Ok(false);
        }
        memory.reserve(key.heap_bytes())?;
        set.insert(key);
        Ok(true)
    }

    fn insert_int(
        &mut self,
        key: i128,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<bool, ExecError> {
        match self {
            Self::Ints(set) => {
                reserve_hash_set_entries(
                    set,
                    1,
                    size_of::<i128>().saturating_add(HASH_ENTRY_OVERHEAD),
                    0,
                    memory,
                )?;
                Ok(set.insert(key))
            }
            Self::Values(_) => self.insert_value(&int_key_value(key), memory, collation),
        }
    }

    fn migrate_to_values(
        &mut self,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<(), ExecError> {
        if let Self::Ints(ints) = self {
            let ints = std::mem::take(ints);
            let mut set = HashSet::with_capacity(ints.len());
            memory.reserve(
                ints.len()
                    .saturating_mul(size_of::<Value>().saturating_add(HASH_ENTRY_OVERHEAD)),
            )?;
            for key in ints {
                if let Some(key) = normalized_hash_key(int_key_value(key), collation) {
                    set.insert(key);
                }
            }
            *self = Self::Values(set);
        }
        Ok(())
    }

    fn drain_values(self) -> Vec<Value> {
        match self {
            Self::Ints(set) => set.into_iter().map(int_key_value).collect(),
            Self::Values(set) => set.into_iter().collect(),
        }
    }
}

#[derive(Clone)]
pub(super) struct AggregateState {
    value: AggregateValue,
    seen: Option<DistinctSeen>,
    /// Copied from the aggregate this state accumulates, so the distinct set
    /// and the extreme comparison use the collation the plan resolved.
    collation: Collation,
    /// f64 of the current Minimum/Maximum extreme when known (typed path).
    /// Guides comparisons: strict f64 inequality between correctly-rounded
    /// values transfers to the exact ordering (rounding is monotone), so only
    /// f64 ties pay the full text/value comparison. Invalidated on merge.
    extreme_number: Option<f64>,
    /// Scaled integer units of the current extreme when every update so far
    /// arrived through `update_extreme_units` (same column, same scale, so
    /// unit ordering IS the value ordering). Invalidated on merge.
    extreme_units: Option<i128>,
}

#[derive(Clone)]
enum AggregateValue {
    Count(u64),
    Sum(Option<Value>),
    /// Exact decimal SUM carried as scaled integer units — accumulating
    /// i128 units replaces a parse-add-format round trip per row on the
    /// canonical text carrier (2026-08-02 phase-0 profile residue).
    DecimalSum {
        units: i128,
        scale: u8,
        /// Emit Float64 at finish (the bound aggregate type): the exact
        /// total converts with ONE correct rounding, unlike per-row f64
        /// accumulation (the Q4 canonical mismatch, 2026-08-02).
        float_output: bool,
    },
    Average {
        sum: f64,
        count: u64,
    },
    /// Exact `MySQL` AVG over exact-numeric inputs: the running total is
    /// carried as integer units already widened to the RESULT scale
    /// (input scale + `div_precision_increment`), so `finish` is a single
    /// half-away-from-zero division by the row count.
    DecimalAverage {
        units: i128,
        scale: u8,
        count: u64,
    },
    Minimum(Option<Value>),
    Maximum(Option<Value>),
    /// `ANY_VALUE`: the first non-NULL value the group sees. `MySQL` does not
    /// define which row wins; clients emit this only for columns they know
    /// are functionally dependent on the grouping key.
    AnyValue(Option<Value>),
    /// Welford moments for `STDDEV`/`VARIANCE`. Summing squares and
    /// subtracting the mean at the end is one operation cheaper per row and
    /// catastrophically worse: on values far from zero the subtraction
    /// cancels away most of the significand. Welford costs an extra subtract
    /// and keeps full precision, which matters when the result lands in a
    /// dashboard beside the same column read from `MySQL`.
    Moments {
        count: u64,
        mean: f64,
        m2: f64,
        sample: bool,
        stddev: bool,
    },
    /// `JSON_OBJECTAGG`: each row contributes a one-member object, merged
    /// left to right. A repeated key keeps the last value, as `MySQL` does.
    JsonObjectAgg {
        members: serde_json::Map<String, serde_json::Value>,
    },
    /// `BIT_AND`/`BIT_OR`/`BIT_XOR`. `seen` exists because `MySQL` folds an
    /// empty group to the operation's identity rather than to NULL, so
    /// `BIT_AND` over no rows is all ones.
    BitFold {
        accumulator: u64,
        seen: bool,
    },
    GroupConcat {
        /// Collected `(order keys, rendered value)` rows.
        items: Vec<(Vec<Value>, String)>,
        /// Join separator resolved at state creation.
        separator: String,
        /// Per-key `(ascending, decimal)` sort spec.
        order: Vec<(bool, bool)>,
    },
    JsonArrayAgg {
        /// Pre-rendered JSON fragments in input order (NULLs included).
        items: Vec<String>,
    },
}

impl AggregateState {
    pub(super) fn new(aggregate: &CompiledAggregate) -> Self {
        let value = match aggregate.function {
            AggregateFunction::Count => AggregateValue::Count(0),
            AggregateFunction::Sum => AggregateValue::Sum(None),
            AggregateFunction::Average => match decimal_average_scale(aggregate) {
                Some(scale) => AggregateValue::DecimalAverage {
                    units: 0,
                    scale,
                    count: 0,
                },
                None => AggregateValue::Average { sum: 0.0, count: 0 },
            },
            AggregateFunction::Minimum => AggregateValue::Minimum(None),
            AggregateFunction::Maximum => AggregateValue::Maximum(None),
            AggregateFunction::GroupConcat => AggregateValue::GroupConcat {
                items: Vec::new(),
                separator: aggregate.separator.clone(),
                order: aggregate
                    .order_within
                    .iter()
                    .map(|(_, ascending, decimal)| (*ascending, *decimal))
                    .collect(),
            },
            AggregateFunction::JsonArrayAgg => AggregateValue::JsonArrayAgg { items: Vec::new() },
            AggregateFunction::JsonObjectAgg => AggregateValue::JsonObjectAgg {
                members: serde_json::Map::new(),
            },
            AggregateFunction::AnyValue => AggregateValue::AnyValue(None),
            AggregateFunction::StdDev { sample } => AggregateValue::Moments {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                sample,
                stddev: true,
            },
            AggregateFunction::Variance { sample } => AggregateValue::Moments {
                count: 0,
                mean: 0.0,
                m2: 0.0,
                sample,
                stddev: false,
            },
            AggregateFunction::BitAnd => AggregateValue::BitFold {
                accumulator: u64::MAX,
                seen: false,
            },
            AggregateFunction::BitOr | AggregateFunction::BitXor => AggregateValue::BitFold {
                accumulator: 0,
                seen: false,
            },
        };
        Self {
            collation: aggregate.collation,
            value,
            seen: aggregate
                .distinct
                .then(|| DistinctSeen::Ints(HashSet::default())),
            extreme_number: None,
            extreme_units: None,
        }
    }

    pub(super) fn update(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        self.update_with_number(aggregate, value, None, memory)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn update_with_number(
        &mut self,
        aggregate: &CompiledAggregate,
        value: &Value,
        number: Option<f64>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        // JSON_ARRAYAGG collects NULLs as JSON nulls, so it intercepts
        // ahead of the NULL skip every other aggregate relies on.
        // Like JSON_ARRAYAGG, this runs ahead of the NULL skip: MySQL keeps a
        // NULL value in the object rather than dropping the member.
        if let AggregateValue::JsonObjectAgg { members } = &mut self.value {
            // The bound expression already rendered JSON_OBJECT(k, v), so
            // this text IS the object. Running it back through
            // json_value_of would wrap it as a JSON *string*, the object
            // parse would fail, and every group answered {}.
            let fragment = match value {
                Value::Utf8(text) => text.clone(),
                other => {
                    crate::expression::mysql_json_text(&crate::expression::json_value_of(other))
                }
            };
            if let Ok(serde_json::Value::Object(pair)) =
                serde_json::from_str::<serde_json::Value>(&fragment)
            {
                for (key, entry) in pair {
                    memory.reserve(key.len().saturating_add(16))?;
                    members.insert(key, entry);
                }
            }
            return Ok(());
        }
        if let AggregateValue::JsonArrayAgg { items } = &mut self.value {
            let fragment = crate::expression::json_scalar_text(
                &crate::expression::json_value_of_typed(value, aggregate.input_type)?,
            );
            reserve_vec_elements(items, 1, 64, memory)?;
            memory.reserve(fragment.len())?;
            items.push(fragment);
            return Ok(());
        }
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen
            && !seen.insert_value(value, memory, self.collation)?
        {
            return Ok(());
        }
        // Decimal-typed SUM accumulates scaled units exactly: morph into the
        // unit state on the first value instead of parsing and reformatting
        // canonical text per row.
        if aggregate.function == AggregateFunction::Sum
            && let Some(DataType::Decimal { scale, .. }) = aggregate.data_type
        {
            let units = match value {
                Value::Utf8(text) => crate::batch::parse_decimal_scaled(text, scale),
                Value::Boolean(flag) => decimal_units_from_int(i128::from(*flag), scale),
                Value::Int64(signed) => decimal_units_from_int(i128::from(*signed), scale),
                Value::UInt64(unsigned) => decimal_units_from_int(i128::from(*unsigned), scale),
                _ => None,
            }
            .ok_or(ExecError::NumericOverflow)?;
            return self.update_decimal_sum_units(units, scale, false);
        }
        match &mut self.value {
            // Handled by the early return above, before the NULL skip.
            AggregateValue::JsonObjectAgg { .. } => {}
            AggregateValue::Count(count) => {
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::AnyValue(slot) => {
                if slot.is_none() {
                    replace_retained_value(slot, value.clone(), memory)?;
                }
            }
            AggregateValue::Moments {
                count, mean, m2, ..
            } => {
                let observation = number.map_or_else(|| mysql_f64(value), Ok)?;
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
                #[allow(clippy::cast_precision_loss)]
                let n = *count as f64;
                let delta = observation - *mean;
                *mean += delta / n;
                *m2 = delta.mul_add(observation - *mean, *m2);
                // Values near the f64 ceiling drive delta to infinity and m2
                // to -infinity, which would surface as a negative variance
                // or a NaN standard deviation. Average and Sum already
                // reject non-finite intermediates; so does this.
                if !mean.is_finite() || !m2.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
            }
            AggregateValue::BitFold { accumulator, seen } => {
                // MySQL coerces the argument to BIGINT UNSIGNED before
                // folding, so a signed or textual input reinterprets rather
                // than erroring.
                let bits = mysql_u64(value).unwrap_or_else(|_| {
                    mysql_i64(value).map_or(0, |signed| u64::from_ne_bytes(signed.to_ne_bytes()))
                });
                *accumulator = match aggregate.function {
                    AggregateFunction::BitAnd => *accumulator & bits,
                    AggregateFunction::BitXor => *accumulator ^ bits,
                    _ => *accumulator | bits,
                };
                *seen = true;
            }
            AggregateValue::DecimalSum { units, scale, .. } => {
                let text = match value {
                    Value::Utf8(text) => text.as_str(),
                    _ => {
                        return Err(ExecError::InvalidPhysicalPlan(
                            "decimal sum updated with a non-text value",
                        ));
                    }
                };
                let scaled = crate::batch::parse_decimal_scaled(text, *scale)
                    .ok_or(ExecError::NumericOverflow)?;
                *units = units
                    .checked_add(scaled)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::Sum(sum) => {
                *sum = Some(if let Some(number) = number {
                    let result = sum.take().map_or(Ok(0.0), |value| mysql_f64(&value))? + number;
                    if !result.is_finite() {
                        return Err(ExecError::NumericOverflow);
                    }
                    Value::float64(result)
                } else {
                    add_aggregate_value(sum.take(), value, aggregate.data_type)?
                });
            }
            AggregateValue::Average { sum, count } => {
                *sum += number.map_or_else(|| mysql_f64(value), Ok)?;
                if !sum.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::DecimalAverage {
                units,
                scale,
                count,
            } => {
                // Typed lanes deliver the row through `number` with a
                // sentinel value; everything else arrives as the real Value.
                let scaled = if let Some(number) = number {
                    exact_decimal_units_from_f64(number, *scale)
                } else {
                    match value {
                        Value::Utf8(text) => crate::batch::parse_decimal_scaled(text, *scale),
                        Value::Boolean(flag) => decimal_units_from_int(i128::from(*flag), *scale),
                        Value::Int64(signed) => decimal_units_from_int(i128::from(*signed), *scale),
                        Value::UInt64(unsigned) => {
                            decimal_units_from_int(i128::from(*unsigned), *scale)
                        }
                        _ => None,
                    }
                };
                let scaled = scaled.ok_or(ExecError::NumericOverflow)?;
                *units = units
                    .checked_add(scaled)
                    .ok_or(ExecError::NumericOverflow)?;
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
            }
            AggregateValue::Minimum(minimum) => {
                let replace = match minimum.as_ref() {
                    Some(current) => match (number, self.extreme_number) {
                        (Some(candidate), Some(extreme)) if candidate < extreme => true,
                        (Some(candidate), Some(extreme)) if candidate > extreme => false,
                        _ => {
                            compare_aggregate_values(
                                value,
                                current,
                                aggregate.data_type,
                                self.collation,
                            )? == Ordering::Less
                        }
                    },
                    None => true,
                };
                if replace {
                    replace_retained_value(minimum, value.clone(), memory)?;
                    self.extreme_number = number;
                }
            }
            AggregateValue::Maximum(maximum) => {
                let replace = match maximum.as_ref() {
                    Some(current) => match (number, self.extreme_number) {
                        (Some(candidate), Some(extreme)) if candidate > extreme => true,
                        (Some(candidate), Some(extreme)) if candidate < extreme => false,
                        _ => {
                            compare_aggregate_values(
                                value,
                                current,
                                aggregate.data_type,
                                self.collation,
                            )? == Ordering::Greater
                        }
                    },
                    None => true,
                };
                if replace {
                    replace_retained_value(maximum, value.clone(), memory)?;
                    self.extreme_number = number;
                }
            }
            AggregateValue::GroupConcat { items, .. } => {
                let value_bytes = scalar_string_memory_upper_bound(value);
                reserve_vec_elements(items, 1, 64, memory)?;
                memory.reserve(value_bytes)?;
                let value = aggregate_string(value)?;
                items.push((Vec::new(), value));
            }
            // Handled by the intercept above the NULL skip.
            AggregateValue::JsonArrayAgg { .. } => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "json-array-agg update bypassed its intercept",
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn merge(
        &mut self,
        aggregate: &CompiledAggregate,
        mut other: Self,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        // Merging may replace the extreme through the Value path; the cached
        // f64 guide is conservative-invalidated rather than tracked.
        self.extreme_number = None;
        if aggregate.distinct {
            if let Some(seen) = other.seen.take() {
                for value in seen.drain_values() {
                    self.update(aggregate, &value, memory)?;
                }
            }
            return Ok(());
        }
        match (&mut self.value, other.value) {
            (AggregateValue::Count(left), AggregateValue::Count(right)) => {
                *left = left.checked_add(right).ok_or(ExecError::NumericOverflow)?;
            }
            (AggregateValue::Sum(left), AggregateValue::Sum(Some(right))) => {
                *left = Some(add_aggregate_value(
                    left.take(),
                    &right,
                    aggregate.data_type,
                )?);
            }
            (
                AggregateValue::Sum(_) | AggregateValue::DecimalSum { .. },
                AggregateValue::Sum(None),
            )
            | (AggregateValue::Minimum(_), AggregateValue::Minimum(None))
            | (AggregateValue::Maximum(_), AggregateValue::Maximum(None)) => {}
            (
                AggregateValue::DecimalSum { units: left, .. },
                AggregateValue::DecimalSum { units: right, .. },
            ) => {
                *left = left.checked_add(right).ok_or(ExecError::NumericOverflow)?;
            }
            (
                value @ AggregateValue::Sum(None),
                AggregateValue::DecimalSum {
                    units,
                    scale,
                    float_output,
                },
            ) => {
                *value = AggregateValue::DecimalSum {
                    units,
                    scale,
                    float_output,
                };
            }
            (AggregateValue::DecimalSum { units, scale, .. }, AggregateValue::Sum(Some(right))) => {
                let scaled = crate::batch::parse_decimal_scaled(
                    match &right {
                        Value::Utf8(text) => text,
                        _ => {
                            return Err(ExecError::InvalidPhysicalPlan(
                                "decimal sum merged with a non-text sum",
                            ));
                        }
                    },
                    *scale,
                )
                .ok_or(ExecError::NumericOverflow)?;
                *units = units
                    .checked_add(scaled)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            (
                AggregateValue::DecimalAverage {
                    units: left_units,
                    scale: left_scale,
                    count: left_count,
                },
                AggregateValue::DecimalAverage {
                    units: right_units,
                    scale: right_scale,
                    count: right_count,
                },
            ) => {
                if left_scale != &right_scale {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "decimal average merged across scales",
                    ));
                }
                *left_units = left_units
                    .checked_add(right_units)
                    .ok_or(ExecError::NumericOverflow)?;
                *left_count = left_count
                    .checked_add(right_count)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            (
                AggregateValue::Average {
                    sum: left_sum,
                    count: left_count,
                },
                AggregateValue::Average {
                    sum: right_sum,
                    count: right_count,
                },
            ) => {
                *left_sum += right_sum;
                if !left_sum.is_finite() {
                    return Err(ExecError::NumericOverflow);
                }
                *left_count = left_count
                    .checked_add(right_count)
                    .ok_or(ExecError::NumericOverflow)?;
            }
            (AggregateValue::Minimum(left), AggregateValue::Minimum(Some(right))) => {
                let replace = match left.as_ref() {
                    Some(current) => {
                        compare_aggregate_values(
                            &right,
                            current,
                            aggregate.data_type,
                            self.collation,
                        )? == Ordering::Less
                    }
                    None => true,
                };
                if replace {
                    replace_retained_value(left, right, memory)?;
                }
            }
            (AggregateValue::Maximum(left), AggregateValue::Maximum(Some(right))) => {
                let replace = match left.as_ref() {
                    Some(current) => {
                        compare_aggregate_values(
                            &right,
                            current,
                            aggregate.data_type,
                            self.collation,
                        )? == Ordering::Greater
                    }
                    None => true,
                };
                if replace {
                    replace_retained_value(left, right, memory)?;
                }
            }
            (AggregateValue::AnyValue(left), AggregateValue::AnyValue(right)) => {
                // Any one value satisfies ANY_VALUE, so an existing one wins
                // and an empty side takes the other's.
                if left.is_none()
                    && let Some(value) = right
                {
                    replace_retained_value(left, value, memory)?;
                }
            }
            (
                AggregateValue::BitFold { accumulator, seen },
                AggregateValue::BitFold {
                    accumulator: other,
                    seen: other_seen,
                },
            ) => {
                // The identity of an unseen side would corrupt the fold —
                // all-ones for BIT_AND — so an empty side is skipped rather
                // than combined.
                if other_seen {
                    if *seen {
                        *accumulator = match aggregate.function {
                            AggregateFunction::BitAnd => *accumulator & other,
                            AggregateFunction::BitXor => *accumulator ^ other,
                            _ => *accumulator | other,
                        };
                    } else {
                        *accumulator = other;
                        *seen = true;
                    }
                }
            }
            (
                AggregateValue::Moments {
                    count, mean, m2, ..
                },
                AggregateValue::Moments {
                    count: other_count,
                    mean: other_mean,
                    m2: other_m2,
                    ..
                },
            ) => {
                // Chan's parallel form. Adding m2 directly would be wrong:
                // the combined spread includes the distance between the two
                // means, which neither partial state carries.
                if other_count > 0 {
                    let total = count
                        .checked_add(other_count)
                        .ok_or(ExecError::NumericOverflow)?;
                    #[allow(clippy::cast_precision_loss)]
                    let (left_n, right_n, total_n) =
                        (*count as f64, other_count as f64, total as f64);
                    let delta = other_mean - *mean;
                    *mean = (left_n * *mean + right_n * other_mean) / total_n;
                    *m2 += other_m2 + delta * delta * left_n * right_n / total_n;
                    *count = total;
                    if !mean.is_finite() || !m2.is_finite() {
                        return Err(ExecError::NumericOverflow);
                    }
                }
            }
            _ => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "aggregate states have incompatible merge shapes",
                ));
            }
        }
        Ok(())
    }

    /// Exact decimal SUM on scaled integer units: no text parse, no text
    /// format until `finish`. The state lazily morphs from `Sum(None)` on
    /// the first unit-borne update.
    pub(super) fn update_decimal_sum_units(
        &mut self,
        units: i128,
        scale: u8,
        float_output: bool,
    ) -> Result<(), ExecError> {
        match &mut self.value {
            AggregateValue::DecimalSum {
                units: total,
                scale: existing,
                ..
            } if *existing == scale => {
                *total = total.checked_add(units).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            value @ AggregateValue::Sum(None) => {
                *value = AggregateValue::DecimalSum {
                    units,
                    scale,
                    float_output,
                };
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "decimal unit sum applied to an incompatible aggregate state",
            )),
        }
    }

    /// `GROUP_CONCAT` update carrying the aggregate-local ORDER BY keys
    /// evaluated for this row.
    pub(super) fn update_group_concat(
        &mut self,
        value: &Value,
        keys: Vec<Value>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if matches!(value, Value::Null) {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen
            && !seen.insert_value(value, memory, self.collation)?
        {
            return Ok(());
        }
        let AggregateValue::GroupConcat { items, .. } = &mut self.value else {
            return Err(ExecError::InvalidPhysicalPlan(
                "group-concat update applied to an incompatible aggregate state",
            ));
        };
        let key_bytes = keys.iter().map(Value::heap_bytes).sum::<usize>();
        reserve_vec_elements(items, 1, 64, memory)?;
        memory.reserve(scalar_string_memory_upper_bound(value).saturating_add(key_bytes))?;
        items.push((keys, aggregate_string(value)?));
        Ok(())
    }

    /// Exact decimal AVG on scaled integer units already widened to the
    /// result scale.
    pub(super) fn update_decimal_average_units(
        &mut self,
        units: i128,
        scale: u8,
    ) -> Result<(), ExecError> {
        match &mut self.value {
            AggregateValue::DecimalAverage {
                units: total,
                scale: existing,
                count,
            } if *existing == scale => {
                *total = total.checked_add(units).ok_or(ExecError::NumericOverflow)?;
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "decimal unit average applied to an incompatible aggregate state",
            )),
        }
    }

    /// COUNT(DISTINCT) on a raw integer key: dedup in the i128 set and
    /// bump the count only for new keys — no Value cell is built.
    pub(super) fn update_distinct_count_int(
        &mut self,
        key: i128,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let collation = self.collation;
        let Some(seen) = &mut self.seen else {
            return Err(ExecError::InvalidPhysicalPlan(
                "distinct update on a non-distinct aggregate state",
            ));
        };
        if !seen.insert_int(key, memory, collation)? {
            return Ok(());
        }
        match &mut self.value {
            AggregateValue::Count(count) => {
                *count = count.checked_add(1).ok_or(ExecError::NumericOverflow)?;
                Ok(())
            }
            _ => Err(ExecError::InvalidPhysicalPlan(
                "distinct int count applied to a non-count aggregate",
            )),
        }
    }

    /// MIN/MAX on packed units: comparisons run on the integer units and
    /// the canonical text is formatted only when the extreme is replaced.
    /// A retained extreme without known units (state arrived via merge or a
    /// mixed path) pays one text comparison and then re-anchors the units.
    pub(super) fn update_extreme_units(
        &mut self,
        aggregate: &CompiledAggregate,
        units: i128,
        format: impl Fn() -> Option<String>,
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        let keep_less = match self.value {
            AggregateValue::Minimum(_) => true,
            AggregateValue::Maximum(_) => false,
            _ => {
                return Err(ExecError::InvalidPhysicalPlan(
                    "unit extreme applied to a non-extreme aggregate state",
                ));
            }
        };
        let current_retained = match &self.value {
            AggregateValue::Minimum(slot) | AggregateValue::Maximum(slot) => slot.as_ref(),
            _ => unreachable!(),
        };
        let (replace, preformatted) = match (self.extreme_units, current_retained) {
            (_, None) => (true, None),
            (Some(current), Some(_)) => (
                if keep_less {
                    units < current
                } else {
                    units > current
                },
                None,
            ),
            (None, Some(current)) => {
                let candidate = Value::Utf8(format().ok_or(ExecError::NumericOverflow)?);
                let ordering = compare_aggregate_values(
                    &candidate,
                    current,
                    aggregate.data_type,
                    self.collation,
                )?;
                let replace = if keep_less {
                    ordering == Ordering::Less
                } else {
                    ordering == Ordering::Greater
                };
                (replace, Some(candidate))
            }
        };
        if replace {
            let value = match preformatted {
                Some(value) => value,
                None => Value::Utf8(format().ok_or(ExecError::NumericOverflow)?),
            };
            let (AggregateValue::Minimum(slot) | AggregateValue::Maximum(slot)) = &mut self.value
            else {
                unreachable!()
            };
            replace_retained_value(slot, value, memory)?;
            self.extreme_units = Some(units);
        }
        Ok(())
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub(super) fn finish(self, memory: &MemoryTracker) -> Result<Value, ExecError> {
        Ok(match self.value {
            AggregateValue::Count(count) => Value::UInt64(count),
            AggregateValue::JsonObjectAgg { members } => Value::Utf8(
                crate::expression::mysql_json_text(&serde_json::Value::Object(members)),
            ),
            AggregateValue::BitFold { accumulator, .. } => Value::UInt64(accumulator),
            AggregateValue::Moments {
                count,
                m2,
                sample,
                stddev,
                ..
            } => {
                // MySQL returns NULL for an empty or all-NULL group, and for
                // the sample forms at n = 1 the divisor is zero, which is
                // also NULL rather than an error. The population forms report
                // 0 for a single row.
                let divisor = if sample {
                    count.saturating_sub(1)
                } else {
                    count
                };
                if count == 0 || divisor == 0 {
                    Value::Null
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    let variance = m2 / divisor as f64;
                    Value::float64(if stddev { variance.sqrt() } else { variance })
                }
            }
            AggregateValue::DecimalSum {
                units,
                scale,
                float_output,
            } => {
                if float_output {
                    #[allow(clippy::cast_precision_loss)]
                    Value::float64(units as f64 / 10_f64.powi(i32::from(scale)))
                } else {
                    Value::Utf8(pintail_types::format_decimal_scaled(units, scale))
                }
            }
            AggregateValue::Sum(value)
            | AggregateValue::Minimum(value)
            | AggregateValue::Maximum(value)
            | AggregateValue::AnyValue(value) => value.unwrap_or(Value::Null),
            AggregateValue::Average { sum: _, count: 0 }
            | AggregateValue::DecimalAverage { count: 0, .. } => Value::Null,
            AggregateValue::Average { sum, count } => Value::float64(sum / count as f64),
            AggregateValue::DecimalAverage {
                units,
                scale,
                count,
            } => {
                let average = pintail_types::div_decimal_round_half_up(units, i128::from(count))
                    .ok_or(ExecError::NumericOverflow)?;
                Value::Utf8(pintail_types::format_decimal_scaled(average, scale))
            }
            AggregateValue::JsonArrayAgg { items } if items.is_empty() => Value::Null,
            AggregateValue::JsonArrayAgg { items } => {
                let joined_bytes = items.iter().map(String::len).fold(
                    items.len().saturating_mul(2).saturating_add(2),
                    usize::saturating_add,
                );
                memory.reserve(joined_bytes)?;
                Value::Utf8(format!("[{}]", items.join(", ")))
            }
            AggregateValue::GroupConcat { items, .. } if items.is_empty() => Value::Null,
            AggregateValue::GroupConcat {
                mut items,
                separator,
                order,
            } => {
                if !order.is_empty() {
                    items.sort_by(|left, right| {
                        for (position, (ascending, decimal)) in order.iter().enumerate() {
                            let ordering = compare_sort_values(
                                left.0.get(position).unwrap_or(&Value::Null),
                                right.0.get(position).unwrap_or(&Value::Null),
                                BoundOrderKey {
                                    index: 0,
                                    ascending: *ascending,
                                    // MySQL sorts NULL keys first ascending.
                                    nulls_first: *ascending,
                                    decimal: *decimal,
                                    collation: None,
                                },
                                self.collation,
                            );
                            if ordering != Ordering::Equal {
                                return ordering;
                            }
                        }
                        Ordering::Equal
                    });
                }
                let joined_bytes = items.iter().map(|(_, text)| text.len()).fold(
                    items
                        .len()
                        .saturating_sub(1)
                        .saturating_mul(separator.len()),
                    usize::saturating_add,
                );
                memory.reserve(joined_bytes)?;
                let mut joined = items
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join(&separator);
                // MySQL truncates at the session's byte ceiling and raises
                // warning 1260 for each group that was cut.
                let limit = SESSION_GROUP_CONCAT_MAX_LEN.get();
                if joined.len() > limit {
                    let mut cut = limit;
                    while !joined.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    joined.truncate(cut);
                    SESSION_GROUP_CONCAT_WARNINGS
                        .set(SESSION_GROUP_CONCAT_WARNINGS.get().saturating_add(1));
                }
                Value::Utf8(joined)
            }
        })
    }
}

fn replace_retained_value(
    current: &mut Option<Value>,
    replacement: Value,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let current_bytes = current.as_ref().map_or(0, Value::heap_bytes);
    let replacement_bytes = replacement.heap_bytes();
    if replacement_bytes > current_bytes {
        memory.reserve(replacement_bytes - current_bytes)?;
    } else {
        memory.release(current_bytes - replacement_bytes);
    }
    *current = Some(replacement);
    Ok(())
}

#[allow(clippy::too_many_lines)]
/// Settled aggregate memo (e18's product lever, exactness-first form):
/// bare full-table aggregates over a settled snapshot are pure functions
/// of `(table, manifest generation, plan signature)`. The memo stores the
/// engine's own exact result and is unreachable the moment any ingest
/// makes the snapshot unsettled, so served rows are provably fresh —
/// unlike TTL query caches. Persistent per-block SMAs remain follow-up.
type SettledMemoKey = (std::path::PathBuf, u64, String);
type SettledMemo = std::sync::Mutex<HashMap<SettledMemoKey, Vec<Vec<Value>>>>;
static SETTLED_AGGREGATE_MEMO: std::sync::LazyLock<SettledMemo> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

const SETTLED_MEMO_MAX_ENTRIES: usize = 32;
const SETTLED_MEMO_MAX_ROWS: usize = 1 << 17;

/// Deterministic plan signature, or `None` when any expression could
/// evaluate differently on identical data (volatile functions).
fn settled_signature(
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
) -> Option<String> {
    use std::fmt::Write;
    let mut signature = String::new();
    for expr in group_by {
        write!(signature, "g:{};", expr.deterministic_signature()?).ok()?;
    }
    for aggregate in aggregates {
        let expr = match &aggregate.expr {
            Some(expr) => expr.deterministic_signature()?,
            None => "*".to_owned(),
        };
        write!(
            signature,
            "a:{:?}:{}:{};",
            aggregate.function, aggregate.distinct, expr
        )
        .ok()?;
    }
    Some(signature)
}

#[allow(clippy::too_many_lines)]
/// Merges finished aggregate values of a memoized result with a freshly
/// aggregated insert-only delta, group by group. Only called for shapes
/// whose finished values merge exactly (COUNT/int-float SUM/MIN/MAX).
fn merge_finished_aggregate_rows(
    mut base: Vec<Vec<Value>>,
    delta: Vec<Vec<Value>>,
    group_len: usize,
    aggregates: &[CompiledAggregate],
    collation: Collation,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut index = HashMap::<Vec<Value>, usize>::new();
    for (position, row) in base.iter().enumerate() {
        let key = row[..group_len]
            .iter()
            .cloned()
            .map(|value| normalized_collation_value(value, collation))
            .collect::<Vec<_>>();
        index.insert(key, position);
    }
    for row in delta {
        let key = row[..group_len]
            .iter()
            .cloned()
            .map(|value| normalized_collation_value(value, collation))
            .collect::<Vec<_>>();
        if let Some(position) = index.get(&key) {
            for (offset, aggregate) in aggregates.iter().enumerate() {
                let column = group_len + offset;
                let current = std::mem::replace(&mut base[*position][column], Value::Null);
                base[*position][column] = merge_finished_value(aggregate, current, &row[column])?;
            }
        } else {
            index.insert(key, base.len());
            base.push(row);
        }
    }
    Ok(base)
}

fn merge_finished_value(
    aggregate: &CompiledAggregate,
    current: Value,
    delta: &Value,
) -> Result<Value, ExecError> {
    if matches!(delta, Value::Null) {
        return Ok(current);
    }
    if matches!(current, Value::Null) {
        return Ok(delta.clone());
    }
    match aggregate.function {
        // ANY_VALUE is satisfied by whichever side already has a value, and
        // the bit folds are associative, so both merge exactly.
        AggregateFunction::AnyValue => Ok(current),
        AggregateFunction::BitAnd | AggregateFunction::BitOr | AggregateFunction::BitXor => {
            let left = mysql_u64(&current).unwrap_or(0);
            let right = mysql_u64(delta).unwrap_or(0);
            Ok(Value::UInt64(match aggregate.function {
                AggregateFunction::BitAnd => left & right,
                AggregateFunction::BitXor => left ^ right,
                _ => left | right,
            }))
        }
        // Two finished standard deviations cannot be combined — the moments
        // they came from are gone. The eligibility gate keeps these off this
        // path; this arm exists so a future gate change fails loudly rather
        // than inventing a number.
        AggregateFunction::StdDev { .. } | AggregateFunction::Variance { .. } => Err(
            ExecError::InvalidPhysicalPlan("STDDEV/VARIANCE finished values cannot be merged"),
        ),
        // Merging rendered objects would need key-level precedence the
        // finished text no longer carries; the eligibility gate keeps this
        // unreachable.
        AggregateFunction::JsonObjectAgg => Err(ExecError::InvalidPhysicalPlan(
            "JSON_OBJECTAGG finished values cannot be merged",
        )),
        AggregateFunction::Count => {
            add_aggregate_value(Some(current), delta, Some(DataType::UInt64))
        }
        AggregateFunction::Sum => add_aggregate_value(Some(current), delta, aggregate.data_type),
        AggregateFunction::Minimum => Ok(
            if compare_aggregate_values(delta, &current, aggregate.data_type, aggregate.collation)?
                == Ordering::Less
            {
                delta.clone()
            } else {
                current
            },
        ),
        AggregateFunction::Maximum => Ok(
            if compare_aggregate_values(delta, &current, aggregate.data_type, aggregate.collation)?
                == Ordering::Greater
            {
                delta.clone()
            } else {
                current
            },
        ),
        AggregateFunction::Average
        | AggregateFunction::GroupConcat
        | AggregateFunction::JsonArrayAgg => Err(ExecError::InvalidPhysicalPlan(
            "unmergeable aggregate reached the delta merge",
        )),
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn build_hash_aggregate(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
    key_collations: &[Collation],
) -> Result<MaterializedRows, ExecError> {
    // Storage predicates are ALSO compiled into Filter operators above the
    // scan (belt and braces), so a filtered plan is Filter(..(Scan)). Those
    // filters are exactly scan.predicates, which the scan signature already
    // covers — walking through them keeps the key sound.
    fn settled_scan(operator: &PullOperator) -> Option<&PullOperator> {
        match operator {
            PullOperator::Scan { .. } => Some(operator),
            PullOperator::Filter { input, .. } => settled_scan(input),
            _ => None,
        }
    }
    /// Data-version identity of a whole settled plan: scans directly,
    /// filters transparently (their predicates ARE the scan signature),
    /// and fresh inner joins when BOTH sides are settled — either table's
    /// ingest or flush changes its component of the key.
    ///
    /// No collation here: a scan signature already fixes the columns, and the
    /// collation is a property of those columns, so two scans that share a
    /// signature necessarily share a collation.
    fn settled_plan_key(operator: &PullOperator) -> Option<(std::path::PathBuf, u64, String)> {
        match operator {
            PullOperator::Scan { stream, .. } => stream.settled_identity(),
            PullOperator::Filter { input, .. } => settled_plan_key(input),
            PullOperator::HashJoin {
                left,
                right,
                kind,
                left_key,
                right_key,
                key_mode,
                right_width,
                state,
                ..
            } if state.is_none() => {
                let (left_dir, left_gen, left_sig) = settled_plan_key(left)?;
                let (right_dir, right_gen, right_sig) = settled_plan_key(right)?;
                Some((
                    left_dir,
                    left_gen,
                    format!(
                        "J{kind:?}|{key_mode:?}|{}|{}|{right_width}|L({left_sig})|R({}:{right_gen}:{right_sig})",
                        left_key.deterministic_signature()?,
                        right_key.deterministic_signature()?,
                        right_dir.display(),
                    ),
                ))
            }
            _ => None,
        }
    }
    // Profiling escape hatch: with the memo on, every settled re-run is a
    // replay and a sampling profiler only ever sees the first execution.
    let memo_key = if std::env::var_os("PINTAIL_DISABLE_SETTLED_MEMO").is_some() {
        None
    } else {
        settled_plan_key(input).and_then(|(directory, generation, scan)| {
            settled_signature(group_by, aggregates)
                .map(|signature| (directory, generation, format!("p{scan:?};{signature}")))
        })
    };
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        eprintln!(
            "[agg] memo key: {:?} (scan found: {})",
            memo_key.as_ref().map(|(_, generation, signature)| (
                generation,
                signature.chars().take(60).collect::<String>()
            )),
            settled_scan(input).is_some()
        );
    }
    if let Some(key) = &memo_key
        && let Some(rows) = SETTLED_AGGREGATE_MEMO
            .lock()
            .expect("settled memo lock")
            .get(key)
            .cloned()
    {
        let payload: usize = rows
            .iter()
            .map(|row| estimated_row_payload_bytes(row))
            .sum();
        memory.reserve(payload)?;
        return Ok(MaterializedRows { rows, position: 0 });
    }
    if memo_key.is_none()
        && let Some(PullOperator::Scan { stream, .. }) = settled_scan(input)
        && let Some(delta) = stream.insert_only_delta()
        && let Some(signature) = settled_signature(group_by, aggregates)
        && aggregates.iter().all(|aggregate| {
            !aggregate.distinct
                && match aggregate.function {
                    // COUNT/MIN/MAX, plus the associative folds, all merge
                    // exactly over the disjoint rows an insert-only delta
                    // contributes.
                    AggregateFunction::Count
                    | AggregateFunction::Minimum
                    | AggregateFunction::Maximum
                    | AggregateFunction::AnyValue
                    | AggregateFunction::BitAnd
                    | AggregateFunction::BitOr
                    | AggregateFunction::BitXor => true,
                    AggregateFunction::Sum => matches!(
                        aggregate.data_type,
                        Some(DataType::Int64 | DataType::UInt64 | DataType::Float64)
                    ),
                    AggregateFunction::Average
                    | AggregateFunction::GroupConcat
                    | AggregateFunction::JsonArrayAgg
                    | AggregateFunction::JsonObjectAgg
                    // Needs the moments, not the finished value.
                    | AggregateFunction::StdDev { .. }
                    | AggregateFunction::Variance { .. } => false,
                }
        })
    {
        let key = (
            delta.directory.clone(),
            delta.generation,
            format!("{};{signature}", delta.scan),
        );
        let base = SETTLED_AGGREGATE_MEMO
            .lock()
            .expect("settled memo lock")
            .get(&key)
            .cloned();
        if let Some(base) = base {
            let row_count = delta.rows.len();
            let columns = (0..delta.types.len())
                .map(|column| {
                    ColumnVector::new(
                        delta.types[column],
                        delta.rows.iter().map(|row| row[column].clone()).collect(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ExecError::InvalidBatch("delta rows do not match the scan types"))?;
            let batch = RecordBatch::new(row_count, columns)
                .map_err(|_| ExecError::InvalidBatch("delta rows do not form a batch"))?;
            let mut one_shot = PullOperator::Scan {
                stream: Box::new(OneShotStream { batch: Some(batch) }),
                expected_types: delta.types.clone(),
            };
            let delta_rows = build_hash_aggregate_scan(
                &mut one_shot,
                group_by,
                aggregates,
                memory,
                collation,
                key_collations,
            )?;
            let merged = merge_finished_aggregate_rows(
                base,
                delta_rows.rows,
                group_by.len(),
                aggregates,
                collation,
            )?;
            let payload: usize = merged
                .iter()
                .map(|row| estimated_row_payload_bytes(row))
                .sum();
            memory.reserve(payload)?;
            return Ok(MaterializedRows {
                rows: merged,
                position: 0,
            });
        }
    }
    if group_by.is_empty()
        && !aggregates.is_empty()
        && let Some(rows) = try_sma_fold(input, aggregates, memory)?
    {
        if let Some(key) = &memo_key {
            let mut memo = SETTLED_AGGREGATE_MEMO.lock().expect("settled memo lock");
            if memo.len() >= SETTLED_MEMO_MAX_ENTRIES {
                memo.clear();
            }
            memo.insert(key.clone(), rows.clone());
        }
        return Ok(MaterializedRows { rows, position: 0 });
    }
    let result = build_hash_aggregate_scan(
        input,
        group_by,
        aggregates,
        memory,
        collation,
        key_collations,
    )?;
    if let Some(key) = memo_key
        && result.rows.len() <= SETTLED_MEMO_MAX_ROWS
    {
        let mut memo = SETTLED_AGGREGATE_MEMO.lock().expect("settled memo lock");
        if memo.len() >= SETTLED_MEMO_MAX_ENTRIES {
            memo.clear();
        }
        memo.insert(key, result.rows.clone());
    }
    Ok(result)
}

// Successful SMA folds on this thread: proof of engagement for tests and
// `PINTAIL_AGG_DEBUG` diagnostics. Thread-local because the fold runs
// synchronously on the plan-building thread and the counter's only
// consumers are same-thread test assertions — a process-wide counter made
// those assertions race with folds from concurrently running tests.
thread_local! {
    static SMA_FOLD_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[allow(dead_code)] // test-and-diagnostics accessor; production reads go through PINTAIL_AGG_DEBUG
pub(crate) fn sma_fold_hits() -> u64 {
    SMA_FOLD_HITS.with(std::cell::Cell::get)
}

/// Folds per-segment SMAs into finished bare-aggregate states and merges
/// the residual memtable rows through the normal update path, so the whole
/// table never rescans while it ingests (WS3-B). Returns `None` whenever
/// any aggregate, column, or segment falls outside the provably-exact
/// envelope; the caller then runs the ordinary scan.
#[allow(clippy::too_many_lines)]
fn try_sma_fold(
    input: &mut PullOperator,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<Option<Vec<Vec<Value>>>, ExecError> {
    // Only a direct bare Scan qualifies: any Filter above it implies
    // residual predicates, and the stream then carries no SMA input.
    let PullOperator::Scan { stream, .. } = &*input else {
        return Ok(None);
    };
    let Some(sma) = stream.sma_fold_input() else {
        return Ok(None);
    };
    let live_rows: u64 = sma.segments.iter().map(|segment| segment.live_rows).sum();
    let mut states = Vec::with_capacity(aggregates.len());
    let mut columns = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        if aggregate.distinct {
            return Ok(None);
        }
        let column = match &aggregate.expr {
            None => {
                if aggregate.function != AggregateFunction::Count {
                    return Ok(None);
                }
                None
            }
            Some(expr) => match expr.column_index() {
                Some(index) => Some(index),
                None => return Ok(None),
            },
        };
        // Every segment must carry an SMA entry for the queried column;
        // a schema-evolved segment written before the column existed
        // declines the fold rather than guessing.
        let entries = match column {
            None => Vec::new(),
            Some(index) => {
                let Some(id) = sma.column_ids.get(index).copied() else {
                    return Ok(None);
                };
                let mut entries = Vec::with_capacity(sma.segments.len());
                for segment in &sma.segments {
                    let Some(entry) = segment.columns.iter().find(|entry| entry.column_id == id)
                    else {
                        return Ok(None);
                    };
                    entries.push(entry);
                }
                entries
            }
        };
        let mut state = AggregateState::new(aggregate);
        let synthetic = match aggregate.function {
            // Segment summaries carry count/sum/min/max only, so none of
            // these can be folded without reading rows. Declining the whole
            // optimization is the point: returning None here would only mean
            // "no synthetic value", and the fold would go on to answer from
            // an empty state — NULL for a variance, all-ones for BIT_AND.
            AggregateFunction::AnyValue
            | AggregateFunction::StdDev { .. }
            | AggregateFunction::Variance { .. }
            | AggregateFunction::BitAnd
            | AggregateFunction::BitOr
            | AggregateFunction::BitXor
            | AggregateFunction::JsonObjectAgg => return Ok(None),
            AggregateFunction::Count => {
                let total = match column {
                    None => live_rows,
                    Some(_) => entries.iter().map(|entry| entry.non_null).sum(),
                };
                Some(AggregateValue::Count(total))
            }
            AggregateFunction::Sum | AggregateFunction::Average => {
                let mut total: Option<pintail_store::SmaSum> = None;
                let mut count = 0_u64;
                let mut foldable = true;
                for entry in &entries {
                    if entry.non_null == 0 {
                        continue;
                    }
                    count += entry.non_null;
                    let Some(sum) = entry.sum else {
                        foldable = false;
                        break;
                    };
                    total = Some(match (total, sum) {
                        (None, sum) => sum,
                        (
                            Some(pintail_store::SmaSum::Int(left)),
                            pintail_store::SmaSum::Int(right),
                        ) => pintail_store::SmaSum::Int(
                            left.checked_add(right).ok_or(ExecError::NumericOverflow)?,
                        ),
                        (
                            Some(pintail_store::SmaSum::Float(left)),
                            pintail_store::SmaSum::Float(right),
                        ) => pintail_store::SmaSum::Float(left + right),
                        (
                            Some(pintail_store::SmaSum::DecimalUnits { units, scale }),
                            pintail_store::SmaSum::DecimalUnits {
                                units: right,
                                scale: right_scale,
                            },
                        ) if scale == right_scale => pintail_store::SmaSum::DecimalUnits {
                            units: units.checked_add(right).ok_or(ExecError::NumericOverflow)?,
                            scale,
                        },
                        _ => {
                            foldable = false;
                            break;
                        }
                    });
                }
                if !foldable {
                    return Ok(None);
                }
                match (aggregate.function, total) {
                    (_, None) => None,
                    (AggregateFunction::Sum, Some(total)) => match total {
                        pintail_store::SmaSum::Int(total) => {
                            let value = match aggregate.data_type.map(DataType::storage_type) {
                                Some(DataType::UInt64) => {
                                    Value::UInt64(match u64::try_from(total) {
                                        Ok(total) => total,
                                        Err(_) => return Ok(None),
                                    })
                                }
                                Some(DataType::Int64) => Value::Int64(match i64::try_from(total) {
                                    Ok(total) => total,
                                    Err(_) => return Ok(None),
                                }),
                                _ => return Ok(None),
                            };
                            Some(AggregateValue::Sum(Some(value)))
                        }
                        pintail_store::SmaSum::Float(total) => {
                            if !total.is_finite() {
                                return Ok(None);
                            }
                            Some(AggregateValue::Sum(Some(Value::float64(total))))
                        }
                        pintail_store::SmaSum::DecimalUnits { units, scale } => {
                            Some(AggregateValue::DecimalSum {
                                units,
                                scale,
                                float_output: aggregate_uses_float(aggregate),
                            })
                        }
                    },
                    (AggregateFunction::Average, Some(total)) => {
                        if let Some(result_scale) = decimal_average_scale(aggregate) {
                            // Exact decimal AVG: rescale the fold's exact
                            // totals to the widened result scale; decline
                            // the fold rather than round through f64.
                            let units = match total {
                                pintail_store::SmaSum::Int(total) => {
                                    decimal_units_from_int(total, result_scale)
                                }
                                pintail_store::SmaSum::DecimalUnits { units, scale }
                                    if scale <= result_scale =>
                                {
                                    decimal_units_from_int(units, result_scale - scale)
                                }
                                _ => None,
                            };
                            let Some(units) = units else {
                                return Ok(None);
                            };
                            Some(AggregateValue::DecimalAverage {
                                units,
                                scale: result_scale,
                                count,
                            })
                        } else {
                            #[allow(clippy::cast_precision_loss)]
                            let sum = match total {
                                pintail_store::SmaSum::Int(total) => total as f64,
                                pintail_store::SmaSum::Float(total) => total,
                                pintail_store::SmaSum::DecimalUnits { units, scale } => {
                                    units as f64 / 10_f64.powi(i32::from(scale))
                                }
                            };
                            if !sum.is_finite() {
                                return Ok(None);
                            }
                            Some(AggregateValue::Average { sum, count })
                        }
                    }
                    _ => unreachable!("outer match covers Sum and Average"),
                }
            }
            AggregateFunction::Minimum | AggregateFunction::Maximum => {
                let mut folded: Option<pintail_store::SmaExtremes> = None;
                for entry in &entries {
                    if entry.non_null == 0 {
                        continue;
                    }
                    let Some(extremes) = entry.extremes else {
                        return Ok(None);
                    };
                    folded = Some(match (folded, extremes) {
                        (None, extremes) => extremes,
                        (
                            Some(pintail_store::SmaExtremes::Int { min, max }),
                            pintail_store::SmaExtremes::Int {
                                min: right_min,
                                max: right_max,
                            },
                        ) => pintail_store::SmaExtremes::Int {
                            min: min.min(right_min),
                            max: max.max(right_max),
                        },
                        (
                            Some(pintail_store::SmaExtremes::UInt { min, max }),
                            pintail_store::SmaExtremes::UInt {
                                min: right_min,
                                max: right_max,
                            },
                        ) => pintail_store::SmaExtremes::UInt {
                            min: min.min(right_min),
                            max: max.max(right_max),
                        },
                        (
                            Some(pintail_store::SmaExtremes::Float { min, max }),
                            pintail_store::SmaExtremes::Float {
                                min: right_min,
                                max: right_max,
                            },
                        ) => pintail_store::SmaExtremes::Float {
                            min: min.min(right_min),
                            max: max.max(right_max),
                        },
                        (
                            Some(pintail_store::SmaExtremes::DecimalUnits { min, max, scale }),
                            pintail_store::SmaExtremes::DecimalUnits {
                                min: right_min,
                                max: right_max,
                                scale: right_scale,
                            },
                        ) if scale == right_scale => pintail_store::SmaExtremes::DecimalUnits {
                            min: min.min(right_min),
                            max: max.max(right_max),
                            scale,
                        },
                        (
                            Some(pintail_store::SmaExtremes::Temporal { min, max, units }),
                            pintail_store::SmaExtremes::Temporal {
                                min: right_min,
                                max: right_max,
                                units: right_units,
                            },
                        ) if units == right_units => pintail_store::SmaExtremes::Temporal {
                            min: min.min(right_min),
                            max: max.max(right_max),
                            units,
                        },
                        _ => return Ok(None),
                    });
                }
                match folded {
                    None => None,
                    Some(extremes) => {
                        let minimum = aggregate.function == AggregateFunction::Minimum;
                        let value = match extremes {
                            pintail_store::SmaExtremes::Int { min, max } => {
                                Value::Int64(if minimum { min } else { max })
                            }
                            pintail_store::SmaExtremes::UInt { min, max } => {
                                Value::UInt64(if minimum { min } else { max })
                            }
                            pintail_store::SmaExtremes::Float { min, max } => {
                                Value::float64(if minimum { min } else { max })
                            }
                            pintail_store::SmaExtremes::DecimalUnits { min, max, scale } => {
                                Value::Utf8(pintail_types::format_decimal_scaled(
                                    if minimum { min } else { max },
                                    scale,
                                ))
                            }
                            pintail_store::SmaExtremes::Temporal { min, max, units } => {
                                match units.format(if minimum { min } else { max }) {
                                    Some(text) => Value::Utf8(text),
                                    None => return Ok(None),
                                }
                            }
                        };
                        Some(if minimum {
                            AggregateValue::Minimum(Some(value))
                        } else {
                            AggregateValue::Maximum(Some(value))
                        })
                    }
                }
            }
            AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg => {
                return Ok(None);
            }
        };
        if let Some(value) = synthetic {
            state.merge(
                aggregate,
                AggregateState {
                    value,
                    seen: None,
                    collation: aggregate.collation,
                    extreme_number: None,
                    extreme_units: None,
                },
                memory,
            )?;
        }
        states.push(state);
        columns.push(column);
    }
    for row in &sma.rows {
        for ((state, aggregate), column) in states.iter_mut().zip(aggregates).zip(&columns) {
            match column {
                None => state.update(aggregate, &Value::UInt64(1), memory)?,
                Some(index) => {
                    let Some(value) = row.get(*index) else {
                        return Err(ExecError::InvalidBatch(
                            "SMA residual row does not match the scan projection",
                        ));
                    };
                    state.update(aggregate, value, memory)?;
                }
            }
        }
    }
    let row = states
        .into_iter()
        .map(|state| state.finish(memory))
        .collect::<Result<Vec<_>, _>>()?;
    memory.reserve(estimated_row_payload_bytes(&row))?;
    SMA_FOLD_HITS.with(|hits| hits.set(hits.get() + 1));
    if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
        eprintln!(
            "[agg] SMA fold: {} segments, {} residual rows",
            sma.segments.len(),
            sma.rows.len()
        );
    }
    Ok(Some(vec![row]))
}

/// Batches gathered before a parallel round runs.
///
/// This is the parallel WIDTH of aggregation: the round is pulled serially,
/// handed to `par_iter`, and merged serially, so a round of eight can occupy
/// at most eight threads however many the machine has. Measured 1->16 threads
/// on sixteen cores, a join-free group-by peaked at 3.12x and went flat after
/// eight threads, which is the shape of exactly this cap.
///
/// Sized from the pool so the round can fill the machine. The per-round memory
/// ceiling below still bounds it, so a wide machine with a tight budget cuts
/// the round short rather than overcommitting.
fn aggregate_round_batches() -> usize {
    rayon::current_num_threads().clamp(8, 64)
}

#[allow(clippy::too_many_lines)]
fn build_hash_aggregate_scan(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
    key_collations: &[Collation],
) -> Result<MaterializedRows, ExecError> {
    if group_by.is_empty()
        && !aggregates.is_empty()
        && aggregates.iter().all(|aggregate| {
            aggregate.function == AggregateFunction::Count
                && aggregate.expr.is_none()
                && !aggregate.distinct
        })
    {
        let mut count = 0_u64;
        while let Some(batch) = input.next_batch(memory)? {
            count = count
                .checked_add(
                    u64::try_from(batch.visible_row_count())
                        .map_err(|_| ExecError::NumericOverflow)?,
                )
                .ok_or(ExecError::NumericOverflow)?;
        }
        let row = vec![Value::UInt64(count); aggregates.len()];
        memory.reserve(estimated_row_payload_bytes(&row))?;
        return Ok(MaterializedRows {
            rows: vec![row],
            position: 0,
        });
    }
    if !group_by.is_empty() {
        let direct_columns = group_by
            .iter()
            .map(CompiledExpr::column_index)
            .collect::<Option<Vec<_>>>();
        // The fused join path keys ONE shared table for the whole key
        // tuple, so it stays on uniform-collation keys; mixed keys take the
        // general per-key path below.
        if key_collations.windows(2).all(|pair| pair[0] == pair[1])
            && let Some(group_columns) = direct_columns.as_deref()
            && let Some(rows) = build_fused_inner_join_aggregate(
                input,
                group_columns,
                aggregates,
                memory,
                collation,
                // Uniform by the gate above; the KEY's collation is what the
                // group fold must use - the plan collation folded a
                // general_ci key under 0900 rules and split its PAD-equal
                // spellings into separate groups (#258).
                key_collations.first().copied().unwrap_or(collation),
            )?
        {
            return Ok(rows);
        }
        if aggregates.iter().all(|aggregate| {
            !matches!(
                aggregate.function,
                AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg
            )
        }) {
            return build_buffered_hash_aggregate(
                input,
                group_by,
                direct_columns.as_deref(),
                aggregates,
                memory,
                collation,
                key_collations,
            );
        }
        if let Some(group_columns) = direct_columns {
            return build_direct_column_aggregate(
                input,
                None,
                &group_columns,
                aggregates,
                memory,
                collation,
                key_collations,
            );
        }
    }

    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    if group_by.is_empty() {
        reserve_hash_map_entries(
            &mut groups,
            1,
            size_of::<Vec<Value>>()
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            0,
            memory,
        )?;
        memory.reserve(aggregates.len().saturating_mul(size_of::<AggregateState>()))?;
        groups.insert(
            Vec::new(),
            AggregateGroup {
                values: Vec::new(),
                states: aggregates.iter().map(AggregateState::new).collect(),
            },
        );
    }

    while let Some(batch) = input.next_batch(memory)? {
        let batch_bytes = batch.estimated_bytes();
        reserve_hash_map_entries(
            &mut groups,
            batch.visible_row_count().min(64),
            size_of::<Vec<Value>>()
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            batch_bytes,
            memory,
        )?;
        for row in batch.selection().selected_rows() {
            let group_expression_memory = group_by
                .iter()
                .map(|expression| expression.allocation_upper_bound(&batch, row))
                .fold(0_usize, usize::saturating_add);
            let group_memory = group_expression_memory
                .saturating_mul(13)
                .saturating_add(
                    group_by
                        .len()
                        .saturating_mul(size_of::<Value>())
                        .saturating_mul(2),
                )
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
                .saturating_add(HASH_ENTRY_OVERHEAD);
            memory.ensure_transient(batch_bytes.saturating_add(group_memory))?;
            let values = group_by
                .iter()
                .map(|expression| expression.evaluate(&batch, row))
                .collect::<Result<Vec<_>, _>>()?;
            let key = values
                .iter()
                .cloned()
                .zip(key_collations)
                .map(|(value, collation)| {
                    normalized_hash_key(value, *collation).unwrap_or(Value::Null)
                })
                .collect::<Vec<_>>();
            if groups.len() == groups.capacity() {
                let growth = groups.capacity().max(1);
                reserve_hash_map_entries(
                    &mut groups,
                    growth,
                    size_of::<Vec<Value>>()
                        .saturating_add(size_of::<AggregateGroup>())
                        .saturating_add(HASH_ENTRY_OVERHEAD),
                    batch_bytes,
                    memory,
                )?;
            }
            let group = match groups.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let bytes = estimated_row_payload_bytes(&values)
                        .saturating_add(estimated_row_payload_bytes(entry.key()))
                        .saturating_add(
                            aggregates.len().saturating_mul(size_of::<AggregateState>()),
                        );
                    memory.ensure_transient(batch_bytes.saturating_add(bytes))?;
                    memory.reserve(bytes)?;
                    entry.insert(AggregateGroup {
                        values,
                        states: aggregates.iter().map(AggregateState::new).collect(),
                    })
                }
            };
            update_aggregate_states(
                &batch,
                row,
                batch_bytes,
                aggregates,
                &mut group.states,
                memory,
            )?;
        }
    }

    memory.reserve(groups.len().saturating_mul(size_of::<Vec<Value>>()))?;
    let mut rows = Vec::with_capacity(groups.len());
    for (_, group) in groups {
        let mut row = group.values;
        reserve_vec_elements(&mut row, group.states.len(), 0, memory)?;
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

#[allow(clippy::too_many_lines)]
/// One or two `DATEPART(column)` group expressions over packed temporal
/// columns, when every part is bounded enough for 20-bit packing.
fn date_part_key_source(
    group_by: &[CompiledExpr],
    batch: &RecordBatch,
) -> Option<TwoPassKeySource> {
    let part_of = |expr: &CompiledExpr| -> Option<(DatePart, usize)> {
        let CompiledExpr::Scalar {
            function: ScalarFunction::DatePart(part),
            args,
            ..
        } = expr
        else {
            return None;
        };
        let [CompiledExpr::Column(column)] = args.as_slice() else {
            return None;
        };
        let vector = batch.column(*column)?;
        if !matches!(
            vector.data_type(),
            DataType::Date32 | DataType::DateTime64 { .. }
        ) {
            return None;
        }
        let (typed, _) = vector.typed()?;
        matches!(typed, crate::batch::TypedValues::Temporal { .. }).then_some((*part, *column))
    };
    match group_by {
        [only] => Some(TwoPassKeySource::DateParts {
            parts: [Some(part_of(only)?), None],
        }),
        [first, second] => Some(TwoPassKeySource::DateParts {
            parts: [Some(part_of(first)?), Some(part_of(second)?)],
        }),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn build_buffered_hash_aggregate(
    input: &mut PullOperator,
    group_by: &[CompiledExpr],
    direct_columns: Option<&[usize]>,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
    key_collations: &[Collation],
) -> Result<MaterializedRows, ExecError> {
    let Some(first_batch) = input.next_batch(memory)? else {
        return Ok(MaterializedRows {
            rows: Vec::new(),
            position: 0,
        });
    };
    // GROUP BY over date-part expressions (the Q5 shape): bounded int
    // domains ride the streaming two-pass without Value keys.
    if direct_columns.is_none()
        && let Some(keys) = date_part_key_source(group_by, &first_batch)
        && let Some(lanes) = two_pass_lanes(aggregates, &first_batch)
    {
        return build_streaming_two_pass_aggregate(
            input,
            first_batch,
            keys,
            &lanes,
            aggregates,
            memory,
            collation,
        );
    }
    let utf8_column = |column: &usize| {
        first_batch
            .column(*column)
            .is_some_and(|values| values.data_type() == DataType::Utf8)
    };
    let direct_eligible = match direct_columns {
        Some([column]) => first_batch.column(*column).is_some_and(|values| {
            matches!(
                values.data_type().storage_type(),
                DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
            ) || (values.data_type() == DataType::Utf8
                && two_pass_lanes(aggregates, &first_batch).is_some())
        }),
        Some([first, second]) => {
            utf8_column(first)
                && utf8_column(second)
                && key_collations.windows(2).all(|pair| pair[0] == pair[1])
                && two_pass_lanes(aggregates, &first_batch).is_some()
        }
        _ => false,
    };
    if direct_eligible {
        // Single int-typed group columns take the sequential direct path:
        // its scalar index avoids the per-row Vec<Value> keys and the
        // per-round global merges that the buffered parallel path pays.
        // Routing them through the parallel path regressed Q6 (2M groups
        // over 20M rows) from seconds to minutes — e02's parallel win used
        // dense arrays at low cardinality and does not transfer to sparse
        // high-cardinality keys. Parallel high-cardinality aggregation
        // needs a partitioned design and its own experiment first.
        return build_direct_column_aggregate(
            input,
            Some(first_batch),
            direct_columns.expect("matched direct columns"),
            aggregates,
            memory,
            collation,
            key_collations,
        );
    }

    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    // Bytes reserved for the live group map (entries plus state growth),
    // measured through used() snapshots around the sequential merge section
    // so state-internal reserves (distinct sets) are included.
    let mut groups_reserved = 0_usize;
    let mut spill_runs: Vec<AggregateSpillRun> = Vec::new();
    let mut first_batch = Some(first_batch);
    let per_row_upper = group_by
        .len()
        .saturating_mul(size_of::<Value>())
        .saturating_mul(2)
        .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
        .saturating_add(size_of::<AggregateGroup>())
        .saturating_add(HASH_ENTRY_OVERHEAD)
        .saturating_add(256);
    loop {
        let round = aggregate_round_batches();
        let mut batches = Vec::with_capacity(round);
        let mut batch_reserved = 0_usize;
        let mut selected_rows = 0_usize;
        while batches.len() < round {
            let batch = if let Some(batch) = first_batch.take() {
                Some(batch)
            } else {
                input.next_batch(memory)?
            };
            let Some(batch) = batch else {
                break;
            };
            let bytes = batch.estimated_bytes();
            reserve_or_spill_groups(
                bytes,
                &mut groups,
                &mut groups_reserved,
                &mut spill_runs,
                memory,
            )?;
            batch_reserved = batch_reserved.saturating_add(bytes);
            selected_rows = selected_rows.saturating_add(batch.visible_row_count());
            batches.push(batch);
            // Tight ceilings cap the round instead of reserving a
            // conservative upper bound larger than the whole budget; the
            // default ceiling keeps full 8-batch rounds.
            if selected_rows.saturating_mul(per_row_upper) > memory.limit() / 4 {
                break;
            }
        }
        if batches.is_empty() {
            break;
        }
        let local_upper = selected_rows.saturating_mul(per_row_upper);
        reserve_or_spill_groups(
            local_upper,
            &mut groups,
            &mut groups_reserved,
            &mut spill_runs,
            memory,
        )?;
        let mut used_before_merge = memory.used();
        let partials = batches
            .par_iter()
            .map(|batch| {
                direct_columns.map_or_else(
                    || {
                        build_local_expression_groups(
                            batch,
                            group_by,
                            aggregates,
                            memory,
                            key_collations,
                        )
                    },
                    |columns| {
                        build_local_direct_groups(
                            batch,
                            columns,
                            aggregates,
                            memory,
                            key_collations,
                        )
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        for partial in partials {
            for entry in partial {
                let mut pending = Some(entry);
                while let Some((key, partial_group)) = pending.take() {
                    // Spill BEFORE merging when the ceiling is close, not after
                    // a merge has failed. A merge that runs out part-way cannot
                    // be retried: the group has already been partially updated,
                    // so the entry cannot be handed back and the error escapes
                    // instead of spilling. A group's COUNT(DISTINCT) sets grow
                    // through exactly that path, which is why this query failed
                    // where it should have spilled once anything else held a
                    // large share of the budget.
                    if memory.used() > memory.limit().saturating_mul(3) / 4 && !groups.is_empty() {
                        groups_reserved = groups_reserved
                            .saturating_add(memory.used().saturating_sub(used_before_merge));
                        spill_runs.push(write_aggregate_spill_run(&mut groups, memory)?);
                        memory.release(groups_reserved);
                        groups_reserved = 0;
                        used_before_merge = memory.used();
                    }
                    match merge_partial_group(
                        &mut groups,
                        key,
                        partial_group,
                        aggregates,
                        batch_reserved,
                        memory,
                    ) {
                        Ok(()) => {}
                        // The entry was handed back untouched, so spilling
                        // the map here and retrying it is safe.
                        Err((ExecError::MemoryLimitExceeded { .. }, Some(returned)))
                            if !groups.is_empty() =>
                        {
                            groups_reserved = groups_reserved
                                .saturating_add(memory.used().saturating_sub(used_before_merge));
                            spill_runs.push(write_aggregate_spill_run(&mut groups, memory)?);
                            memory.release(groups_reserved);
                            groups_reserved = 0;
                            used_before_merge = memory.used();
                            pending = Some(returned);
                        }
                        Err((error, _)) => return Err(error),
                    }
                }
            }
        }
        groups_reserved =
            groups_reserved.saturating_add(memory.used().saturating_sub(used_before_merge));
        memory.release(local_upper.saturating_add(batch_reserved));
        // Proactive spill at half the ceiling, mirroring the sort spill:
        // upstream scans size their working sets from the remaining
        // headroom, so a group map that hoards the budget until hard
        // failure starves them.
        //
        // Total pressure counts as well as the map's own share. Watching only
        // `groups_reserved` assumes the map is what fills the budget, and it
        // usually is - but a group's COUNT(DISTINCT) sets grow through a path
        // this retry does not cover, so when anything else holds a large part
        // of the ceiling those sets hit it first and the query fails where it
        // should have spilled. Larger batches make that ordinary rather than
        // rare, since a batch in flight is then a real share of the budget.
        let under_pressure = memory.used() > memory.limit().saturating_mul(3) / 4;
        if (groups_reserved > memory.limit() / 2 || under_pressure) && !groups.is_empty() {
            spill_runs.push(write_aggregate_spill_run(&mut groups, memory)?);
            memory.release(groups_reserved);
            groups_reserved = 0;
        }
    }
    if spill_runs.is_empty() {
        return finish_aggregate_groups(groups.into_values(), memory);
    }
    memory.release(groups_reserved);
    merge_spilled_aggregate_groups(spill_runs, groups, aggregates, memory)
}

/// The error and, when the failure struck *before* the entry touched the
/// map, the entry itself so the caller can spill and retry it; `None`
/// means a mid-merge failure that cannot be replayed.
type MergeGroupFailure = (ExecError, Option<(Vec<Value>, AggregateGroup)>);

/// Merges one partial group into the live map. A memory failure *before*
/// the entry touches the map hands the entry back (`Some`) so the caller
/// can spill and retry it; a failure while merging states cannot be
/// replayed and returns `None`.
fn merge_partial_group(
    groups: &mut HashMap<Vec<Value>, AggregateGroup>,
    key: Vec<Value>,
    partial_group: AggregateGroup,
    aggregates: &[CompiledAggregate],
    batch_reserved: usize,
    memory: &MemoryTracker,
) -> Result<(), MergeGroupFailure> {
    if groups.len() == groups.capacity() {
        let growth = groups.capacity().max(64);
        if let Err(error) = reserve_hash_map_entries(
            groups,
            growth,
            size_of::<Vec<Value>>()
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD),
            batch_reserved,
            memory,
        ) {
            return Err((error, Some((key, partial_group))));
        }
    }
    let group = match groups.entry(key) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let bytes = estimated_row_payload_bytes(&partial_group.values)
                .saturating_add(estimated_row_payload_bytes(entry.key()))
                .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()));
            if let Err(error) = memory.reserve(bytes) {
                return Err((error, Some((entry.into_key(), partial_group))));
            }
            entry.insert(AggregateGroup {
                values: partial_group.values,
                states: aggregates.iter().map(AggregateState::new).collect(),
            })
        }
    };
    for ((state, partial_state), aggregate) in group
        .states
        .iter_mut()
        .zip(partial_group.states)
        .zip(aggregates)
    {
        state
            .merge(aggregate, partial_state, memory)
            .map_err(|error| (error, None))?;
    }
    Ok(())
}

/// Reserves `bytes`, spilling the live group map as an on-disk run and
/// retrying once when the first attempt exceeds the memory ceiling. Only
/// reserves made *between* merge sections are safe to retry this way; a
/// failure mid-merge propagates because the interrupted group state cannot
/// be replayed.
fn reserve_or_spill_groups(
    bytes: usize,
    groups: &mut HashMap<Vec<Value>, AggregateGroup>,
    groups_reserved: &mut usize,
    spill_runs: &mut Vec<AggregateSpillRun>,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    match memory.reserve(bytes) {
        Ok(()) => Ok(()),
        Err(ExecError::MemoryLimitExceeded { .. }) if !groups.is_empty() => {
            spill_runs.push(write_aggregate_spill_run(groups, memory)?);
            memory.release(*groups_reserved);
            *groups_reserved = 0;
            memory.reserve(bytes)
        }
        Err(error) => Err(error),
    }
}

/// One spilled aggregation run: entries sorted by their encoded group key,
/// one length-framed record each, streamed back in write order.
struct AggregateSpillRun {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
    _path: tempfile::TempPath,
    _reservation: spill::SpillReservation,
}

struct SpilledGroupEntry {
    /// The group key, encoded once at spill time; the k-way merge orders
    /// and matches entries on these exact bytes.
    key: Vec<u8>,
    values: Vec<Value>,
    states: Vec<SpilledAggregateState>,
}

struct SpilledAggregateState {
    value: SpilledAggregateValue,
    /// Drained DISTINCT keys; revival replays them through the regular
    /// update path, which rebuilds the int-set/value-set split.
    seen: Option<Vec<Value>>,
}

/// Spillable mirror of [`AggregateValue`]. `i128` units travel as decimal
/// strings so the encoding stays independent of integer width.
enum SpilledAggregateValue {
    Count(u64),
    Sum(Option<Value>),
    DecimalSum {
        units: String,
        scale: u8,
        float_output: bool,
    },
    Average {
        sum: f64,
        count: u64,
    },
    DecimalAverage {
        units: String,
        scale: u8,
        count: u64,
    },
    Minimum(Option<Value>),
    Maximum(Option<Value>),
    AnyValue(Option<Value>),
    Moments {
        count: u64,
        mean: f64,
        m2: f64,
        sample: bool,
        stddev: bool,
    },
    BitFold {
        accumulator: u64,
        seen: bool,
    },
}

fn spill_aggregate_state(state: AggregateState) -> Result<SpilledAggregateState, ExecError> {
    let AggregateState { value, seen, .. } = state;
    let value = match value {
        AggregateValue::Count(count) => SpilledAggregateValue::Count(count),
        AggregateValue::Sum(sum) => SpilledAggregateValue::Sum(sum),
        AggregateValue::DecimalSum {
            units,
            scale,
            float_output,
        } => SpilledAggregateValue::DecimalSum {
            units: units.to_string(),
            scale,
            float_output,
        },
        AggregateValue::Average { sum, count } => SpilledAggregateValue::Average { sum, count },
        AggregateValue::DecimalAverage {
            units,
            scale,
            count,
        } => SpilledAggregateValue::DecimalAverage {
            units: units.to_string(),
            scale,
            count,
        },
        AggregateValue::Minimum(value) => SpilledAggregateValue::Minimum(value),
        AggregateValue::Maximum(value) => SpilledAggregateValue::Maximum(value),
        AggregateValue::AnyValue(value) => SpilledAggregateValue::AnyValue(value),
        AggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        } => SpilledAggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        },
        AggregateValue::BitFold { accumulator, seen } => {
            SpilledAggregateValue::BitFold { accumulator, seen }
        }
        AggregateValue::GroupConcat { .. }
        | AggregateValue::JsonArrayAgg { .. }
        | AggregateValue::JsonObjectAgg { .. } => {
            return Err(ExecError::InvalidPhysicalPlan(
                "aggregation spill reached a non-spillable aggregate state",
            ));
        }
    };
    Ok(SpilledAggregateState {
        value,
        seen: seen.map(DistinctSeen::drain_values),
    })
}

fn spilled_units(units: &str) -> Result<i128, ExecError> {
    units
        .parse::<i128>()
        .map_err(|_| ExecError::Source("aggregate spill decode: bad decimal units".to_owned()))
}

/// Rebuilds a live aggregate state from its spilled form. DISTINCT states
/// replay their seen keys through the regular update path; everything else
/// restores the accumulator directly (the extreme caches stay invalidated,
/// exactly as after a merge).
fn revive_aggregate_state(
    spilled: SpilledAggregateState,
    aggregate: &CompiledAggregate,
    memory: &MemoryTracker,
) -> Result<AggregateState, ExecError> {
    let mut state = AggregateState::new(aggregate);
    if let Some(values) = spilled.seen {
        for value in values {
            state.update(aggregate, &value, memory)?;
        }
        return Ok(state);
    }
    state.value = match spilled.value {
        SpilledAggregateValue::Count(count) => AggregateValue::Count(count),
        SpilledAggregateValue::AnyValue(value) => AggregateValue::AnyValue(value),
        SpilledAggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        } => AggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        },
        SpilledAggregateValue::BitFold { accumulator, seen } => {
            AggregateValue::BitFold { accumulator, seen }
        }
        SpilledAggregateValue::Sum(sum) => AggregateValue::Sum(sum),
        SpilledAggregateValue::DecimalSum {
            units,
            scale,
            float_output,
        } => AggregateValue::DecimalSum {
            units: spilled_units(&units)?,
            scale,
            float_output,
        },
        SpilledAggregateValue::Average { sum, count } => AggregateValue::Average { sum, count },
        SpilledAggregateValue::DecimalAverage {
            units,
            scale,
            count,
        } => AggregateValue::DecimalAverage {
            units: spilled_units(&units)?,
            scale,
            count,
        },
        SpilledAggregateValue::Minimum(value) => AggregateValue::Minimum(value),
        SpilledAggregateValue::Maximum(value) => AggregateValue::Maximum(value),
    };
    Ok(state)
}

/// Drains the live group map into one sorted on-disk run.
fn write_aggregate_spill_run(
    groups: &mut HashMap<Vec<Value>, AggregateGroup>,
    memory: &MemoryTracker,
) -> Result<AggregateSpillRun, ExecError> {
    let mut entries = groups
        .drain()
        .map(|(key, group)| {
            let mut encoder = spill::Encoder::with_capacity(32);
            encoder.values(&key);
            Ok(SpilledGroupEntry {
                key: encoder.finish(),
                values: group.values,
                states: group
                    .states
                    .into_iter()
                    .map(spill_aggregate_state)
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<Vec<_>, ExecError>>()?;
    entries.sort_unstable_by(|left, right| left.key.cmp(&right.key));
    let file = spill::spill_file("pintail-aggregate-spill-", memory.spill())
        .map_err(|error| ExecError::Source(format!("aggregate spill create: {error}")))?;
    let (file, path, mut reservation) = file.into_parts();
    let mut writer = std::io::BufWriter::new(file);
    for entry in entries {
        let mut encoder = spill::Encoder::with_capacity(entry.key.len() + 64);
        encoder.bytes(&entry.key);
        encoder.values(&entry.values);
        encoder.count(entry.states.len());
        for state in &entry.states {
            encode_aggregate_state(&mut encoder, state);
        }
        spill::write_record_quota(&mut writer, &encoder.finish(), &mut reservation)
            .map_err(|error| ExecError::Source(format!("aggregate spill write: {error}")))?;
    }
    let mut file = writer
        .into_inner()
        .map_err(|error| ExecError::Source(format!("aggregate spill flush: {error}")))?;
    std::io::Seek::rewind(&mut file)
        .map_err(|error| ExecError::Source(format!("aggregate spill rewind: {error}")))?;
    Ok(AggregateSpillRun {
        reader: std::io::BufReader::new(file),
        payload: Vec::new(),
        _path: path,
        _reservation: reservation,
    })
}

impl AggregateSpillRun {
    fn next_entry(&mut self) -> Result<Option<SpilledGroupEntry>, ExecError> {
        if !spill::read_record(&mut self.reader, &mut self.payload)
            .map_err(|error| ExecError::Source(format!("aggregate spill read: {error}")))?
        {
            return Ok(None);
        }
        let mut decoder = spill::Decoder::new(&self.payload);
        let entry = (|| {
            let key = decoder.bytes()?.to_vec();
            let values = decoder.values()?;
            let count = decoder.count()?;
            let mut states = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                states.push(decode_aggregate_state(&mut decoder)?);
            }
            Ok::<_, String>(SpilledGroupEntry {
                key,
                values,
                states,
            })
        })()
        .map_err(|error| ExecError::Source(format!("aggregate spill decode: {error}")))?;
        Ok(Some(entry))
    }
}

const AGGREGATE_COUNT: u8 = 0;
const AGGREGATE_SUM: u8 = 1;
const AGGREGATE_DECIMAL_SUM: u8 = 2;
const AGGREGATE_AVERAGE: u8 = 3;
const AGGREGATE_DECIMAL_AVERAGE: u8 = 4;
const AGGREGATE_MINIMUM: u8 = 5;
const AGGREGATE_MAXIMUM: u8 = 6;
const AGGREGATE_ANY_VALUE: u8 = 7;
const AGGREGATE_MOMENTS: u8 = 8;
const AGGREGATE_BIT_FOLD: u8 = 9;

fn encode_aggregate_state(encoder: &mut spill::Encoder, state: &SpilledAggregateState) {
    match &state.value {
        SpilledAggregateValue::AnyValue(value) => {
            encoder.u8(AGGREGATE_ANY_VALUE);
            encoder.optional_value(value.as_ref());
        }
        SpilledAggregateValue::Moments {
            count,
            mean,
            m2,
            sample,
            stddev,
        } => {
            encoder.u8(AGGREGATE_MOMENTS);
            encoder.u64(*count);
            encoder.f64(*mean);
            encoder.f64(*m2);
            encoder.bool(*sample);
            encoder.bool(*stddev);
        }
        SpilledAggregateValue::BitFold { accumulator, seen } => {
            encoder.u8(AGGREGATE_BIT_FOLD);
            encoder.u64(*accumulator);
            encoder.bool(*seen);
        }
        SpilledAggregateValue::Count(count) => {
            encoder.u8(AGGREGATE_COUNT);
            encoder.u64(*count);
        }
        SpilledAggregateValue::Sum(sum) => {
            encoder.u8(AGGREGATE_SUM);
            encoder.optional_value(sum.as_ref());
        }
        SpilledAggregateValue::DecimalSum {
            units,
            scale,
            float_output,
        } => {
            encoder.u8(AGGREGATE_DECIMAL_SUM);
            encoder.str(units);
            encoder.u8(*scale);
            encoder.bool(*float_output);
        }
        SpilledAggregateValue::Average { sum, count } => {
            encoder.u8(AGGREGATE_AVERAGE);
            encoder.f64(*sum);
            encoder.u64(*count);
        }
        SpilledAggregateValue::DecimalAverage {
            units,
            scale,
            count,
        } => {
            encoder.u8(AGGREGATE_DECIMAL_AVERAGE);
            encoder.str(units);
            encoder.u8(*scale);
            encoder.u64(*count);
        }
        SpilledAggregateValue::Minimum(value) => {
            encoder.u8(AGGREGATE_MINIMUM);
            encoder.optional_value(value.as_ref());
        }
        SpilledAggregateValue::Maximum(value) => {
            encoder.u8(AGGREGATE_MAXIMUM);
            encoder.optional_value(value.as_ref());
        }
    }
    match &state.seen {
        None => encoder.bool(false),
        Some(seen) => {
            encoder.bool(true);
            encoder.values(seen);
        }
    }
}

fn decode_aggregate_state(
    decoder: &mut spill::Decoder<'_>,
) -> Result<SpilledAggregateState, String> {
    let value = match decoder.u8()? {
        AGGREGATE_COUNT => SpilledAggregateValue::Count(decoder.u64()?),
        AGGREGATE_ANY_VALUE => SpilledAggregateValue::AnyValue(decoder.optional_value()?),
        AGGREGATE_MOMENTS => SpilledAggregateValue::Moments {
            count: decoder.u64()?,
            mean: decoder.f64()?,
            m2: decoder.f64()?,
            sample: decoder.bool()?,
            stddev: decoder.bool()?,
        },
        AGGREGATE_BIT_FOLD => SpilledAggregateValue::BitFold {
            accumulator: decoder.u64()?,
            seen: decoder.bool()?,
        },
        AGGREGATE_SUM => SpilledAggregateValue::Sum(decoder.optional_value()?),
        AGGREGATE_DECIMAL_SUM => SpilledAggregateValue::DecimalSum {
            units: decoder.string()?,
            scale: decoder.u8()?,
            float_output: decoder.bool()?,
        },
        AGGREGATE_AVERAGE => SpilledAggregateValue::Average {
            sum: decoder.f64()?,
            count: decoder.u64()?,
        },
        AGGREGATE_DECIMAL_AVERAGE => SpilledAggregateValue::DecimalAverage {
            units: decoder.string()?,
            scale: decoder.u8()?,
            count: decoder.u64()?,
        },
        AGGREGATE_MINIMUM => SpilledAggregateValue::Minimum(decoder.optional_value()?),
        AGGREGATE_MAXIMUM => SpilledAggregateValue::Maximum(decoder.optional_value()?),
        other => return Err(format!("spilled aggregate holds unknown tag {other}")),
    };
    let seen = if decoder.bool()? {
        Some(decoder.values()?)
    } else {
        None
    };
    Ok(SpilledAggregateState { value, seen })
}

/// K-way merges the spilled runs (plus the resident remainder written as a
/// final run) into finished output rows. Runs are keyed and sorted by the
/// serialized group key, so equal groups are adjacent across run heads and
/// partial states combine through the existing merge path.
fn merge_spilled_aggregate_groups(
    mut runs: Vec<AggregateSpillRun>,
    mut groups: HashMap<Vec<Value>, AggregateGroup>,
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    if !groups.is_empty() {
        runs.push(write_aggregate_spill_run(&mut groups, memory)?);
    }
    let mut heads = Vec::with_capacity(runs.len());
    for run in &mut runs {
        heads.push(run.next_entry()?);
    }
    let mut rows: Vec<Vec<Value>> = Vec::new();
    loop {
        let mut winner: Option<usize> = None;
        for (index, head) in heads.iter().enumerate() {
            let Some(candidate) = head else { continue };
            let better = match winner {
                None => true,
                Some(current) => {
                    let current_head = heads[current]
                        .as_ref()
                        .expect("winner head is always occupied");
                    candidate.key < current_head.key
                }
            };
            if better {
                winner = Some(index);
            }
        }
        let Some(winner) = winner else { break };
        let replacement = runs[winner].next_entry()?;
        let entry =
            std::mem::replace(&mut heads[winner], replacement).expect("winner head is occupied");
        if entry.states.len() != aggregates.len() {
            return Err(ExecError::Source(
                "aggregate spill decode: state arity mismatch".to_owned(),
            ));
        }
        // Revival and finishing reserve transient state (distinct sets,
        // merge growth) that dies with this group; measure and release it,
        // then account the finished row alone.
        let used_before_group = memory.used();
        let mut states = entry
            .states
            .into_iter()
            .zip(aggregates)
            .map(|(state, aggregate)| revive_aggregate_state(state, aggregate, memory))
            .collect::<Result<Vec<_>, _>>()?;
        // Fold every other run's entry for the same key into this group.
        for index in 0..heads.len() {
            while heads[index]
                .as_ref()
                .is_some_and(|head| head.key == entry.key)
            {
                let replacement = runs[index].next_entry()?;
                let duplicate = std::mem::replace(&mut heads[index], replacement)
                    .expect("matching head is occupied");
                if duplicate.states.len() != aggregates.len() {
                    return Err(ExecError::Source(
                        "aggregate spill decode: state arity mismatch".to_owned(),
                    ));
                }
                for ((state, spilled), aggregate) in
                    states.iter_mut().zip(duplicate.states).zip(aggregates)
                {
                    let other = revive_aggregate_state(spilled, aggregate, memory)?;
                    state.merge(aggregate, other, memory)?;
                }
            }
        }
        let mut row = entry.values;
        row.reserve(states.len());
        for state in states {
            row.push(state.finish(memory)?);
        }
        memory.release(memory.used().saturating_sub(used_before_group));
        memory
            .reserve(size_of::<Vec<Value>>().saturating_add(estimated_row_payload_bytes(&row)))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

#[allow(clippy::too_many_lines)]
fn build_fused_inner_join_aggregate(
    input: &mut PullOperator,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
    group_collation: Collation,
) -> Result<Option<MaterializedRows>, ExecError> {
    let PullOperator::HashJoin {
        left,
        right,
        kind,
        left_key,
        right_key,
        extra_keys,
        key_mode,
        column_types,
        right_width,
        state,
        residual,
        residual_columns: _,
        collation: _,
    } = input
    else {
        return Ok(None);
    };
    // The fused spine matches on the hash key alone and never sees candidate
    // pairs, so it cannot apply a residual ON predicate. Declining here sends
    // the join to the general operator, which can - silently ignoring it
    // would return rows the ON clause excludes.
    if residual.is_some() {
        return Ok(None);
    }
    let left_width = column_types.len().saturating_sub(*right_width);
    // The fused spine probes on the primary key alone; composite-key joins
    // stay on the general operator.
    if !extra_keys.is_empty()
        || *kind != BoundJoinKind::Inner
        || state.is_some()
        || *right_width > column_types.len()
        || group_columns
            .iter()
            .any(|column| *column < left_width || *column >= column_types.len())
        || aggregates.iter().any(|aggregate| {
            aggregate.distinct
                || matches!(
                    aggregate.function,
                    AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg
                )
        })
        || aggregates.iter().any(|aggregate| {
            aggregate
                .expr
                .as_ref()
                .is_some_and(|expression| expression.column_index().is_none())
        })
    {
        return Ok(None);
    }

    let right_group_columns = group_columns
        .iter()
        .map(|column| column - left_width)
        .collect::<Vec<_>>();
    let build_clock = std::time::Instant::now();
    let build_start = memory.used();
    let join = build_hash_join_state(right, right_key, *key_mode, extra_keys, memory, collation)?;
    let build_reserved = memory.used().saturating_sub(build_start);
    let build_us = build_clock.elapsed().as_micros();
    let probe_clock = std::time::Instant::now();
    let mut pull_us = 0_u128;
    // A build side that outgrew the ceiling is no longer in `join.build`: it
    // was drained into grace partitions, and only the general operator knows
    // how to serve those. The fused spine reads the resident map directly, so
    // probing it here would match nothing and answer with silence rather than
    // an error. Hand the built state back to the operator - which resumes
    // from exactly this - and let it run.
    //
    // No query is known that reaches this: every fused candidate tried so far
    // resolves its group columns to the probe side and declines above. It is
    // guarded anyway because the failure mode is a wrong answer, not a crash,
    // and the guard costs one branch per query.
    if join.spilled() {
        *state = Some(Box::new(join));
        return Ok(None);
    }
    // Dense direct-address probe (experiments/RESULTS.md e04, 2.4-4.2x):
    // Integer-mode build keys occupying a small dense range trade the
    // per-probe evaluate+hash for one bounds-checked index lookup. MySQL
    // auto-increment keys make this the common case, not the exception.
    let dense: Option<DenseJoinTable<'_>> =
        if matches!(key_mode, JoinKeyMode::Integer) && !join.build.is_empty() {
            let mut min = i128::MAX;
            let mut max = i128::MIN;
            let mut integers = true;
            for key in join.build.keys() {
                match key {
                    JoinHashKey::NegativeInteger(value) => {
                        min = min.min(i128::from(*value));
                        max = max.max(i128::from(*value));
                    }
                    JoinHashKey::NonNegativeInteger(value) => {
                        min = min.min(i128::from(*value));
                        max = max.max(i128::from(*value));
                    }
                    _ => {
                        integers = false;
                        break;
                    }
                }
            }
            if integers && max - min < MAX_DENSE_SPAN {
                let span = usize::try_from(max - min).expect("bounded span") + 1;
                let mut table: Vec<Option<&Vec<Vec<Value>>>> = vec![None; span];
                for (key, bucket) in join.build.iter() {
                    let value = match key {
                        JoinHashKey::NegativeInteger(value) => i128::from(*value),
                        JoinHashKey::NonNegativeInteger(value) => i128::from(*value),
                        _ => unreachable!("verified integer keys"),
                    };
                    table[usize::try_from(value - min).expect("within span")] = Some(bucket);
                }
                Some((min, table))
            } else {
                None
            }
        } else {
            None
        };
    let plan = resolve_join_group_plan(&join.build, &right_group_columns, group_collation)?;
    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    loop {
        let gather_clock = std::time::Instant::now();
        let round = aggregate_round_batches();
        let mut batches = Vec::with_capacity(round);
        let mut batch_reserved = 0_usize;
        while batches.len() < round {
            let Some(batch) = left.next_batch(memory)? else {
                break;
            };
            let bytes = batch.estimated_bytes();
            memory.reserve(bytes)?;
            batch_reserved = batch_reserved.saturating_add(bytes);
            batches.push(batch);
        }
        if batches.is_empty() {
            break;
        }
        pull_us += gather_clock.elapsed().as_micros();
        let selected_rows = batches
            .iter()
            .map(RecordBatch::visible_row_count)
            .sum::<usize>();
        let local_upper = selected_rows.saturating_mul(
            right_group_columns
                .len()
                .saturating_mul(size_of::<Value>())
                .saturating_mul(2)
                .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()))
                .saturating_add(size_of::<AggregateGroup>())
                .saturating_add(HASH_ENTRY_OVERHEAD)
                .saturating_add(256),
        );
        memory.reserve(local_upper)?;
        let partials = batches
            .par_iter()
            .map(|batch| {
                build_local_fused_join_groups(
                    batch,
                    left_key,
                    *key_mode,
                    group_collation,
                    left_width,
                    aggregates,
                    &join.build,
                    dense.as_ref(),
                    &plan,
                    memory,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        for partial in partials {
            for (key, partial_group) in partial {
                if groups.len() == groups.capacity() {
                    let growth = groups.capacity().max(64);
                    reserve_hash_map_entries(
                        &mut groups,
                        growth,
                        size_of::<Vec<Value>>()
                            .saturating_add(size_of::<AggregateGroup>())
                            .saturating_add(HASH_ENTRY_OVERHEAD),
                        batch_reserved,
                        memory,
                    )?;
                }
                let group = match groups.entry(key) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let bytes = estimated_row_payload_bytes(&partial_group.values)
                            .saturating_add(estimated_row_payload_bytes(entry.key()))
                            .saturating_add(
                                aggregates.len().saturating_mul(size_of::<AggregateState>()),
                            );
                        memory.reserve(bytes)?;
                        entry.insert(AggregateGroup {
                            values: partial_group.values,
                            states: aggregates.iter().map(AggregateState::new).collect(),
                        })
                    }
                };
                for ((state, partial_state), aggregate) in group
                    .states
                    .iter_mut()
                    .zip(partial_group.states)
                    .zip(aggregates)
                {
                    state.merge(aggregate, partial_state, memory)?;
                }
            }
        }
        memory.release(local_upper.saturating_add(batch_reserved));
    }

    if std::env::var_os("PINTAIL_PHASE_TIMING").is_some() {
        eprintln!(
            "JOINPHASE build={}us probe={}us of_which_pull={}us",
            build_us,
            probe_clock.elapsed().as_micros(),
            pull_us
        );
    }
    drop(dense);
    drop(join);
    memory.release(build_reserved);
    Ok(Some(finish_aggregate_groups(groups.into_values(), memory)?))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_local_fused_join_groups(
    batch: &RecordBatch,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    group_collation: Collation,
    left_width: usize,
    aggregates: &[CompiledAggregate],
    build: &PartitionedBuild,
    dense: Option<&DenseJoinTable<'_>>,
    plan: &JoinGroupPlan,
    parent_memory: &MemoryTracker,
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    // Groups are fixed by the build side: start with the resolved set and
    // index into it, so the probe loop never hashes or compares group
    // values (the Q8 profile's dominant cost).
    let mut groups = plan
        .values
        .iter()
        .map(|values| AggregateGroup {
            values: values.clone(),
            states: aggregates.iter().map(AggregateState::new).collect(),
        })
        .collect::<Vec<_>>();
    let mut touched = vec![false; groups.len()];
    let memory = parent_memory.unbounded_worker();
    // Probe through the dense table when the left key is a packed integer
    // column; Integer key mode guarantees those physical variants, and NULL
    // rows skip exactly as normalized_join_key's None does.
    let left_typed = dense.and_then(|_| {
        left_key
            .column_index()
            .and_then(|column| batch.column(column))
            .and_then(ColumnVector::typed)
            .filter(|(typed, _)| {
                matches!(
                    typed,
                    crate::batch::TypedValues::Int64(_) | crate::batch::TypedValues::UInt64(_)
                )
            })
    });
    for (offset, row) in batch.selection().selected_rows().enumerate() {
        if offset % 1024 == 0 {
            memory.check_interruption()?;
        }
        let matches = if let (Some((min, table)), Some((typed, validity))) = (dense, left_typed) {
            if !validity.is_valid(row) {
                continue;
            }
            let candidate = match typed {
                crate::batch::TypedValues::Int64(values) => i128::from(values[row]),
                crate::batch::TypedValues::UInt64(values) => i128::from(values[row]),
                _ => unreachable!("filtered to integer projections"),
            };
            let Some(offset) = candidate
                .checked_sub(*min)
                .and_then(|delta| usize::try_from(delta).ok())
            else {
                continue;
            };
            match table.get(offset) {
                Some(Some(bucket)) => *bucket,
                _ => continue,
            }
        } else {
            let Some(key) = normalized_join_key(left_key.evaluate(batch, row)?, key_mode)? else {
                continue;
            };
            let Some(matches) = build.get(&key) else {
                continue;
            };
            matches
        };
        let indexes = plan
            .buckets
            .get(&(std::ptr::from_ref(matches) as usize))
            .ok_or(ExecError::InvalidPhysicalPlan(
                "probe matched a bucket outside the resolved group plan",
            ))?;
        for (right_values, group_index) in matches.iter().zip(indexes) {
            let group_index = *group_index;
            touched[group_index] = true;
            for (aggregate, state) in aggregates.iter().zip(&mut groups[group_index].states) {
                let value = match aggregate.expr.as_ref() {
                    None => &Value::Boolean(true),
                    Some(expression) => {
                        let column =
                            expression
                                .column_index()
                                .ok_or(ExecError::InvalidPhysicalPlan(
                                    "fused join aggregate expression is not a column",
                                ))?;
                        if column < left_width {
                            // Probe-side columns update typed-first: the
                            // Q8 profile showed per-row decimal text
                            // parse/format dominating this loop.
                            if update_state_from_typed_column(
                                state, aggregate, batch, column, row, &memory,
                            )? {
                                continue;
                            }
                            direct_group_value(batch, row, column)?
                        } else {
                            right_values.get(column - left_width).ok_or(
                                ExecError::InvalidPhysicalPlan(
                                    "join aggregate column is outside the joined layout",
                                ),
                            )?
                        }
                    }
                };
                state.update(aggregate, value, &memory)?;
            }
        }
    }
    // Uniform by construction: the fused path is gated to keys that share
    // one collation before it is attempted.
    //
    // Only groups a probe row actually TOUCHED leave this batch: the plan
    // pre-seeds one slot per build-side group so the probe loop can index
    // instead of hash, and emitting the untouched slots invented zero-count
    // groups an INNER join must not have (sakila: every language appeared
    // with COUNT 0 beside English's 1000).
    //
    // Slots whose keys NORMALIZE equal must MERGE, not last-write-win: a
    // plain collect() dropped every earlier slot's states whenever two
    // build spellings folded to one key, silently losing their rows'
    // aggregates (#258's vanished red/RED group).
    let mut folded: HashMap<Vec<Value>, AggregateGroup> = HashMap::with_capacity(groups.len());
    for (group, touched) in groups.into_iter().zip(touched) {
        if !touched {
            continue;
        }
        let key: Vec<Value> = group
            .values
            .iter()
            .cloned()
            .map(|value| normalized_collation_value(value, group_collation))
            .collect();
        match folded.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(group);
            }
            Entry::Occupied(mut entry) => {
                for ((state, partial_state), aggregate) in entry
                    .get_mut()
                    .states
                    .iter_mut()
                    .zip(group.states)
                    .zip(aggregates)
                {
                    state.merge(aggregate, partial_state, &memory)?;
                }
            }
        }
    }
    Ok(folded)
}

/// Dictionary-code aggregation for low-cardinality string group keys
/// (experiments/RESULTS.md e02: array-indexed accumulation, no hash table).
/// Handles one or two Utf8 group columns via base-256 composite codes mapped
/// to dense slots. Local dedup is byte-exact, mirroring the general path —
/// collation unification still happens at the normalized-key merge. Falls
/// back (`None`) whenever the shape doesn't qualify.
#[allow(clippy::too_many_lines)]
fn build_local_dictionary_groups(
    batch: &RecordBatch,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    parent_memory: &MemoryTracker,
    key_collations: &[Collation],
) -> Result<Option<HashMap<Vec<Value>, AggregateGroup>>, ExecError> {
    struct DictAggregate {
        function: AggregateFunction,
        column: Option<usize>,
    }
    const MAX_CODES: usize = 256;
    const MAX_SLOTS: usize = 4096;
    let memory = parent_memory.unbounded_worker();
    if group_columns.is_empty() || group_columns.len() > 2 {
        return Ok(None);
    }
    let mut key_columns = Vec::with_capacity(group_columns.len());
    for column in group_columns {
        let Some(vector) = batch.column(*column) else {
            return Ok(None);
        };
        let Some((crate::batch::TypedValues::Utf8(keys), validity)) = vector.typed() else {
            return Ok(None);
        };
        key_columns.push((vector, keys, validity));
    }
    // Aggregate inputs: plain typed columns (numbers via the packed path) or
    // COUNT(*); anything else falls back to the general builder.
    let mut dict_aggregates = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        if aggregate.distinct {
            return Ok(None);
        }
        match aggregate.function {
            AggregateFunction::Count => {}
            // Float-carried sums and averages only: exact decimal AVG needs
            // the generic i128-unit state, not this lane's f64 slots.
            AggregateFunction::Sum | AggregateFunction::Average
                if aggregate_uses_float(aggregate) => {}
            _ => return Ok(None),
        }
        let column = match &aggregate.expr {
            None => None,
            Some(expression) => match expression.column_index() {
                Some(column) => Some(column),
                None => return Ok(None),
            },
        };
        if let Some(column) = column {
            let typed = batch.column(column).and_then(ColumnVector::typed);
            if aggregate.function != AggregateFunction::Count && typed.is_none() {
                return Ok(None);
            }
            if aggregate.function != AggregateFunction::Count
                && typed.is_some_and(|(values, _)| values.number_at(0).is_none())
                && batch.row_count() > 0
            {
                return Ok(None);
            }
        }
        dict_aggregates.push(DictAggregate {
            function: aggregate.function,
            column,
        });
    }

    // Pass 1: per-column dictionary codes (0 = NULL), composed base-256 and
    // mapped to dense slots through a sentinel table. The representative row
    // of a slot exhibits every key column's original value.
    let mut column_dicts: Vec<Vec<Option<usize>>> =
        key_columns.iter().map(|_| vec![None]).collect();
    let selected = batch.visible_row_count();
    let mut rows_buffer = Vec::with_capacity(selected);
    let mut codes_buffer = Vec::with_capacity(selected);
    let composite_capacity = MAX_CODES.pow(u32::try_from(key_columns.len()).expect("<= 2 columns"));
    let mut slot_table = vec![u16::MAX; composite_capacity];
    let mut slot_rows: Vec<usize> = Vec::new();
    for (offset, row) in batch.selection().selected_rows().enumerate() {
        if offset % 1024 == 0 {
            memory.check_interruption()?;
        }
        let mut composite = 0_usize;
        for ((_, keys, validity), dict) in key_columns.iter().zip(column_dicts.iter_mut()) {
            let views = keys.views();
            let heap = keys.heap();
            let code = if validity.is_valid(row) {
                let view = &views[row];
                let found = dict[1..].iter().position(|representative| {
                    representative.is_some_and(|existing| view.same_bytes(&views[existing], heap))
                });
                if let Some(index) = found {
                    index + 1
                } else {
                    if dict.len() > MAX_CODES - 1 {
                        return Ok(None);
                    }
                    dict.push(Some(row));
                    dict.len() - 1
                }
            } else {
                if dict[0].is_none() {
                    dict[0] = Some(row);
                }
                0
            };
            composite = composite * MAX_CODES + code;
        }
        let slot = if slot_table[composite] == u16::MAX {
            if slot_rows.len() >= MAX_SLOTS {
                return Ok(None);
            }
            let slot = u16::try_from(slot_rows.len()).expect("bounded slots");
            slot_table[composite] = slot;
            slot_rows.push(row);
            slot
        } else {
            slot_table[composite]
        };
        rows_buffer.push(row);
        codes_buffer.push(slot);
    }

    // Pass 2: per aggregate, one tight loop over (row, slot).
    let code_count = slot_rows.len();
    let mut states: Vec<Vec<AggregateState>> = (0..code_count)
        .map(|_| aggregates.iter().map(AggregateState::new).collect())
        .collect();
    for (aggregate_index, dict_aggregate) in dict_aggregates.iter().enumerate() {
        match dict_aggregate.function {
            AggregateFunction::Count => {
                let mut counts = vec![0_u64; code_count];
                match dict_aggregate.column {
                    None => {
                        for (offset, &code) in codes_buffer.iter().enumerate() {
                            if offset % 1024 == 0 {
                                memory.check_interruption()?;
                            }
                            counts[usize::from(code)] += 1;
                        }
                    }
                    Some(column) => {
                        let validity = batch
                            .column(column)
                            .and_then(ColumnVector::typed)
                            .map(|(_, validity)| validity);
                        for (offset, (&row, &code)) in
                            rows_buffer.iter().zip(&codes_buffer).enumerate()
                        {
                            if offset % 1024 == 0 {
                                memory.check_interruption()?;
                            }
                            let non_null = match validity {
                                Some(validity) => validity.is_valid(row),
                                None => !matches!(
                                    batch.column(column).and_then(|c| c.value(row)),
                                    Some(Value::Null) | None
                                ),
                            };
                            if non_null {
                                counts[usize::from(code)] += 1;
                            }
                        }
                    }
                }
                for (code, count) in counts.iter().enumerate() {
                    states[code][aggregate_index].value = AggregateValue::Count(*count);
                }
            }
            AggregateFunction::Sum | AggregateFunction::Average => {
                let column = dict_aggregate.column.expect("validated column input");
                let (typed, validity) = batch
                    .column(column)
                    .and_then(ColumnVector::typed)
                    .expect("validated typed input");
                let mut sums = vec![0.0_f64; code_count];
                let mut counts = vec![0_u64; code_count];
                for (offset, (&row, &code)) in rows_buffer.iter().zip(&codes_buffer).enumerate() {
                    if offset % 1024 == 0 {
                        memory.check_interruption()?;
                    }
                    if validity.is_valid(row)
                        && let Some(number) = typed.number_at(row)
                    {
                        let slot = usize::from(code);
                        sums[slot] += number;
                        counts[slot] += 1;
                    }
                }
                for code in 0..code_count {
                    if !sums[code].is_finite() {
                        return Err(ExecError::NumericOverflow);
                    }
                    states[code][aggregate_index].value = if dict_aggregate.function
                        == AggregateFunction::Sum
                    {
                        AggregateValue::Sum((counts[code] > 0).then(|| Value::float64(sums[code])))
                    } else {
                        AggregateValue::Average {
                            sum: sums[code],
                            count: counts[code],
                        }
                    };
                }
            }
            _ => unreachable!("filtered above"),
        }
    }

    // Finalize: original values from each slot's representative row,
    // normalized map keys — the general local builder's exact contract.
    let mut groups: HashMap<Vec<Value>, AggregateGroup> = HashMap::with_capacity(code_count * 2);
    for (slot, &row) in slot_rows.iter().enumerate() {
        let mut values = Vec::with_capacity(key_columns.len());
        for (vector, _, _) in &key_columns {
            values.push(
                vector
                    .value(row)
                    .cloned()
                    .ok_or(ExecError::InvalidBatch("dictionary key row out of range"))?,
            );
        }
        let key = values
            .iter()
            .cloned()
            .zip(key_collations)
            .map(|(value, collation)| normalized_collation_value(value, *collation))
            .collect();
        let group = AggregateGroup {
            values,
            states: std::mem::take(&mut states[slot]),
        };
        match groups.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(group);
            }
            Entry::Occupied(mut entry) => {
                for ((state, other), aggregate) in entry
                    .get_mut()
                    .states
                    .iter_mut()
                    .zip(group.states)
                    .zip(aggregates)
                {
                    state.merge(aggregate, other, &memory)?;
                }
            }
        }
    }
    Ok(Some(groups))
}

fn build_local_direct_groups(
    batch: &RecordBatch,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    parent_memory: &MemoryTracker,
    key_collations: &[Collation],
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    if let Some(groups) = build_local_dictionary_groups(
        batch,
        group_columns,
        aggregates,
        parent_memory,
        key_collations,
    )? {
        return Ok(groups);
    }
    let mut groups = Vec::<AggregateGroup>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let memory = parent_memory.unbounded_worker();
    let batch_bytes = batch.estimated_bytes();
    for (offset, row) in batch.selection().selected_rows().enumerate() {
        if offset % 1024 == 0 {
            memory.check_interruption()?;
        }
        let raw_hash = direct_group_hash(batch, row, group_columns)?;
        let existing = raw_index
            .get(&raw_hash)
            .copied()
            .filter(|index| {
                direct_group_matches_exact(&groups[*index].values, batch, row, group_columns)
            })
            .or_else(|| {
                groups.iter().position(|group| {
                    direct_group_matches(&group.values, batch, row, group_columns, key_collations)
                })
            });
        let group_index = existing.unwrap_or_else(|| {
            let values = group_columns
                .iter()
                .map(|column| {
                    direct_group_value(batch, row, *column)
                        .expect("validated direct grouping column")
                        .clone()
                })
                .collect();
            let index = groups.len();
            groups.push(AggregateGroup {
                values,
                states: aggregates.iter().map(AggregateState::new).collect(),
            });
            index
        });
        raw_index.entry(raw_hash).or_insert(group_index);
        update_aggregate_states(
            batch,
            row,
            batch_bytes,
            aggregates,
            &mut groups[group_index].states,
            &memory,
        )?;
    }
    // Groups whose keys NORMALIZE equal must MERGE, not last-write-win: the
    // ASCII fast path above can leave two local groups whose keys the
    // collation folds together, and a plain collect() dropped every earlier
    // group's states (#258, the same defect the fused finalize had).
    let mut folded: HashMap<Vec<Value>, AggregateGroup> = HashMap::with_capacity(groups.len());
    for group in groups {
        let key: Vec<Value> = group
            .values
            .iter()
            .cloned()
            .zip(key_collations)
            .map(|(value, collation)| normalized_collation_value(value, *collation))
            .collect();
        match folded.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(group);
            }
            Entry::Occupied(mut entry) => {
                for ((state, partial_state), aggregate) in entry
                    .get_mut()
                    .states
                    .iter_mut()
                    .zip(group.states)
                    .zip(aggregates)
                {
                    state.merge(aggregate, partial_state, &memory)?;
                }
            }
        }
    }
    Ok(folded)
}

fn build_local_expression_groups(
    batch: &RecordBatch,
    group_by: &[CompiledExpr],
    aggregates: &[CompiledAggregate],
    parent_memory: &MemoryTracker,
    key_collations: &[Collation],
) -> Result<HashMap<Vec<Value>, AggregateGroup>, ExecError> {
    let mut groups = HashMap::<Vec<Value>, AggregateGroup>::new();
    let memory = parent_memory.unbounded_worker();
    let batch_bytes = batch.estimated_bytes();
    for (offset, row) in batch.selection().selected_rows().enumerate() {
        if offset % 1024 == 0 {
            memory.check_interruption()?;
        }
        let values = group_by
            .iter()
            .map(|expression| expression.evaluate(batch, row))
            .collect::<Result<Vec<_>, _>>()?;
        let key = values
            .iter()
            .cloned()
            .zip(key_collations)
            .map(|(value, collation)| normalized_collation_value(value, *collation))
            .collect::<Vec<_>>();
        let group = groups.entry(key).or_insert_with(|| AggregateGroup {
            values,
            states: aggregates.iter().map(AggregateState::new).collect(),
        });
        update_aggregate_states(
            batch,
            row,
            batch_bytes,
            aggregates,
            &mut group.states,
            &memory,
        )?;
    }
    Ok(groups)
}

fn finish_aggregate_groups(
    groups: impl ExactSizeIterator<Item = AggregateGroup>,
    memory: &MemoryTracker,
) -> Result<MaterializedRows, ExecError> {
    memory.reserve(groups.len().saturating_mul(size_of::<Vec<Value>>()))?;
    let mut rows = Vec::with_capacity(groups.len());
    for group in groups {
        let mut row = group.values;
        reserve_vec_elements(&mut row, group.states.len(), 0, memory)?;
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn build_direct_column_aggregate(
    input: &mut PullOperator,
    mut first_batch: Option<RecordBatch>,
    group_columns: &[usize],
    aggregates: &[CompiledAggregate],
    memory: &MemoryTracker,
    collation: Collation,
    key_collations: &[Collation],
) -> Result<MaterializedRows, ExecError> {
    // Single-int-column inputs with eligible lanes take the streaming
    // two-pass partitioned path (e13: 4.2-8.9x); ineligible aggregate
    // shapes fall through to the sequential scalar-index loop below.
    let mut pending = std::collections::VecDeque::new();
    if let Some(batch) = first_batch.take() {
        pending.push_back(batch);
    }
    if matches!(*group_columns, [_] | [_, _]) {
        let head = match pending.pop_front() {
            Some(batch) => Some(batch),
            None => input.next_batch(memory)?,
        };
        let Some(head) = head else {
            return Ok(MaterializedRows {
                rows: Vec::new(),
                position: 0,
            });
        };
        let typed_text = |column: usize| {
            head.column(column).map(ColumnVector::data_type) == Some(DataType::Utf8)
                && head
                    .column(column)
                    .and_then(ColumnVector::typed)
                    .is_some_and(|(typed, _)| matches!(typed, crate::batch::TypedValues::Utf8(_)))
        };
        let keys = match *group_columns {
            [column] => {
                let group_type = head.column(column).map(ColumnVector::data_type);
                let int_typed = group_type.is_some_and(|data_type| {
                    matches!(
                        data_type.storage_type(),
                        DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
                    )
                });
                if int_typed {
                    group_type.map(|group_type| TwoPassKeySource::Int { column, group_type })
                } else if typed_text(column) {
                    Some(TwoPassKeySource::Text { column })
                } else {
                    None
                }
            }
            [first, second]
                if typed_text(first)
                    && typed_text(second)
                    && key_collations.windows(2).all(|pair| pair[0] == pair[1]) =>
            {
                Some(TwoPassKeySource::TextPair { first, second })
            }
            _ => None,
        };
        let lanes = keys.and_then(|_| two_pass_lanes(aggregates, &head));
        if std::env::var_os("PINTAIL_AGG_DEBUG").is_some() {
            let kinds = lanes.as_ref().map(|lanes| {
                lanes
                    .iter()
                    .map(|lane| match lane {
                        TwoPassLane::CountStar => "count*",
                        TwoPassLane::Float { .. } => "float",
                        TwoPassLane::Int { .. } => "int",
                        TwoPassLane::Exact { .. } => "exact",
                        TwoPassLane::DecimalUnits { .. } => "decimal-units",
                        TwoPassLane::Distinct { .. } => "distinct",
                        TwoPassLane::ExtremeDecimal { .. } => "extreme-decimal",
                    })
                    .collect::<Vec<_>>()
            });
            eprintln!("[agg] direct path: keys={} lanes={kinds:?}", keys.is_some());
        }
        if let (Some(keys), Some(lanes)) = (keys, lanes) {
            // Streaming scatter (phase-0 profile, 2026-08-02): retaining
            // RecordBatches cost ~118 bytes/row and forced the sequential
            // Value-hashmap fallback on real 20M-row inputs. Scattering
            // (key bits, lane bits) as batches arrive costs the exact
            // 8*(1+lanes)+1 bytes/row and never falls back.
            // Text keys intern under the KEY's collation; the plan fallback
            // only reaches the intern when the key carries no text.
            let intern_collation = key_collations.first().copied().unwrap_or(collation);
            return build_streaming_two_pass_aggregate(
                input,
                head,
                keys,
                &lanes,
                aggregates,
                memory,
                intern_collation,
            );
        }
        pending.push_front(head);
    }
    let mut groups = Vec::<AggregateGroup>::new();
    let mut scalar_index = HashMap::<Value, usize>::new();
    let mut raw_index = HashMap::<u64, usize>::new();
    let mut index_reserved = 0_usize;

    loop {
        let batch = if let Some(batch) = pending.pop_front() {
            batch
        } else if let Some(batch) = input.next_batch(memory)? {
            batch
        } else {
            break;
        };
        let batch_bytes = batch.estimated_bytes();
        let indexed = group_columns.len() == 1
            && batch.column(group_columns[0]).is_some_and(|column| {
                matches!(
                    column.data_type().storage_type(),
                    DataType::Boolean | DataType::Int64 | DataType::UInt64 | DataType::Float64
                )
            });
        for row in batch.selection().selected_rows() {
            let raw_hash = (!indexed)
                .then(|| direct_group_hash(&batch, row, group_columns))
                .transpose()?;
            let existing = if indexed {
                let value = direct_group_value(&batch, row, group_columns[0])?;
                scalar_index.get(value).copied()
            } else {
                raw_index
                    .get(&raw_hash.expect("non-indexed groups have a raw hash"))
                    .copied()
                    .filter(|index| {
                        direct_group_matches_exact(
                            &groups[*index].values,
                            &batch,
                            row,
                            group_columns,
                        )
                    })
                    .or_else(|| {
                        groups.iter().position(|group| {
                            direct_group_matches(
                                &group.values,
                                &batch,
                                row,
                                group_columns,
                                key_collations,
                            )
                        })
                    })
            };
            let group_index = if let Some(index) = existing {
                index
            } else {
                let values = group_columns
                    .iter()
                    .map(|column| direct_group_value(&batch, row, *column).cloned())
                    .collect::<Result<Vec<_>, _>>()?;
                let bytes = estimated_row_payload_bytes(&values)
                    .saturating_add(aggregates.len().saturating_mul(size_of::<AggregateState>()));
                memory.ensure_transient(batch_bytes.saturating_add(bytes))?;
                reserve_vec_elements(&mut groups, 1, 64, memory)?;
                memory.reserve(bytes)?;
                let index = groups.len();
                groups.push(AggregateGroup {
                    values,
                    states: aggregates.iter().map(AggregateState::new).collect(),
                });
                if indexed {
                    index_reserved = index_reserved.saturating_add(reserve_hash_map_entries(
                        &mut scalar_index,
                        1,
                        size_of::<Value>()
                            .saturating_add(size_of::<usize>())
                            .saturating_add(HASH_ENTRY_OVERHEAD),
                        batch_bytes,
                        memory,
                    )?);
                    let key = direct_group_value(&batch, row, group_columns[0])?.clone();
                    memory.reserve(key.heap_bytes())?;
                    index_reserved = index_reserved.saturating_add(key.heap_bytes());
                    scalar_index.insert(key, index);
                }
                index
            };
            if let Some(raw_hash) = raw_hash
                && !raw_index.contains_key(&raw_hash)
            {
                index_reserved = index_reserved.saturating_add(reserve_hash_map_entries(
                    &mut raw_index,
                    1,
                    size_of::<u64>()
                        .saturating_add(size_of::<usize>())
                        .saturating_add(HASH_ENTRY_OVERHEAD),
                    batch_bytes,
                    memory,
                )?);
                raw_index.insert(raw_hash, group_index);
            }
            update_aggregate_states(
                &batch,
                row,
                batch_bytes,
                aggregates,
                &mut groups[group_index].states,
                memory,
            )?;
        }
    }

    drop(scalar_index);
    drop(raw_index);
    memory.release(index_reserved);
    memory.reserve(groups.len().saturating_mul(size_of::<Vec<Value>>()))?;
    let mut rows = Vec::with_capacity(groups.len());
    for group in groups {
        let mut row = group.values;
        reserve_vec_elements(&mut row, group.states.len(), 0, memory)?;
        for state in group.states {
            row.push(state.finish(memory)?);
        }
        memory.reserve(estimated_row_payload_bytes(&row))?;
        rows.push(row);
    }
    Ok(MaterializedRows { rows, position: 0 })
}

pub(super) fn direct_group_value(
    batch: &RecordBatch,
    row: usize,
    column: usize,
) -> Result<&Value, ExecError> {
    batch
        .column(column)
        .and_then(|values| values.value(row))
        .ok_or(ExecError::InvalidBatch(
            "grouping column is outside the input batch",
        ))
}

pub(super) fn direct_group_matches(
    values: &[Value],
    batch: &RecordBatch,
    row: usize,
    columns: &[usize],
    key_collations: &[Collation],
) -> bool {
    values
        .iter()
        .zip(columns)
        .zip(key_collations)
        .all(|((grouped, column), collation)| {
            direct_group_value(batch, row, *column).is_ok_and(|candidate| {
                match (grouped, candidate) {
                    (Value::Utf8(left), Value::Utf8(right)) => {
                        // The ASCII fast path may only answer EQUAL: it
                        // ignores PAD SPACE, so 'red' vs 'red ' must fall
                        // through to the collation, which pads (#258).
                        (left.is_ascii() && right.is_ascii() && left.eq_ignore_ascii_case(right))
                            || compare_utf8_mysql(left, right, *collation) == Ordering::Equal
                    }
                    _ => grouped == candidate,
                }
            })
        })
}

pub(super) fn direct_group_matches_exact(
    values: &[Value],
    batch: &RecordBatch,
    row: usize,
    columns: &[usize],
) -> bool {
    values.iter().zip(columns).all(|(grouped, column)| {
        direct_group_value(batch, row, *column).is_ok_and(|candidate| grouped == candidate)
    })
}

pub(super) fn direct_group_hash(
    batch: &RecordBatch,
    row: usize,
    columns: &[usize],
) -> Result<u64, ExecError> {
    // The result only routes rows into LOCAL per-batch groups; cross-batch
    // merging keys on normalized values, so per-column path choice (typed vs
    // Value hashing) just needs to be consistent within one batch — and it
    // is, because a column's typed projection is a per-batch constant.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for column in columns {
        let typed_hash = batch
            .column(*column)
            .and_then(ColumnVector::typed)
            .and_then(|(typed, validity)| typed.group_hash_at(row, validity));
        let column_hash = if let Some(column_hash) = typed_hash {
            column_hash
        } else {
            let mut hasher = DefaultHasher::new();
            direct_group_value(batch, row, *column)?.hash(&mut hasher);
            hasher.finish()
        };
        hash = crate::batch::mix64(hash ^ column_hash);
    }
    Ok(hash)
}

/// Typed-first aggregate update from a batch column: packed units and raw
/// integers route straight into the state with no Value cell and no lazy
/// text. Returns whether the row was handled (NULL rows count as handled —
/// they join no aggregate).
pub(super) fn update_state_from_typed_column(
    state: &mut AggregateState,
    aggregate: &CompiledAggregate,
    batch: &RecordBatch,
    column: usize,
    row: usize,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    let Some((typed, validity)) = batch.column(column).and_then(super::ColumnVector::typed) else {
        return Ok(false);
    };
    if !validity.is_valid(row) {
        // JSON_ARRAYAGG is the one aggregate that collects NULL inputs;
        // its generic update encodes them as JSON nulls.
        return Ok(aggregate.function != AggregateFunction::JsonArrayAgg);
    }
    if aggregate.distinct {
        // COUNT(DISTINCT int_col): dedup on the raw integer, no Value (e16).
        if matches!(aggregate.function, AggregateFunction::Count)
            && let Some(key) = typed.int_key_at(row)
        {
            state.update_distinct_count_int(key, memory)?;
            return Ok(true);
        }
        return Ok(false);
    }
    match aggregate.function {
        // No typed fast path yet; `false` sends the row through the generic
        // Value update, which handles them correctly.
        AggregateFunction::AnyValue
        | AggregateFunction::StdDev { .. }
        | AggregateFunction::Variance { .. }
        | AggregateFunction::BitAnd
        | AggregateFunction::BitOr
        | AggregateFunction::BitXor
        | AggregateFunction::JsonObjectAgg => return Ok(false),
        AggregateFunction::Count => {
            state.update_with_number(aggregate, &Value::Boolean(true), None, memory)?;
            return Ok(true);
        }
        AggregateFunction::Sum => {
            if let (Some(units), Some(scale)) = (typed.units_at(row), typed.decimal_scale()) {
                state.update_decimal_sum_units(units, scale, aggregate_uses_float(aggregate))?;
                return Ok(true);
            }
            if aggregate_uses_float(aggregate)
                && let Some(number) = typed.number_at(row)
            {
                state.update_with_number(aggregate, &Value::Boolean(true), Some(number), memory)?;
                return Ok(true);
            }
        }
        AggregateFunction::Average => {
            if let Some(result_scale) = decimal_average_scale(aggregate) {
                // Exact decimal AVG: take packed units when the column has
                // them; integer numbers stay exact through the f64 hint;
                // anything else falls back to the real-value update.
                if let (Some(units), Some(scale)) = (typed.units_at(row), typed.decimal_scale())
                    && scale <= result_scale
                    && let Some(rescaled) = decimal_units_from_int(units, result_scale - scale)
                {
                    state.update_decimal_average_units(rescaled, result_scale)?;
                    return Ok(true);
                }
                if let Some(number) = typed.number_at(row)
                    && number.fract() == 0.0
                {
                    state.update_with_number(
                        aggregate,
                        &Value::Boolean(true),
                        Some(number),
                        memory,
                    )?;
                    return Ok(true);
                }
                return Ok(false);
            }
            if let Some(number) = typed.number_at(row) {
                state.update_with_number(aggregate, &Value::Boolean(true), Some(number), memory)?;
                return Ok(true);
            }
        }
        AggregateFunction::Minimum | AggregateFunction::Maximum => {
            if let Some(units) = typed.units_at(row) {
                state.update_extreme_units(aggregate, units, || typed.format_unit(row), memory)?;
                return Ok(true);
            }
        }
        AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg => {}
    }
    Ok(false)
}

#[allow(clippy::too_many_lines)]
pub(super) fn update_aggregate_states(
    batch: &RecordBatch,
    row: usize,
    batch_bytes: usize,
    aggregates: &[CompiledAggregate],
    states: &mut [AggregateState],
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let mut numeric_cache = [None::<(usize, f64)>; 8];
    let mut numeric_cache_len = 0_usize;
    for (aggregate, state) in aggregates.iter().zip(states) {
        let direct_scalar = aggregate
            .expr
            .as_ref()
            .is_none_or(|expression| expression.column_index().is_some())
            && !matches!(
                aggregate.function,
                AggregateFunction::GroupConcat | AggregateFunction::JsonArrayAgg
            );
        if !direct_scalar {
            let update_memory = aggregate
                .expr
                .as_ref()
                .map_or(0, |expression| {
                    expression
                        .allocation_upper_bound(batch, row)
                        .saturating_mul(13)
                })
                .saturating_add(
                    64_usize
                        .saturating_mul(size_of::<String>())
                        .saturating_add(256),
                );
            memory.ensure_transient(batch_bytes.saturating_add(update_memory))?;
        }
        // GROUP_CONCAT with an aggregate-local ORDER BY evaluates its key
        // expressions alongside the argument so finish can sort.
        if aggregate.function == AggregateFunction::GroupConcat
            && !aggregate.order_within.is_empty()
        {
            let Some(expression) = &aggregate.expr else {
                return Err(ExecError::InvalidPhysicalPlan(
                    "group-concat requires an argument expression",
                ));
            };
            let value = expression.evaluate(batch, row)?;
            let keys = aggregate
                .order_within
                .iter()
                .map(|(key, _, _)| key.evaluate(batch, row))
                .collect::<Result<Vec<_>, _>>()?;
            state.update_group_concat(&value, keys, memory)?;
            continue;
        }
        match &aggregate.expr {
            None => state.update(aggregate, &Value::Boolean(true), memory)?,
            Some(expression) => {
                if let Some(column) = expression.column_index() {
                    // Typed-first: numeric aggregation over packed units
                    // never touches the column's Value cells or lazy text
                    // (2026-08-02 phase-0 profile: whole-column text
                    // forcing dominated the string-keyed paths).
                    if update_state_from_typed_column(state, aggregate, batch, column, row, memory)?
                    {
                        continue;
                    }
                    let value = direct_group_value(batch, row, column)?;
                    let is_extreme = matches!(
                        aggregate.function,
                        AggregateFunction::Minimum | AggregateFunction::Maximum
                    );
                    // Min/Max take a typed number when available to guide
                    // comparisons but never pay a text parse for it — the
                    // fallback parse is reserved for float-accumulating
                    // aggregates that need the number unconditionally.
                    if is_extreme && !matches!(value, Value::Null) {
                        let number = batch
                            .column(column)
                            .and_then(super::ColumnVector::typed)
                            .and_then(|(typed, _)| typed.number_at(row));
                        state.update_with_number(aggregate, value, number, memory)?;
                        continue;
                    }
                    let number = if aggregate_uses_float(aggregate) && !matches!(value, Value::Null)
                    {
                        if let Some((_, number)) = numeric_cache[..numeric_cache_len]
                            .iter()
                            .filter_map(Option::as_ref)
                            .find(|(cached_column, _)| *cached_column == column)
                        {
                            Some(*number)
                        } else {
                            // Packed projections resolve without per-row text
                            // parsing; mysql_f64 remains the fallback carrier
                            // path (docs/decisions.md, native decimal ADR).
                            let number = batch
                                .column(column)
                                .and_then(super::ColumnVector::typed)
                                .and_then(|(typed, _)| typed.number_at(row))
                                .map_or_else(|| mysql_f64(value), Ok)?;
                            if numeric_cache_len < numeric_cache.len() {
                                numeric_cache[numeric_cache_len] = Some((column, number));
                                numeric_cache_len += 1;
                            }
                            Some(number)
                        }
                    } else {
                        None
                    };
                    state.update_with_number(aggregate, value, number, memory)?;
                } else {
                    let value = expression.evaluate(batch, row)?;
                    state.update(aggregate, &value, memory)?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn aggregate_uses_float(aggregate: &CompiledAggregate) -> bool {
    (aggregate.function == AggregateFunction::Average && decimal_average_scale(aggregate).is_none())
        || (aggregate.function == AggregateFunction::Sum
            && aggregate.data_type == Some(DataType::Float64))
}

/// The result scale of an exact decimal AVG, when the binder typed this
/// aggregate as one. `None` keeps the f64 average path.
pub(super) fn decimal_average_scale(aggregate: &CompiledAggregate) -> Option<u8> {
    if aggregate.function != AggregateFunction::Average {
        return None;
    }
    match aggregate.data_type {
        Some(DataType::Decimal { scale, .. }) => Some(scale),
        _ => None,
    }
}

/// Exact scaled units from a typed-lane f64 (integers below 2^53 convert
/// losslessly); `None` refuses anything that cannot be exact.
pub(super) fn exact_decimal_units_from_f64(number: f64, scale: u8) -> Option<i128> {
    if number.fract() != 0.0 || number.abs() >= 9_007_199_254_740_992.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    decimal_units_from_int(number as i128, scale)
}

pub(super) fn decimal_units_from_int(value: i128, scale: u8) -> Option<i128> {
    value.checked_mul(10_i128.checked_pow(u32::from(scale))?)
}

pub(super) fn add_aggregate_value(
    current: Option<Value>,
    value: &Value,
    data_type: Option<DataType>,
) -> Result<Value, ExecError> {
    match data_type {
        Some(DataType::UInt64) => {
            let left = current.map_or(Ok(0), |value| mysql_u64(&value))?;
            left.checked_add(mysql_u64(value)?)
                .map(Value::UInt64)
                .ok_or(ExecError::NumericOverflow)
        }
        Some(DataType::Int64) => {
            let left = current.map_or(Ok(0), |value| mysql_i64(&value))?;
            left.checked_add(mysql_i64(value)?)
                .map(Value::Int64)
                .ok_or(ExecError::NumericOverflow)
        }
        Some(DataType::Float64) => {
            let result = current.map_or(Ok(0.0), |value| mysql_f64(&value))? + mysql_f64(value)?;
            if result.is_finite() {
                Ok(Value::float64(result))
            } else {
                Err(ExecError::NumericOverflow)
            }
        }
        _ => Err(ExecError::InvalidExpressionType),
    }
}

pub(super) fn compare_aggregate_values(
    left: &Value,
    right: &Value,
    data_type: Option<DataType>,
    collation: Collation,
) -> Result<Ordering, ExecError> {
    if matches!(data_type, Some(DataType::Decimal { .. })) {
        let (Value::Utf8(left), Value::Utf8(right)) = (left, right) else {
            return Err(ExecError::InvalidExpressionType);
        };
        return compare_decimal_text(left, right);
    }
    compare_mysql(left, right, collation)
}

pub(crate) fn compare_decimal_text(left: &str, right: &str) -> Result<Ordering, ExecError> {
    let (left_negative, left_integer, left_fraction) = decimal_parts(left)?;
    let (right_negative, right_integer, right_fraction) = decimal_parts(right)?;
    if left_negative != right_negative {
        return Ok(if left_negative {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let magnitude = left_integer
        .len()
        .cmp(&right_integer.len())
        .then_with(|| left_integer.cmp(right_integer))
        .then_with(|| {
            let digits = left_fraction.len().max(right_fraction.len());
            (0..digits)
                .map(|index| {
                    left_fraction
                        .as_bytes()
                        .get(index)
                        .copied()
                        .unwrap_or(b'0')
                        .cmp(
                            &right_fraction
                                .as_bytes()
                                .get(index)
                                .copied()
                                .unwrap_or(b'0'),
                        )
                })
                .find(|ordering| *ordering != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        });
    Ok(if left_negative {
        magnitude.reverse()
    } else {
        magnitude
    })
}

fn decimal_parts(value: &str) -> Result<(bool, &str, &str), ExecError> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if integer.is_empty()
        || !integer.bytes().all(|digit| digit.is_ascii_digit())
        || !fraction.bytes().all(|digit| digit.is_ascii_digit())
    {
        return Err(ExecError::InvalidExpressionType);
    }
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let zero = integer == "0" && fraction.bytes().all(|digit| digit == b'0');
    Ok((negative && !zero, integer, fraction))
}

pub(super) fn aggregate_string(value: &Value) -> Result<String, ExecError> {
    match value {
        Value::Null => Err(ExecError::InvalidExpressionType),
        Value::Boolean(value) => Ok(if *value { "1" } else { "0" }.to_owned()),
        Value::Int64(value) => Ok(value.to_string()),
        Value::UInt64(value) => Ok(value.to_string()),
        Value::Float64(value) => Ok(value.get().to_string()),
        Value::Utf8(value) | Value::Enum { label: value, .. } => Ok(value.clone()),
        Value::Binary(value) => {
            String::from_utf8(value.clone()).map_err(|_| ExecError::InvalidUtf8Number)
        }
    }
}
