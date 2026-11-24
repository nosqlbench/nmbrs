// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Strict scalar conversion for prepared-statement bind values.
//!
//! The one place either engine turns a cycle-time [`Value`] into a
//! CQL integer, float or boolean. Every conversion either yields the
//! exact value or an error naming the value and the column type — a
//! value that doesn't fit is never truncated, wrapped or replaced by
//! zero / `false`.
//!
//! - **Integers** (`tinyint`, `smallint`, `int`, `bigint`, `counter`):
//!   `I64`; `U64` read as the `i64` it stores (polydat's bit-stuffed
//!   convention, as in [`Value::as_i64`]); an integral, in-range
//!   `F64`; or a `Str` that parses as a base-10 integer. The result
//!   must fit the column's width.
//! - **Floats** (`float`, `double`): `F64`, `U64`, `I64`, or a `Str`
//!   that parses as a float. A finite value too large for `float` is
//!   an error, not infinity.
//! - **Booleans**: `Bool`, or the `Str` `true` / `false`. Numbers are
//!   not booleans.

use polydat::ast::Value;

/// Convert `value` for an integer column of width `T` (`i8`, `i16`,
/// `i32`, `i64`). `cql_type` names the column type in errors.
pub fn bind_int<T: TryFrom<i64>>(value: &Value, cql_type: &str) -> Result<T, String> {
    let wide: i64 = match value {
        Value::I64(v) => *v,
        Value::U64(v) => *v as i64,
        Value::F64(v) => {
            if !v.is_finite() || v.fract() != 0.0 || *v < i64::MIN as f64 || *v >= i64::MAX as f64 {
                return Err(format!(
                    "F64 {v} is not an integer; a CQL {cql_type} column takes whole numbers"
                ));
            }
            *v as i64
        }
        Value::Str(s) => s.parse::<i64>().map_err(|_| {
            format!("'{s}' is not an integer; a CQL {cql_type} column takes whole numbers")
        })?,
        other => return Err(mismatch(other, cql_type)),
    };
    T::try_from(wide).map_err(|_| format!("{wide} is out of range for a CQL {cql_type} column"))
}

/// Convert `value` for a CQL `double` column.
pub fn bind_f64(value: &Value) -> Result<f64, String> {
    match value {
        Value::F64(v) => Ok(*v),
        Value::U64(v) => Ok(*v as f64),
        Value::I64(v) => Ok(*v as f64),
        Value::Str(s) => s
            .parse::<f64>()
            .map_err(|_| format!("'{s}' is not a number; a CQL double column takes numbers")),
        other => Err(mismatch(other, "double")),
    }
}

/// Convert `value` for a CQL `float` column.
pub fn bind_f32(value: &Value) -> Result<f32, String> {
    let wide = match value {
        Value::Str(s) => s
            .parse::<f64>()
            .map_err(|_| format!("'{s}' is not a number; a CQL float column takes numbers"))?,
        Value::F64(_) | Value::U64(_) | Value::I64(_) => bind_f64(value)?,
        other => return Err(mismatch(other, "float")),
    };
    let narrow = wide as f32;
    if wide.is_finite() && !narrow.is_finite() {
        return Err(format!("{wide} is out of range for a CQL float column"));
    }
    Ok(narrow)
}

/// Convert `value` for a CQL `boolean` column.
pub fn bind_bool(value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(v) => Ok(*v),
        Value::Str(s) => match &**s {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(format!(
                "'{s}' is not a boolean; a CQL boolean column takes true or false"
            )),
        },
        other => Err(mismatch(other, "boolean")),
    }
}

fn mismatch(value: &Value, cql_type: &str) -> String {
    format!(
        "cannot bind {} {} to a CQL {cql_type} column",
        value.type_name(),
        value.to_display_string()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    #[test]
    fn integers_bind_exactly_or_fail() {
        assert_eq!(bind_int::<i32>(&Value::U64(7), "int"), Ok(7));
        assert_eq!(bind_int::<i32>(&Value::I64(-7), "int"), Ok(-7));
        assert_eq!(bind_int::<i32>(&Value::F64(3.0), "int"), Ok(3));
        assert_eq!(bind_int::<i32>(&s("-42"), "int"), Ok(-42));
        // bigint reads a bit-stuffed U64 as the i64 it stores.
        assert_eq!(bind_int::<i64>(&Value::U64(u64::MAX), "bigint"), Ok(-1));

        for (bad, why) in [
            (s("12abc"), "not an integer"),
            (s(""), "not an integer"),
            (s(" 5"), "not an integer"),
            (Value::F64(3.7), "not an integer"),
            (Value::F64(f64::NAN), "not an integer"),
            (Value::U64(1 << 40), "out of range"),
            (Value::Bool(true), "cannot bind"),
        ] {
            let err = bind_int::<i32>(&bad, "int").expect_err("must reject");
            assert!(err.contains(why) && err.contains("int"), "{bad:?}: {err}");
        }
        let err = bind_int::<i8>(&Value::U64(300), "tinyint").expect_err("must reject");
        assert!(err.contains("out of range for a CQL tinyint"), "{err}");
    }

    #[test]
    fn floats_bind_exactly_or_fail() {
        assert_eq!(bind_f64(&Value::F64(0.5)), Ok(0.5));
        assert_eq!(bind_f64(&Value::U64(2)), Ok(2.0));
        assert_eq!(bind_f64(&s("1e3")), Ok(1000.0));
        assert_eq!(bind_f32(&s("0.25")), Ok(0.25));
        assert!(bind_f64(&s("abc")).unwrap_err().contains("not a number"));
        assert!(bind_f32(&s("")).unwrap_err().contains("not a number"));
        assert!(
            bind_f32(&Value::F64(1e300))
                .unwrap_err()
                .contains("out of range")
        );
        assert!(
            bind_f64(&Value::Bool(false))
                .unwrap_err()
                .contains("cannot bind")
        );
    }

    #[test]
    fn booleans_are_true_or_false_only() {
        assert_eq!(bind_bool(&Value::Bool(true)), Ok(true));
        assert_eq!(bind_bool(&s("false")), Ok(false));
        for bad in [s("1"), s("TRUE"), s("yes"), Value::U64(1), Value::U64(0)] {
            assert!(
                bind_bool(&bad).is_err(),
                "{bad:?} must not bind as a boolean"
            );
        }
    }
}
