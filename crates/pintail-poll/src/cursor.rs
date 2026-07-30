use mysql_async::Value as MysqlValue;
use serde::{Deserialize, Serialize};

use crate::PollError;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub(crate) enum CursorValue {
    Null,
    Bytes(Vec<u8>),
    Int(i64),
    UInt(u64),
    Float(u32),
    Double(u64),
    Date((u16, u8, u8, u8, u8, u8, u32)),
    Time((bool, u32, u8, u8, u8, u32)),
}

impl From<MysqlValue> for CursorValue {
    fn from(value: MysqlValue) -> Self {
        match value {
            MysqlValue::NULL => Self::Null,
            MysqlValue::Bytes(value) => Self::Bytes(value),
            MysqlValue::Int(value) => Self::Int(value),
            MysqlValue::UInt(value) => Self::UInt(value),
            MysqlValue::Float(value) => Self::Float(value.to_bits()),
            MysqlValue::Double(value) => Self::Double(value.to_bits()),
            MysqlValue::Date(year, month, day, hour, minute, second, micros) => {
                Self::Date((year, month, day, hour, minute, second, micros))
            }
            MysqlValue::Time(negative, days, hours, minutes, seconds, micros) => {
                Self::Time((negative, days, hours, minutes, seconds, micros))
            }
        }
    }
}

impl CursorValue {
    pub(crate) fn into_mysql(self) -> MysqlValue {
        match self {
            Self::Null => MysqlValue::NULL,
            Self::Bytes(value) => MysqlValue::Bytes(value),
            Self::Int(value) => MysqlValue::Int(value),
            Self::UInt(value) => MysqlValue::UInt(value),
            Self::Float(bits) => MysqlValue::Float(f32::from_bits(bits)),
            Self::Double(bits) => MysqlValue::Double(f64::from_bits(bits)),
            Self::Date((year, month, day, hour, minute, second, micros)) => {
                MysqlValue::Date(year, month, day, hour, minute, second, micros)
            }
            Self::Time((negative, days, hours, minutes, seconds, micros)) => {
                MysqlValue::Time(negative, days, hours, minutes, seconds, micros)
            }
        }
    }

    pub(crate) fn encode(&self) -> Result<String, PollError> {
        serde_json::to_string(self).map_err(PollError::Json)
    }

    pub(crate) fn decode(value: &str) -> Result<Self, PollError> {
        serde_json::from_str(value).map_err(PollError::Json)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProbeToken {
    pub(crate) count: u64,
    pub(crate) maximum: CursorValue,
}

impl ProbeToken {
    pub(crate) fn encode(&self) -> Result<String, PollError> {
        serde_json::to_string(self).map_err(PollError::Json)
    }
}
